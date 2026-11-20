use std::collections::HashMap;
use std::sync::Mutex;

use agent_client_protocol::schema::v1::{AvailableCommand, SessionId};
use tokio::sync::broadcast;

use super::events::{AgentStreamEvent, AvailableCommandsEventData};

#[derive(Default)]
enum CatalogState {
    #[default]
    Inactive,
    Opening(HashMap<SessionId, Vec<AvailableCommand>>),
    Active {
        session_id: SessionId,
        commands: Option<Vec<AvailableCommand>>,
    },
}

/// The command list is scoped to the session opened on this connection.
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

    /// The manager serializes session opens. Clear the previous generation
    /// before dispatch, including when the peer will reuse the same ID.
    pub(super) fn begin_open(&self) -> CatalogOpen<'_> {
        *self.state.lock().unwrap() = CatalogState::Opening(HashMap::new());
        CatalogOpen(self)
    }

    pub(super) fn activate(&self, session_id: SessionId) {
        let mut state = self.state.lock().unwrap();
        let commands = match &mut *state {
            CatalogState::Opening(pending) => pending.remove(&session_id),
            _ => None,
        };
        if let Some(commands) = &commands {
            self.publish(commands);
        }
        *state = CatalogState::Active { session_id, commands };
    }

    pub(super) fn advertise(&self, id: &SessionId, advertised: &[AvailableCommand]) {
        let mut state = self.state.lock().unwrap();
        match &mut *state {
            // The peer may advertise before the open response names its ID.
            CatalogState::Opening(pending) => {
                pending.insert(id.clone(), advertised.to_vec());
            }
            CatalogState::Active { session_id, commands } if session_id == id => {
                *commands = Some(advertised.to_vec());
                self.publish(advertised);
            }
            _ => {}
        }
    }

    pub(super) fn with_commands<T>(&self, id: &str, read: impl FnOnce(&[AvailableCommand]) -> T) -> Option<T> {
        let state = self.state.lock().unwrap();
        match &*state {
            CatalogState::Active { session_id, commands } if session_id.0.as_ref() == id => {
                commands.as_deref().map(read)
            }
            _ => None,
        }
    }

    // Publish under the state lock so a concurrent advertisement cannot
    // overtake the initial list and then be overwritten by that snapshot.
    fn publish(&self, commands: &[AvailableCommand]) {
        let _ = self
            .events
            .send(AgentStreamEvent::AvailableCommands(AvailableCommandsEventData {
                commands: commands.to_vec(),
            }));
    }
}

pub(super) struct CatalogOpen<'a>(&'a CommandCatalog);

impl CatalogOpen<'_> {
    pub(super) fn finish(self, session_id: SessionId) {
        self.0.activate(session_id);
    }
}

impl Drop for CatalogOpen<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        if matches!(*state, CatalogState::Opening(_)) {
            *state = CatalogState::Inactive;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abandoned_open_discards_pending_and_previous_catalogs() {
        let (events, _) = broadcast::channel(8);
        let catalog = CommandCatalog::new(events);
        let reused = SessionId::new("reused");
        catalog.activate(reused.clone());
        catalog.advertise(&reused, &[AvailableCommand::new("previous", "Previous command")]);

        let open = catalog.begin_open();
        catalog.advertise(&reused, &[AvailableCommand::new("abandoned", "Abandoned command")]);
        drop(open);

        assert_eq!(catalog.with_commands("reused", <[_]>::len), None);
        catalog.begin_open().finish(reused);
        assert_eq!(catalog.with_commands("reused", <[_]>::len), None);
    }

    #[test]
    fn missing_advertisement_does_not_publish_an_explicit_empty_catalog() {
        let (events, mut rx) = broadcast::channel(8);
        let catalog = CommandCatalog::new(events);
        let active = SessionId::new("legacy");
        catalog.activate(active.clone());

        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert_eq!(catalog.with_commands("legacy", <[_]>::len), None);

        catalog.advertise(&active, &[]);
        let AgentStreamEvent::AvailableCommands(update) = rx.try_recv().unwrap() else {
            panic!("explicit advertisement must publish the active catalog");
        };
        assert!(update.commands.is_empty());
        assert_eq!(catalog.with_commands("legacy", <[_]>::len), Some(0));
    }
}
