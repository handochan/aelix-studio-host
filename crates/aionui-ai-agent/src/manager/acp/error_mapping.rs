use crate::error::AgentError;
use crate::protocol::error::AcpError;
use crate::protocol::send_error::AgentSendError;
use aionui_api_types::{AgentErrorCode, AgentErrorOwnership};

#[derive(Debug)]
pub(super) enum AcpSendFailure {
    Agent(AgentError),
    Acp(AcpError),
    PreludePersistence(AgentError),
}

impl AcpSendFailure {
    #[allow(dead_code)]
    pub(super) fn to_agent_send_error(&self) -> AgentSendError {
        self.to_agent_send_error_for_backend(None)
    }

    pub(super) fn to_agent_send_error_for_backend(&self, backend: Option<&str>) -> AgentSendError {
        match self {
            AcpSendFailure::Agent(err) => AgentSendError::from_agent_error_ref_for_backend(err, backend),
            AcpSendFailure::Acp(err) => AgentSendError::from_acp_error_ref_for_backend(err, backend),
            // A fresh runtime could run the same input with a different ACP
            // session ID. Leave a failed prelude write for the caller to retry.
            AcpSendFailure::PreludePersistence(err) => AgentSendError::new(
                "AionUI could not save the deferred Agent prelude",
                AgentErrorCode::AionuiInternalError,
                AgentErrorOwnership::Aionui,
                Some(err.public_message()),
                false,
                true,
                None,
            ),
        }
    }

    pub(super) fn into_agent_error(self) -> AgentError {
        match self {
            AcpSendFailure::Agent(err) | AcpSendFailure::PreludePersistence(err) => err,
            AcpSendFailure::Acp(err) => AgentError::Acp(err),
        }
    }
}

impl From<AgentError> for AcpSendFailure {
    fn from(err: AgentError) -> Self {
        AcpSendFailure::Agent(err)
    }
}

impl From<AcpError> for AcpSendFailure {
    fn from(err: AcpError) -> Self {
        AcpSendFailure::Acp(err)
    }
}

impl std::fmt::Display for AcpSendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpSendFailure::Agent(err) | AcpSendFailure::PreludePersistence(err) => std::fmt::Display::fmt(err, f),
            AcpSendFailure::Acp(err) => f.write_str(&acp_error_public_message(err)),
        }
    }
}

impl std::error::Error for AcpSendFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AcpSendFailure::Agent(err) | AcpSendFailure::PreludePersistence(err) => Some(err),
            AcpSendFailure::Acp(err) => Some(err),
        }
    }
}

pub(super) fn is_acp_session_not_found(err: &AcpError) -> bool {
    matches!(err, AcpError::SessionNotFound { .. })
}

pub(super) fn is_missing_resumed_session(err: &AcpError, resumed_session_id: &str) -> bool {
    is_acp_session_not_found(err)
        || matches!(
            err,
            AcpError::ResourceNotFound {
                resource: Some(resource),
                ..
            } if resource == resumed_session_id
        )
}

fn acp_error_public_message(err: &AcpError) -> String {
    match err {
        AcpError::AgentInternal { code, .. } => format!("Agent internal error (code {code})"),
        _ => err.to_string(),
    }
}
