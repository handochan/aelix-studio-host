use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use agent_client_protocol::schema::v1::{AvailableCommand, SessionId};
use tokio::sync::broadcast;

use super::events::{AgentStreamEvent, AvailableCommandsEventData};

struct SessionCommands {
    id: SessionId,
    commands: Option<Vec<AvailableCommand>>,
}

#[derive(Default)]
enum OpenPhase {
    #[default]
    Inactive,
    Opening {
        expected_id: Option<SessionId>,
        ads: HashMap<SessionId, Vec<AvailableCommand>>,
    },
    Reconciling(SessionCommands),
    Ready(SessionCommands),
}

#[derive(Default)]
struct CatalogState {
    generation: u64,
    retired: HashSet<SessionId>,
    phase: OpenPhase,
}

impl CatalogState {
    /// An ACP notification identifies the session, but carries no open
    /// generation. A reused ID on this connection is therefore ambiguous.
    fn retire(&mut self) -> bool {
        match std::mem::take(&mut self.phase) {
            OpenPhase::Inactive => return false,
            OpenPhase::Opening { expected_id, ads } => {
                self.retired.extend(expected_id);
                self.retired.extend(ads.into_keys());
            }
            OpenPhase::Reconciling(session) | OpenPhase::Ready(session) => {
                self.retired.insert(session.id);
            }
        }
        true
    }
}

/// The command list is scoped to a successfully prepared session on this connection.
pub(super) struct CommandCatalog {
    state: Mutex<CatalogState>,
    events: broadcast::Sender<AgentStreamEvent>,
}

impl CommandCatalog {
    pub(super) fn new(events: broadcast::Sender<AgentStreamEvent>) -> Self {
        Self {
            state: Mutex::default(),
            events,
        }
    }

    /// The manager holds its session lock across setup, including any recovery.
    pub(super) fn setup(&self) -> CatalogSetup<'_> {
        CatalogSetup {
            catalog: self,
            confirmed: false,
        }
    }

    pub(super) fn begin_attempt(&self, expected_id: Option<SessionId>) -> u64 {
        let mut state = self.state.lock().unwrap();
        if state.retire() {
            // A replacement must clear the failed attempt's metadata even
            // when the next session never advertises.
            self.publish(AvailableCommandsEventData::invalidated());
        }
        state.generation = state.generation.wrapping_add(1);
        state.phase = OpenPhase::Opening {
            expected_id,
            ads: HashMap::new(),
        };
        state.generation
    }

    pub(super) fn identify(&self, generation: u64, id: SessionId) {
        let mut state = self.state.lock().unwrap();
        if state.generation != generation {
            return;
        }
        if let OpenPhase::Opening { ads, .. } = &mut state.phase {
            let commands = ads.remove(&id);
            state.phase = OpenPhase::Reconciling(SessionCommands { id, commands });
        }
    }

    pub(super) fn advertise(&self, id: &SessionId, advertised: &[AvailableCommand]) {
        let mut state = self.state.lock().unwrap();
        if state.retired.contains(id) {
            return;
        }
        match &mut state.phase {
            OpenPhase::Opening { expected_id, ads } if expected_id.as_ref().is_none_or(|known| known == id) => {
                ads.insert(id.clone(), advertised.to_vec());
            }
            OpenPhase::Reconciling(session) if &session.id == id => {
                session.commands = Some(advertised.to_vec());
            }
            OpenPhase::Ready(session) if &session.id == id => {
                session.commands = Some(advertised.to_vec());
                self.publish(AvailableCommandsEventData::new(advertised.to_vec()));
            }
            _ => {}
        }
    }

    pub(super) fn with_commands<T>(&self, id: &str, read: impl FnOnce(&[AvailableCommand]) -> T) -> Option<T> {
        let state = self.state.lock().unwrap();
        match &state.phase {
            OpenPhase::Ready(session) if session.id.0.as_ref() == id => session.commands.as_deref().map(read),
            _ => None,
        }
    }

    fn confirm(&self, id: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if !matches!(&state.phase, OpenPhase::Reconciling(session) if session.id.0.as_ref() == id) {
            return false;
        }
        let OpenPhase::Reconciling(session) = std::mem::take(&mut state.phase) else {
            unreachable!("checked under the state lock");
        };
        if let Some(commands) = &session.commands {
            self.publish(AvailableCommandsEventData::new(commands.clone()));
        }
        state.phase = OpenPhase::Ready(session);
        true
    }

    pub(super) fn discard(&self, known_id: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        if let Some(id) = known_id {
            state.retired.insert(SessionId::new(id));
        }
        if state.retire() {
            self.publish(AvailableCommandsEventData::invalidated());
        }
    }

    // Publish under the state lock so a concurrent advertisement cannot
    // overtake the initial list and then be overwritten by that snapshot.
    fn publish(&self, update: AvailableCommandsEventData) {
        let _ = self.events.send(AgentStreamEvent::AvailableCommands(update));
    }
}

pub(crate) struct CatalogSetup<'a> {
    catalog: &'a CommandCatalog,
    confirmed: bool,
}

impl CatalogSetup<'_> {
    pub(crate) fn finish(mut self, session_id: &str) {
        self.confirmed = self.catalog.confirm(session_id);
    }
}

impl Drop for CatalogSetup<'_> {
    fn drop(&mut self) {
        if !self.confirmed {
            self.catalog.discard(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_open_retires_pending_ids_and_clears_published_metadata() {
        let (events, mut rx) = broadcast::channel(8);
        let catalog = CommandCatalog::new(events);
        let reused = SessionId::new("reused");

        let setup = catalog.setup();
        let generation = catalog.begin_attempt(None);
        catalog.advertise(&reused, &[AvailableCommand::new("abandoned", "Abandoned command")]);
        drop(setup);

        let AgentStreamEvent::AvailableCommands(update) = rx.try_recv().unwrap() else {
            panic!("abandoned opening must clear stale metadata");
        };
        assert!(update.invalidated);
        assert!(update.commands.is_empty());
        assert_eq!(catalog.with_commands("reused", <[_]>::len), None);

        let setup = catalog.setup();
        let next = catalog.begin_attempt(None);
        catalog.identify(generation, SessionId::new("old-response"));
        catalog.advertise(&reused, &[AvailableCommand::new("late", "Late command")]);
        catalog.identify(next, reused);
        setup.finish("reused");
        assert_eq!(catalog.with_commands("reused", <[_]>::len), None);
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }

    #[test]
    fn missing_advertisement_does_not_publish_an_explicit_empty_catalog() {
        let (events, mut rx) = broadcast::channel(8);
        let catalog = CommandCatalog::new(events);
        let active = SessionId::new("legacy");
        let setup = catalog.setup();
        let generation = catalog.begin_attempt(Some(active.clone()));
        catalog.identify(generation, active.clone());
        setup.finish("legacy");

        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert_eq!(catalog.with_commands("legacy", <[_]>::len), None);

        catalog.advertise(&active, &[]);
        let AgentStreamEvent::AvailableCommands(update) = rx.try_recv().unwrap() else {
            panic!("explicit advertisement must publish the active catalog");
        };
        assert!(!update.invalidated);
        assert!(update.commands.is_empty());
        assert_eq!(catalog.with_commands("legacy", <[_]>::len), Some(0));
    }
}
