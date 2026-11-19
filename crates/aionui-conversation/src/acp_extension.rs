//! Admission and pending budgets for the protected ACP extension endpoint.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_ai_agent::AgentInstance;
use aionui_ai_agent::manager::acp::AcpAgentManager;
use aionui_ai_agent::protocol::acp_relay::{DEADLINE, RelayCall, RelayError, RelayWriteAdmission};
use aionui_api_types::{AcpExtensionDiscovery, AcpExtensionRequest, AcpExtensionResult};
use tokio::time::Instant;

use crate::{ConversationError, ConversationService};

#[derive(Debug)]
pub(crate) enum ExtensionError {
    Conversation(ConversationError),
    Relay(RelayError),
    Authentication,
}

impl From<RelayError> for ExtensionError {
    fn from(error: RelayError) -> Self {
        Self::Relay(error)
    }
}

#[derive(Default)]
pub(crate) struct ExtensionLimits(Mutex<HashMap<String, OwnerBudget>>);

struct OwnerBudget {
    tokens: f64,
    updated: Instant,
    active: usize,
    overdue: usize,
    bindings: HashMap<String, BindingBudget>,
}

#[derive(Default)]
struct BindingBudget {
    active: usize,
    overdue: usize,
}

impl ExtensionLimits {
    fn charge(&self, owner: &str) -> Result<(), RelayError> {
        let mut owners = self.0.lock().unwrap();
        let now = Instant::now();
        // A full and idle bucket has no effects worth retaining.
        owners.retain(|_, b| b.active + b.overdue > 0 || now.duration_since(b.updated).as_secs() < 2);
        let budget = owners.entry(owner.into()).or_insert_with(|| OwnerBudget {
            tokens: 20.0,
            updated: now,
            active: 0,
            overdue: 0,
            bindings: HashMap::new(),
        });
        budget.tokens = (budget.tokens + now.duration_since(budget.updated).as_secs_f64() * 10.0).min(20.0);
        budget.updated = now;
        if budget.tokens < 1.0 {
            return Err(RelayError::RateLimited);
        }
        budget.tokens -= 1.0;
        Ok(())
    }

    fn reserve(self: &Arc<Self>, owner: &str, binding: &str) -> Result<Reservation, RelayError> {
        let mut owners = self.0.lock().unwrap();
        let budget = owners.get_mut(owner).ok_or(RelayError::RateLimited)?;
        if budget.active >= 8 || budget.active + budget.overdue >= 16 || budget.overdue >= 8 {
            return Err(RelayError::RateLimited);
        }
        let binding_budget = budget.bindings.entry(binding.into()).or_default();
        if binding_budget.active >= 4 || binding_budget.overdue >= 3 {
            return Err(RelayError::RateLimited);
        }
        binding_budget.active += 1;
        budget.active += 1;
        Ok(Reservation {
            limits: self.clone(),
            owner: owner.into(),
            binding: binding.into(),
            overdue: false,
        })
    }
}

struct Reservation {
    limits: Arc<ExtensionLimits>,
    owner: String,
    binding: String,
    overdue: bool,
}

impl Reservation {
    fn expire(&mut self) {
        let mut owners = self.limits.0.lock().unwrap();
        let budget = owners.get_mut(&self.owner).unwrap();
        let binding = budget.bindings.get_mut(&self.binding).unwrap();
        budget.active -= 1;
        budget.overdue += 1;
        binding.active -= 1;
        binding.overdue += 1;
        self.overdue = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut owners = self.limits.0.lock().unwrap();
        let budget = owners.get_mut(&self.owner).unwrap();
        let binding = budget.bindings.get_mut(&self.binding).unwrap();
        if self.overdue {
            budget.overdue -= 1;
            binding.overdue -= 1;
        } else {
            budget.active -= 1;
            binding.active -= 1;
        }
        if binding.active + binding.overdue == 0 {
            budget.bindings.remove(&self.binding);
        }
    }
}

impl ConversationService {
    fn with_extension_agent<T>(
        &self,
        owner: &str,
        conversation: &str,
        operation: impl FnOnce(&Arc<AcpAgentManager>) -> Result<T, ExtensionError>,
    ) -> Result<T, ExtensionError> {
        let mut operation = Some(operation);
        let mut result = Err(RelayError::NoLiveTask.into());
        self.task_manager().visit_task(conversation, &mut |agent| {
            result = match agent {
                AgentInstance::Acp(agent) if agent.relay_owned_by(owner) => operation.take().unwrap()(agent),
                _ => Err(RelayError::Unavailable.into()),
            };
        });
        result
    }

    fn relay_write_admission(
        &self,
        owner: &str,
        conversation: &str,
        agent: &Arc<AcpAgentManager>,
        browser: Option<aionui_common::WsSessionGuard>,
        deadline: Instant,
    ) -> RelayWriteAdmission {
        let runtime = self.runtime_state();
        let manager = Arc::downgrade(self.task_manager());
        let conversation = conversation.to_owned();
        let owner = owner.to_owned();
        let agent = Arc::downgrade(agent);
        let repo = self.conversation_repo().clone();
        let checked_owner = owner.clone();
        let checked_conversation = conversation.clone();
        RelayWriteAdmission::new(browser.clone(), deadline, move |write| {
            let (Some(agent), Some(manager)) = (agent.upgrade(), manager.upgrade()) else {
                return;
            };
            runtime.with_relay_admission(&conversation, || {
                manager.visit_task(&conversation, &mut |current| {
                    if let AgentInstance::Acp(current) = current
                        && Arc::ptr_eq(current, &agent)
                        && current.relay_owned_by(&owner)
                    {
                        if let Some(browser) = &browser {
                            browser.while_current(write);
                        } else {
                            write();
                        }
                    }
                });
            });
        })
        .with_owner_recheck(move || {
            let repo = repo.clone();
            let owner = checked_owner.clone();
            let conversation = checked_conversation.clone();
            Box::pin(async move { matches!(repo.get(&owner, &conversation).await, Ok(Some(_))) })
        })
    }

    pub(crate) fn charge_acp_extension(&self, owner: &str) -> Result<(), ExtensionError> {
        Ok(self.acp_extension_limits.charge(owner)?)
    }

    pub(crate) async fn discover_acp_extensions(
        &self,
        owner: &str,
        conversation: &str,
    ) -> Result<AcpExtensionDiscovery, ExtensionError> {
        self.acp_extension_limits.charge(owner)?;
        self.ensure_owned_conversation(owner, conversation)
            .await
            .map_err(ExtensionError::Conversation)?;
        self.runtime_state()
            .with_relay_admission(conversation, || {
                self.with_extension_agent(owner, conversation, |agent| Ok(agent.relay_discovery()?))
            })
            .ok_or(RelayError::NoLiveTask)?
    }

    pub(crate) async fn call_acp_extension(
        &self,
        owner: &str,
        conversation: &str,
        request: AcpExtensionRequest,
        guard: Option<&aionui_common::WsSessionGuard>,
    ) -> Result<AcpExtensionResult, ExtensionError> {
        let binding = request.binding_id.clone();
        self.ensure_owned_conversation(owner, conversation)
            .await
            .map_err(ExtensionError::Conversation)?;
        if let Some(guard) = guard
            && !guard.recheck().await
        {
            return Err(ExtensionError::Authentication);
        }
        let deadline = Instant::now() + DEADLINE;
        let (call, reservation) = self
            .runtime_state()
            .with_relay_admission(conversation, || {
                if guard.is_some_and(|guard| !guard.valid()) {
                    return Err(ExtensionError::Authentication);
                }
                self.with_extension_agent(owner, conversation, |agent| {
                    // Compare the current nonce before using it as a budget key.
                    let discovery = agent.relay_discovery()?;
                    if discovery.binding_id != request.binding_id {
                        return Err(RelayError::StaleBinding.into());
                    }
                    if !discovery.requests.contains(&request.method) {
                        return Err(RelayError::MethodDenied.into());
                    }
                    let reservation = self.acp_extension_limits.reserve(owner, &request.binding_id)?;
                    let admission = self.relay_write_admission(owner, conversation, agent, guard.cloned(), deadline);
                    let call = agent.relay_dispatch(request, admission)?;
                    Ok((call, reservation))
                })
            })
            .ok_or(RelayError::StaleBinding)??;

        // The task keeps the execution/pending budgets across HTTP aborts and
        // releases them only when the session's SDK correlation is recovered.
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(settle_call(call, reservation, deadline, tx));
        let result = rx.await.map_err(|_| RelayError::TransportUnknown)?;
        if matches!(result, Ok(_) | Err(RelayError::Agent(_))) {
            if let Some(guard) = guard
                && !guard.recheck().await
            {
                return Err(RelayError::BindingUnknown.into());
            }
            self.runtime_state()
                .with_relay_admission(conversation, || {
                    self.with_extension_agent(owner, conversation, |agent| match agent.relay_discovery() {
                        Ok(current) if current.binding_id == binding => Ok(()),
                        _ => Err(RelayError::BindingUnknown.into()),
                    })
                })
                .ok_or(RelayError::BindingUnknown)?
                .map_err(|_| RelayError::BindingUnknown)?;
        }
        Ok(AcpExtensionResult { result: result? })
    }
}

async fn settle_call(
    mut call: RelayCall,
    mut reservation: Reservation,
    deadline: Instant,
    tx: tokio::sync::oneshot::Sender<Result<serde_json::Value, RelayError>>,
) {
    let mut result_tx = Some(tx);
    tokio::select! {
        biased;
        result = &mut call.reply => {
            report_result(result_tx.take(), response(&call, result));
            return;
        }
        _ = call.binding_generation.changed() => {
            let reason = (*call.binding_generation.borrow()).unwrap_or(RelayError::BindingUnknown);
            report_result(result_tx.take(), Err(binding_failure(&call, reason)));
        }
        _ = tokio::time::sleep_until(deadline) => {}
    }
    // A binding can be replaced while its old request is still pending.
    // Retain the underlying slot until its own deadline even in that case.
    if let Ok(result) = tokio::time::timeout_at(deadline, &mut call.reply).await {
        report_result(result_tx.take(), response(&call, result));
        return;
    }
    reservation.expire();
    report_result(result_tx.take(), Err(RelayError::DeadlineUnknown));
    let _ = call.reply.await;
}

fn response(
    call: &RelayCall,
    reply: Result<Result<serde_json::Value, RelayError>, tokio::sync::oneshot::error::RecvError>,
) -> Result<serde_json::Value, RelayError> {
    if matches!(
        reply,
        Ok(Err(RelayError::AuthorizationRevoked | RelayError::StaleBinding))
    ) {
        return reply.unwrap();
    }
    if let Some(reason) = *call.binding_generation.borrow() {
        return Err(binding_failure(call, reason));
    }
    reply.map_err(|_| RelayError::TransportUnknown)?
}

fn binding_failure(call: &RelayCall, reason: RelayError) -> RelayError {
    if reason == RelayError::BindingUnknown && !call.started.load(std::sync::atomic::Ordering::Acquire) {
        RelayError::StaleBinding
    } else {
        reason
    }
}

fn report_result(
    tx: Option<tokio::sync::oneshot::Sender<Result<serde_json::Value, RelayError>>>,
    result: Result<serde_json::Value, RelayError>,
) {
    if let Some(tx) = tx {
        if let Err(error @ (RelayError::TransportUnknown | RelayError::BindingUnknown | RelayError::DeadlineUnknown)) =
            &result
        {
            tracing::info!(?error, "ACP session extension outcome unknown");
        }
        let _ = tx.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn acp_relay_rate_budget_is_shared_and_refills_at_ten_per_second() {
        let limits = ExtensionLimits::default();
        for _ in 0..20 {
            limits.charge("same user").unwrap();
        }
        assert_eq!(limits.charge("same user"), Err(RelayError::RateLimited));
        limits.charge("another user").unwrap();
        tokio::time::advance(std::time::Duration::from_millis(99)).await;
        assert_eq!(limits.charge("same user"), Err(RelayError::RateLimited));
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        limits.charge("same user").unwrap();
        assert_eq!(limits.charge("same user"), Err(RelayError::RateLimited));
    }

    #[tokio::test(start_paused = true)]
    async fn acp_relay_deadline_reports_unknown_and_accounts_for_a_late_reply() {
        let limits = Arc::new(ExtensionLimits::default());
        limits.charge("owner").unwrap();
        let reservation = limits.reserve("owner", "binding").unwrap();
        let (peer, reply) = tokio::sync::oneshot::channel();
        let (_live, binding_generation) = tokio::sync::watch::channel(None);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(settle_call(
            RelayCall {
                reply,
                binding_generation,
                started: Arc::default(),
            },
            reservation,
            Instant::now() + DEADLINE,
            tx,
        ));
        assert_eq!(rx.await.unwrap(), Err(RelayError::DeadlineUnknown));
        {
            let owners = limits.0.lock().unwrap();
            let budget = &owners["owner"];
            assert_eq!((budget.active, budget.overdue), (0, 1));
        }
        assert!(!task.is_finished());
        peer.send(Ok(serde_json::Value::Null)).unwrap();
        task.await.unwrap();
        assert_eq!(limits.0.lock().unwrap()["owner"].overdue, 0);
    }

    #[test]
    fn acp_relay_pending_limits_release_and_quarantine_by_binding() {
        let limits = Arc::new(ExtensionLimits::default());
        limits.charge("owner").unwrap();
        let mut a = (0..4)
            .map(|_| limits.reserve("owner", "a").unwrap())
            .collect::<Vec<_>>();
        let b = (0..4)
            .map(|_| limits.reserve("owner", "b").unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(limits.reserve("owner", "a"), Err(RelayError::RateLimited)));
        assert!(matches!(limits.reserve("owner", "c"), Err(RelayError::RateLimited)));
        for request in a.iter_mut().take(2) {
            request.expire();
        }
        let extra = [
            limits.reserve("owner", "a").unwrap(),
            limits.reserve("owner", "a").unwrap(),
        ];
        a[2].expire();
        assert!(matches!(limits.reserve("owner", "a"), Err(RelayError::RateLimited)));
        limits.reserve("owner", "new generation").unwrap();
        drop(a.remove(0));
        limits.reserve("owner", "a").unwrap();
        drop((a, b, extra));
        assert!(limits.0.lock().unwrap()["owner"].bindings.is_empty());
    }
}
