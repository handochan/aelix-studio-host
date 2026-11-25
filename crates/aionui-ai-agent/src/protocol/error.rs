use agent_client_protocol::{Error as SdkError, ErrorCode};
use aionui_common::AgentKillReason;

/// Why an ACP session was closed / terminated.
///
/// Captured at the close site (cancel / kill / send-message-error) so the
/// next user-facing toast can render something better than "session closed"
/// or "Bad gateway". `summary` is the redacted, user-safe message — stderr
/// MUST be filtered through `stderr_error_extractor::extract_error_message`
/// before reaching this type. Raw stderr is logged via `tracing` only and
/// must never land here.
///
/// Lifecycle:
/// - writer: `AcpSession::record_close_reason`, called by the manager when
///   a close path runs (`send_message` Err, `cancel`, `kill`, post-init
///   process exit detection).
/// - reader: `AcpSession::last_close_reason`, drained by the manager when
///   composing the user-facing error message for the next toast.
/// - invalidation: cleared on `clear_session_id` and on
///   `record_close_reason(None)` so a rebuilt session starts fresh.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // `Killed`/`UserCancel` are wired by the manager close path; kept for completeness across all close sites.
pub enum CloseReason {
    /// User cancelled the in-flight prompt via `cancel()`. Distinct from
    /// `Killed` so the toast can say "cancelled" instead of "killed".
    UserCancel,

    /// Manager invoked `kill()` (idle timeout, conversation deletion, …).
    /// Carries the structured reason so the toast text is actionable.
    Killed { reason: Option<AgentKillReason> },

    /// CLI process exited unexpectedly. Mirrors `AcpError::Disconnected`
    /// but with a redacted summary; stderr stays in tracing logs only.
    ProcessExited {
        exit_code: Option<i32>,
        signal: Option<String>,
        /// User-safe summary derived from `extract_error_message` over the
        /// stderr tail. Empty when the extractor's allowlist did not match.
        redacted_summary: String,
    },

    /// Generic upstream / protocol failure that closed the turn but where
    /// the process is still alive. `display` is the
    /// `user_facing_message`-stripped form of the originating agent error,
    /// so it never starts with "Bad gateway: ".
    Failed { display: String },
}

impl CloseReason {
    /// Render a single-line, user-facing summary safe to broadcast over
    /// WebSocket / put into HTTP responses. stderr never leaves the
    /// `redacted_summary` field, which is itself allowlist-filtered.
    pub fn user_facing_message(&self) -> String {
        match self {
            CloseReason::UserCancel => "Conversation cancelled".to_owned(),
            CloseReason::Killed { reason } => match reason {
                Some(AgentKillReason::IdleTimeout) => "Agent killed: idle timeout".to_owned(),
                Some(AgentKillReason::AgentErrorRecovery) => "Agent killed: error recovery".to_owned(),
                Some(AgentKillReason::TeamMcpRebuild) => "Agent killed: team MCP rebuild".to_owned(),
                Some(AgentKillReason::TeamContextReset) => "Agent killed: team context reset".to_owned(),
                Some(AgentKillReason::TeamDeleted) => "Agent killed: team deleted".to_owned(),
                Some(AgentKillReason::ConversationDeleted) => "Agent killed: conversation deleted".to_owned(),
                Some(AgentKillReason::UserCancelTimeout) => "Conversation cancelled; agent restarted".to_owned(),
                Some(AgentKillReason::RuntimeRestart) => "Conversation cancelled; agent restarted".to_owned(),
                Some(AgentKillReason::RuntimeCapabilityChanged) => {
                    "Agent killed: runtime capability changed".to_owned()
                }
                Some(AgentKillReason::SessionRevoked) => "Agent killed: session revoked".to_owned(),
                Some(AgentKillReason::Archived) => "Agent killed: archived".to_owned(),
                None => "Agent killed".to_owned(),
            },
            CloseReason::ProcessExited {
                exit_code,
                signal,
                redacted_summary,
            } => {
                let detail = format_exit_detail(*exit_code, signal.as_deref());
                if redacted_summary.is_empty() {
                    format!("Agent process exited{detail}")
                } else {
                    format!("Agent process exited{detail}: {redacted_summary}")
                }
            }
            CloseReason::Failed { display } => display.clone(),
        }
    }
}

/// Workspace-facing owned ACP error contract.
///
/// Variants and fields are exposed for structured classification by workspace
/// callers, but must not be rendered directly to public HTTP or WebSocket
/// output. The `stderr` fields may contain sensitive data; `Display`
/// intentionally omits them.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
#[allow(dead_code)] // Variants constructed as error paths mature; kept for complete ACP error model.
pub enum AcpError {
    // ── Process lifecycle ──────────────────────────────────────────
    /// CLI binary not found or not executable.
    SpawnFailed { message: String },

    /// Process exited before the initialize handshake completed.
    StartupCrash {
        exit_code: Option<i32>,
        signal: Option<String>,
        stderr: String,
    },

    /// Process crashed while a request was in flight.
    Disconnected {
        exit_code: Option<i32>,
        signal: Option<String>,
        stderr: String,
    },

    // ── ACP protocol errors (from SDK ErrorCode) ──────────────────
    /// Agent requires authentication first.
    AuthRequired,

    /// Agent returned JSON that the protocol layer could not parse.
    ProtocolParseError { message: String },

    /// Agent rejected a malformed JSON-RPC request object.
    InvalidRequest { message: String },

    /// Agent-side session not found.
    SessionNotFound { session_id: String },

    /// Agent-side resource not found. This is distinct from stale ACP session
    /// IDs; `session-not-found` payloads are normalized to `SessionNotFound`
    /// before this variant is constructed.
    ResourceNotFound { resource: Option<String>, message: String },

    /// Agent does not support the requested method.
    MethodNotFound { method: String },

    /// Invalid request parameters.
    InvalidParams { message: String },

    /// Agent reported an internal error. `data` carries the optional JSON-RPC
    /// `error.data` payload from the agent — see the [`Display`] impl for how
    /// it is rendered.
    ///
    /// [`Display`]: std::fmt::Display
    AgentInternal {
        message: String,
        code: i32,
        data: Option<serde_json::Value>,
    },

    /// Agent returned a custom JSON-RPC/ACP error code outside the standard
    /// set. Keep it structured so UI can show a protocol error instead of an
    /// unknown upstream failure.
    OtherProtocolError {
        code: i32,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// Application contract from the negotiated Aelix adapter. Other agents'
    /// errors retain the generic protocol mapping and data exposure policy.
    ApplicationError {
        code: i32,
        message: String,
        data: Option<serde_json::Value>,
    },

    // ── Local errors ──────────────────────────────────────────────
    /// Protocol not connected (used before connect or after disconnect).
    NotConnected,

    /// The prompt was handed to the SDK, then its connection was lost without
    /// a response. Execution may already have happened; never replay this turn.
    PromptOutcomeUnknown,

    /// Initialize handshake timed out.
    InitTimeout { timeout_secs: u64 },

    /// A config/mode/model RPC did not return within CONFIG_RPC_TIMEOUT_SECS.
    /// Distinct from `InitTimeout`, which covers only the initialize handshake.
    RequestTimeout { method: String, timeout_secs: u64 },
}

/// Format the human-readable suffix for `StartupCrash` / `Disconnected`.
/// stderr is deliberately omitted; app-layer mappers must use `Display`
/// instead of serializing raw process output.
fn format_exit_detail(exit_code: Option<i32>, signal: Option<&str>) -> String {
    match (exit_code, signal) {
        (Some(code), Some(sig)) => format!(" (exit code {code}, {sig})"),
        (Some(code), None) => format!(" (exit code {code})"),
        (None, Some(sig)) => format!(" ({sig})"),
        (None, None) => String::new(),
    }
}

/// JSON-RPC default message strings that carry no useful information.
/// When `AgentInternal` arrives with one of these as its `message`, we fall
/// back to a diagnostic display ("Agent internal error (code -32603)").
///
/// These strings are copied from `ErrorCode`'s `strum::Display` attributes in
/// `agent-client-protocol-schema`. If the SDK changes them, update this list
/// to avoid silently reverting to the diagnostic fallback.
const SDK_DEFAULT_MESSAGES: &[&str] = &[
    "Parse error",
    "Invalid request",
    "Method not found",
    "Invalid params",
    "Internal error",
];

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcpError::SpawnFailed { message } => {
                write!(f, "Failed to spawn agent process: {message}")
            }
            AcpError::StartupCrash { exit_code, signal, .. } => {
                // stderr intentionally NOT included — may carry secrets.
                let detail = format_exit_detail(*exit_code, signal.as_deref());
                write!(f, "Agent process exited before initialize handshake completed{detail}")
            }
            AcpError::Disconnected { exit_code, signal, .. } => {
                let detail = format_exit_detail(*exit_code, signal.as_deref());
                write!(f, "Agent process disconnected{detail}")
            }
            AcpError::AuthRequired => f.write_str("Authentication required"),
            AcpError::ProtocolParseError { message } => {
                write!(f, "Agent protocol parse error: {message}")
            }
            AcpError::InvalidRequest { message } => {
                write!(f, "Agent rejected an invalid protocol request: {message}")
            }
            AcpError::SessionNotFound { session_id } => {
                write!(f, "Session not found: {session_id}")
            }
            AcpError::ResourceNotFound { resource, message } => match resource {
                Some(resource) => write!(f, "Agent resource not found: {resource} ({message})"),
                None => write!(f, "Agent resource not found: {message}"),
            },
            AcpError::MethodNotFound { method } => {
                write!(f, "Method not supported: {method}")
            }
            AcpError::InvalidParams { message } => {
                write!(f, "Invalid parameters: {message}")
            }
            AcpError::AgentInternal { message, code, data } => {
                let trimmed = message.trim();
                let is_default =
                    trimmed.is_empty() || SDK_DEFAULT_MESSAGES.iter().any(|d| d.eq_ignore_ascii_case(trimmed));
                if is_default {
                    write!(f, "Agent internal error (code {code})")?;
                } else {
                    f.write_str(trimmed)?;
                }
                if let Some(data) = data {
                    // serde_json::to_string on a Value cannot actually fail;
                    // the fallback exists only because Display must be infallible.
                    let compact = serde_json::to_string(data).unwrap_or_else(|_| "<unserializable data>".to_owned());
                    write!(f, " ({compact})")?;
                }
                Ok(())
            }
            AcpError::OtherProtocolError { code, message, data }
            | AcpError::ApplicationError { code, message, data } => {
                write!(f, "Agent protocol error (code {code}): {message}")?;
                if let Some(data) = data {
                    let compact = serde_json::to_string(data).unwrap_or_else(|_| "<unserializable data>".to_owned());
                    write!(f, " ({compact})")?;
                }
                Ok(())
            }
            AcpError::NotConnected => f.write_str("ACP protocol not connected"),
            AcpError::PromptOutcomeUnknown => {
                f.write_str("The prompt was sent, but the Agent connection was lost and its outcome is unknown")
            }
            AcpError::InitTimeout { timeout_secs } => {
                write!(f, "Initialize handshake timed out after {timeout_secs}s")
            }
            AcpError::RequestTimeout { method, timeout_secs } => {
                write!(f, "Agent request '{method}' timed out after {timeout_secs}s")
            }
        }
    }
}

impl AcpError {
    pub(crate) fn is_model_selection_required(&self) -> bool {
        matches!(
            self,
            Self::ApplicationError { code: -32004, data: Some(data), .. }
                if model_selection_count(data).is_some()
        )
    }

    /// Apply only after prompt dispatch. Do not infer delivery from the absence
    /// of streamed output, or classify ordinary agent error replies as EOF.
    pub(crate) fn after_prompt_dispatch(self, transport_closed: bool) -> Self {
        if matches!(self, Self::Disconnected { .. } | Self::NotConnected)
            || (transport_closed && matches!(self, Self::AgentInternal { .. }))
        {
            Self::PromptOutcomeUnknown
        } else {
            self
        }
    }

    /// Whether the caller may retry the operation.
    #[allow(dead_code)] // Will be used once retry logic is wired into the send path.
    pub(crate) fn is_retryable(&self) -> bool {
        matches!(
            self,
            AcpError::SpawnFailed { .. }
                | AcpError::StartupCrash { .. }
                | AcpError::Disconnected { .. }
                | AcpError::AgentInternal { .. }
                | AcpError::InitTimeout { .. }
                | AcpError::RequestTimeout { .. }
        )
    }

    /// Keep the SDK's classification while discarding free-form peer details
    /// at the Host error boundary. HTTP, WS and diagnostics share this error.
    pub fn without_remote_detail(mut self, method: &str) -> Self {
        let startup_details = self.public_startup_diagnostic().map(|(_, details)| details);
        match &mut self {
            Self::SessionNotFound { session_id } => *session_id = method.to_owned(),
            Self::ProtocolParseError { message }
            | Self::InvalidRequest { message }
            | Self::InvalidParams { message } => *message = ACP_REQUEST_FAILED.into(),
            Self::ResourceNotFound { resource, message } => {
                *resource = None;
                *message = ACP_REQUEST_FAILED.into();
            }
            Self::ApplicationError { code, message, data } => {
                *message = ACP_REQUEST_FAILED.into();
                *data = startup_details.or_else(|| safe_application_data(*code, data.as_ref()));
            }
            Self::OtherProtocolError { message, data, .. } => {
                *message = ACP_REQUEST_FAILED.into();
                *data = startup_details;
            }
            Self::AgentInternal { message, data, .. } => {
                *message = ACP_REQUEST_FAILED.into();
                *data = None;
            }
            _ => {}
        }
        self
    }

    /// Host-authored guidance and allowlisted details for ACP startup failures.
    pub fn public_startup_diagnostic(&self) -> Option<(&'static str, serde_json::Value)> {
        match self {
            Self::ApplicationError { data, .. } => safe_startup_diagnostic(data.as_ref()),
            Self::OtherProtocolError { code, data, .. } if matches!(ErrorCode::from(*code), ErrorCode::Other(_)) => {
                safe_startup_diagnostic(data.as_ref())
            }
            _ => None,
        }
    }

    /// Convert an SDK [`Error`](SdkError) into an [`AcpError`].
    ///
    /// Mapping is by [`ErrorCode`], never by message text. The single
    /// exceptions are known stale-session shapes: `data.error == "Session not
    /// found: ..."` and resource-not-found replies from `session/load`
    /// without a concrete resource URI. OpenCode/Codex have emitted stale
    /// resume failures with those shapes, so we re-classify them into
    /// `SessionNotFound` to keep recovery paths uniform across agents.
    /// See ELECTRON-1HQ.
    /// `context` carries the session ID or method name for diagnostics.
    pub fn from_sdk(err: SdkError, context: &str) -> Self {
        Self::from_sdk_for_agent(err, context, None)
    }

    pub(crate) fn from_sdk_for_agent(err: SdkError, context: &str, agent_name: Option<&str>) -> Self {
        let code = i32::from(err.code);
        // agent-client-protocol-schema-1.5.0/src/v1/error.rs reserves -32000 for AuthRequired.
        // Only the identified Aelix adapter uses it for busy/limit instead:
        // aelix-acp@16d6555/src/aelix_acp/server.py (initialize and dispatch).
        // Preserve standard authentication behavior for every other agent.
        if agent_name == Some("aelix-acp") && matches!(code, -33001 | -33002 | -32000 | -32004) {
            let data = if code == -32004
                && err
                    .data
                    .as_ref()
                    .and_then(|data| data.get("kind"))
                    .and_then(serde_json::Value::as_str)
                    == Some(MODEL_SELECTION_REQUIRED)
            {
                safe_application_data(code, err.data.as_ref())
            } else {
                err.data
            };
            return AcpError::ApplicationError {
                code,
                message: err.message,
                data,
            };
        }
        match err.code {
            ErrorCode::AuthRequired => AcpError::AuthRequired,
            ErrorCode::ParseError => AcpError::ProtocolParseError { message: err.message },
            ErrorCode::InvalidRequest => AcpError::InvalidRequest { message: err.message },
            ErrorCode::ResourceNotFound => {
                if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else if extract_resource_not_found(err.data.as_ref()).is_none() && is_session_load_method(context) {
                    AcpError::SessionNotFound {
                        session_id: context.to_owned(),
                    }
                } else {
                    AcpError::ResourceNotFound {
                        resource: extract_resource_not_found(err.data.as_ref()),
                        message: err.message,
                    }
                }
            }
            ErrorCode::MethodNotFound => AcpError::MethodNotFound {
                method: context.to_owned(),
            },
            ErrorCode::InvalidParams => {
                if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else {
                    AcpError::InvalidParams { message: err.message }
                }
            }
            ErrorCode::InternalError => {
                if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else {
                    AcpError::AgentInternal {
                        message: err.message,
                        code: i32::from(err.code),
                        data: err.data,
                    }
                }
            }
            _ => {
                let code = i32::from(err.code);
                // -32001: additional session-not-found code used by some agents.
                // -32002 is ACP ResourceNotFound and is handled above by ErrorCode.
                if code == -32001 {
                    AcpError::SessionNotFound {
                        session_id: context.to_owned(),
                    }
                } else if matches!(err.code, ErrorCode::Other(_))
                    && let Some((_, details)) = safe_startup_diagnostic(err.data.as_ref())
                {
                    // Classify the structured startup error before inspecting
                    // agent prose for legacy stale-session signals.
                    AcpError::OtherProtocolError {
                        code,
                        message: "ACP request failed".into(),
                        data: Some(details),
                    }
                } else if let Some(sid) = extract_session_not_found(err.data.as_ref()) {
                    AcpError::SessionNotFound { session_id: sid }
                } else {
                    AcpError::OtherProtocolError {
                        code,
                        message: err.message,
                        data: err.data,
                    }
                }
            }
        }
    }
}

pub(super) const ACP_REQUEST_FAILED: &str = "ACP request failed";
pub(super) const MODEL_SELECTION_REQUIRED: &str = "model_selection_required";

fn model_selection_count(data: &serde_json::Value) -> Option<u32> {
    if data.as_object()?.len() != 4
        || data.get("kind")?.as_str() != Some(MODEL_SELECTION_REQUIRED)
        || data.get("configId")?.as_str() != Some("model")
        || data.get("retryable")?.as_bool() != Some(false)
    {
        return None;
    }
    // A u32 count is exactly representable by Studio's JavaScript numbers.
    u32::try_from(data.get("availableModels")?.as_u64()?).ok()
}

/// Project a startup diagnostic from the ACP data object. Verified adapter
/// example: aelix-acp@7a2ca0e/src/aelix_acp/session.py and src/aelix_acp/wire/errors.py.
fn safe_startup_diagnostic(data: Option<&serde_json::Value>) -> Option<(&'static str, serde_json::Value)> {
    use serde_json::{Value, json};

    let data = data?;
    if data.get("kind").and_then(Value::as_str) != Some("socket_path_too_long") {
        return None;
    }
    let mut details = json!({"kind":"socket_path_too_long"});
    if let Some(limit) = data
        .get("limitBytes")
        .and_then(Value::as_u64)
        .and_then(|limit| u16::try_from(limit).ok())
        .filter(|limit| *limit > 0)
    {
        details["limitBytes"] = json!(limit);
    }
    Some((
        "ACP could not start: the temporary directory path is too long. Set a shorter TMPDIR.",
        details,
    ))
}

/// Retain the allowlisted diagnostics consumed by Studio.
fn safe_application_data(code: i32, data: Option<&serde_json::Value>) -> Option<serde_json::Value> {
    use serde_json::{Value, json};

    let data = data?;
    let kind = data.get("kind").and_then(Value::as_str)?;
    match (code, kind) {
        (-33001, "session_locked") => {
            let locked_by = data.get("lockedBy").and_then(Value::as_str)?;
            if !matches!(locked_by, "native" | "acp") {
                return None;
            }
            Some(json!({"kind":kind,"lockedBy":locked_by,"holder":null,"retryable":false}))
        }
        (-33002, "turn_failed_after_dispatch") => Some(json!({"kind":kind,"retryable":false})),
        (-32000, "session_changing") => Some(json!({"kind":kind})),
        (-32004, MODEL_SELECTION_REQUIRED) => {
            // aelix-acp@a6545f0b/src/aelix_acp/wire/errors.py defines this refusal.
            let available_models = model_selection_count(data)?;
            Some(json!({"kind":kind,"configId":"model","availableModels":available_models,"retryable":false}))
        }
        (-32004, "studio_extension_management_required") => {
            let action = data.get("action").and_then(Value::as_str)?;
            matches!(action, "install" | "update" | "remove").then(|| json!({"kind":kind,"action":action}))
        }
        (-32004, "restrictions_unenforceable") => {
            // The adapter checks this finite set before restoring a session.
            let requested = data.get("methods").and_then(Value::as_array);
            let methods: Vec<_> = ["agents/use", "tools/set_active", "permissions/set"]
                .into_iter()
                .filter(|method| requested.is_some_and(|items| items.iter().any(|item| item.as_str() == Some(*method))))
                .collect();
            Some(json!({"kind":kind,"methods":methods}))
        }
        // Studio uses a non-empty label to select each recovery message.
        // Profile names and extension references (including basenames) are peer data.
        (-32004, "profile_unavailable") => Some(json!({"kind":kind,"profile":"saved session profile"})),
        (-32004, "extension_owned_by_launch") => Some(json!({"kind":kind,"source":"launch-managed extension"})),
        (-32004, "client_action_required" | "runtime_control_required") => Some(json!({"kind":kind})),
        _ => None,
    }
}

fn is_session_load_method(context: &str) -> bool {
    context == "session/load"
}

/// If `data` carries a `{"error": "Session not found: <sid>"}` payload
/// (either as a JSON object or as a JSON-string-of-JSON, which is what
/// OpenCode actually emits — see ELECTRON-1HQ), return the session id.
/// Returns `None` for any other shape so callers can fall through to
/// the default `code`-based mapping.
/// Pull a stale session id out of a JSON-RPC `data` payload.
///
/// Matches on the VALUE, not on a key name. Agents disagree about where they
/// put this string — OpenCode/Codex use `error`, glm-acp-agent uses `details`
/// — and a fixed key list fails closed: the payload degrades to
/// `AgentInternal`, `is_acp_session_not_found` returns false, and the
/// `session/load` rebuild in `open_session_resume` never fires. The stale id
/// then survives on disk and every later turn replays it, so the real failure
/// (live case: a missing `Z_AI_API_KEY`) stays permanently hidden behind
/// `Session not found`. Scanning the object's string values keeps recovery
/// working for agents we have never probed.
fn extract_session_not_found(data: Option<&serde_json::Value>) -> Option<String> {
    let value = data?;
    let obj = match value {
        serde_json::Value::Object(_) => value.clone(),
        serde_json::Value::String(s) => serde_json::from_str(s).ok()?,
        _ => return None,
    };
    let prefix = "Session not found: ";
    obj.as_object()?.values().filter_map(|v| v.as_str()).find_map(|msg| {
        let sid = msg.strip_prefix(prefix)?.trim();
        (!sid.is_empty()).then(|| sid.to_owned())
    })
}

fn extract_resource_not_found(data: Option<&serde_json::Value>) -> Option<String> {
    let value = data?;
    let obj = match value {
        serde_json::Value::Object(_) => value.clone(),
        serde_json::Value::String(s) => serde_json::from_str(s).ok()?,
        _ => return None,
    };
    obj.get("uri").and_then(|uri| uri.as_str()).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_startup_details_map_known_kind_and_optional_hint() {
        let known = SdkError::new(-33004, "private message")
            .data(serde_json::json!({"kind":"socket_path_too_long","limitBytes":104}));
        let error = AcpError::from_sdk(known, "session/new").without_remote_detail("session/new");
        assert_eq!(
            error.public_startup_diagnostic(),
            Some((
                "ACP could not start: the temporary directory path is too long. Set a shorter TMPDIR.",
                serde_json::json!({"kind":"socket_path_too_long","limitBytes":104})
            ))
        );

        let kind_only =
            SdkError::new(-33005, "private message").data(serde_json::json!({"kind":"socket_path_too_long"}));
        let error = AcpError::from_sdk(kind_only, "session/new");
        assert_eq!(
            error.public_startup_diagnostic().map(|(_, details)| details),
            Some(serde_json::json!({"kind":"socket_path_too_long"}))
        );
    }

    #[test]
    fn acp_startup_details_omit_unknown_kind_and_missing_data() {
        let unknown = SdkError::new(-33004, "socket_path_too_long")
            .data(serde_json::json!({"kind":"/private/unknown","limitBytes":104}));
        let missing = SdkError::new(-33004, "socket_path_too_long");
        let unrelated = SdkError::new(-32602, "private message")
            .data(serde_json::json!({"kind":"socket_path_too_long","limitBytes":104}));
        let cancelled = SdkError::new(-32800, "cancelled").data(serde_json::json!({"kind":"socket_path_too_long"}));
        for reply in [unknown, missing, unrelated, cancelled] {
            assert_eq!(
                AcpError::from_sdk(reply, "session/new").public_startup_diagnostic(),
                None
            );
        }
    }

    #[test]
    fn acp_startup_details_discard_malicious_and_oversized_data() {
        use serde_json::json;

        for invalid_limit in [
            json!(u64::MAX),
            json!(0),
            json!(-1),
            json!("104"),
            json!({"path":"/private"}),
        ] {
            let reply = SdkError::new(-33004, "/private/message").data(json!({
                "kind":"socket_path_too_long",
                "limitBytes":invalid_limit,
                "error":"Session not found: /private/session",
                "stderr":"/private/credential".repeat(16_384),
                "nested":{"token":"fixture-secret"}
            }));
            let error = AcpError::from_sdk(reply, "session/new");
            assert_eq!(
                error.public_startup_diagnostic().map(|(_, details)| details),
                Some(json!({"kind":"socket_path_too_long"}))
            );
            assert!(!error.to_string().contains("/private/"));
        }
    }

    // ── CloseReason ─────────────────────────────────────────────────────

    #[test]
    fn close_reason_user_cancel_is_user_friendly() {
        let msg = CloseReason::UserCancel.user_facing_message();
        assert_eq!(msg, "Conversation cancelled");
    }

    #[test]
    fn close_reason_killed_renders_each_kill_reason() {
        assert_eq!(
            CloseReason::Killed {
                reason: Some(AgentKillReason::IdleTimeout)
            }
            .user_facing_message(),
            "Agent killed: idle timeout"
        );
        assert_eq!(
            CloseReason::Killed {
                reason: Some(AgentKillReason::ConversationDeleted)
            }
            .user_facing_message(),
            "Agent killed: conversation deleted"
        );
        assert_eq!(
            CloseReason::Killed { reason: None }.user_facing_message(),
            "Agent killed"
        );
    }

    #[test]
    fn close_reason_process_exited_renders_exit_code_and_summary() {
        let msg = CloseReason::ProcessExited {
            exit_code: Some(127),
            signal: None,
            redacted_summary: "usage limit exceeded".into(),
        }
        .user_facing_message();
        assert!(msg.contains("exit code 127"), "got {msg}");
        assert!(msg.contains("usage limit exceeded"), "got {msg}");
    }

    #[test]
    fn close_reason_process_exited_omits_summary_when_empty() {
        // No allowlist match → no trailing colon, no stray noise.
        let msg = CloseReason::ProcessExited {
            exit_code: Some(1),
            signal: None,
            redacted_summary: String::new(),
        }
        .user_facing_message();
        assert!(msg.contains("exit code 1"), "got {msg}");
        assert!(!msg.ends_with(": "), "must not have a dangling colon; got {msg}");
    }

    #[test]
    fn close_reason_process_exited_includes_signal() {
        let msg = CloseReason::ProcessExited {
            exit_code: None,
            signal: Some("signal:9".into()),
            redacted_summary: String::new(),
        }
        .user_facing_message();
        assert!(msg.contains("signal:9"), "got {msg}");
    }

    #[test]
    fn close_reason_user_facing_message_is_safe_to_redisplay() {
        // The helper produced a synthetic stderr containing fake credentials.
        // The allowlist filter is responsible for keeping that out of the
        // `redacted_summary` field — the user_facing_message helper itself
        // must not invent or re-fetch any non-allowlisted content.
        let reason = CloseReason::ProcessExited {
            exit_code: Some(2),
            signal: None,
            redacted_summary: "rate limit exceeded".into(),
        };
        let msg = reason.user_facing_message();
        assert!(!msg.contains("Bearer"), "must not include synthetic secret material");
        assert!(!msg.contains("api_key="), "must not include synthetic secret material");
        assert!(msg.contains("rate limit exceeded"));
    }

    #[test]
    fn close_reason_failed_carries_through_user_facing_text() {
        let reason = CloseReason::Failed {
            display: "API Error: Internal server error".into(),
        };
        assert_eq!(reason.user_facing_message(), "API Error: Internal server error");
    }

    #[test]
    fn retryable_variants() {
        assert!(
            AcpError::SpawnFailed {
                message: "not found".into()
            }
            .is_retryable()
        );
        assert!(
            AcpError::StartupCrash {
                exit_code: Some(1),
                signal: None,
                stderr: String::new(),
            }
            .is_retryable()
        );
        assert!(
            AcpError::Disconnected {
                exit_code: None,
                signal: Some("SIGKILL".into()),
                stderr: String::new(),
            }
            .is_retryable()
        );
        assert!(
            AcpError::AgentInternal {
                message: "oops".into(),
                code: -32603,
                data: None,
            }
            .is_retryable()
        );
        assert!(AcpError::InitTimeout { timeout_secs: 30 }.is_retryable());
    }

    #[test]
    fn non_retryable_variants() {
        assert!(!AcpError::AuthRequired.is_retryable());
        assert!(
            !AcpError::SessionNotFound {
                session_id: "s1".into()
            }
            .is_retryable()
        );
        assert!(!AcpError::MethodNotFound { method: "foo".into() }.is_retryable());
        assert!(!AcpError::InvalidParams { message: "bad".into() }.is_retryable());
        assert!(!AcpError::NotConnected.is_retryable());
    }

    #[test]
    fn request_timeout_is_retryable() {
        assert!(
            AcpError::RequestTimeout {
                method: "session/setConfigOption".into(),
                timeout_secs: 10,
            }
            .is_retryable(),
            "config RPC timeout must be retryable so the user can immediately retry"
        );
    }

    #[test]
    fn request_timeout_display_names_method_and_timeout_without_sensitive_payload() {
        let display = AcpError::RequestTimeout {
            method: "session/setConfigOption".into(),
            timeout_secs: 10,
        }
        .to_string();
        assert!(display.contains("timed out after 10s"), "got {display}");
        assert!(display.contains("session/setConfigOption"), "got {display}");
        // Display must stay a single, redacted line — no payload, no newline.
        assert!(!display.contains('\n'), "must be single line; got {display}");
    }

    #[test]
    fn byte_grant_private_rpc_error_keeps_only_safe_structured_fields() {
        let input = serde_json::json!({"kind":"session_locked","lockedBy":"native",
            "holder":{"path":"private-file"}, "sessionId":"private-id"});
        let error = AcpError::ApplicationError {
            code: -33001,
            message: "private message".into(),
            data: Some(input),
        }
        .without_remote_detail("session/load");
        let AcpError::ApplicationError { data, code, message } = error else {
            panic!("lost protocol classification")
        };
        assert_eq!(code, -33001);
        assert_eq!(message, "ACP request failed");
        assert_eq!(
            data.unwrap(),
            serde_json::json!({"kind":"session_locked","lockedBy":"native","holder":null,"retryable":false})
        );
        let untrusted = serde_json::json!({"kind":"session_locked","lockedBy":"private-file"});
        assert!(safe_application_data(-33001, Some(&untrusted)).is_none());
        let refusal = serde_json::json!({"kind":"studio_extension_management_required",
            "action":"install","source":"private-file"});
        assert_eq!(
            safe_application_data(-32004, Some(&refusal)).unwrap(),
            serde_json::json!({"kind":"studio_extension_management_required","action":"install"})
        );
    }

    #[test]
    fn byte_grant_restriction_recovery_emits_only_allowlisted_methods() {
        use serde_json::json;

        let input = json!({"kind":"restrictions_unenforceable","methods":[
            "permissions/set", "private-file", "agents/use", {}, null, "tools/set_active",
            "agents/use", "tools/set_active/private", " Tools/set_active "
        ]});
        assert_eq!(
            safe_application_data(-32004, Some(&input)).unwrap(),
            json!({"kind":"restrictions_unenforceable","methods":["agents/use","tools/set_active","permissions/set"]})
        );
        for methods in [json!("agents/use"), json!(["private-file", null, {}])] {
            let input = json!({"kind":"restrictions_unenforceable","methods":methods});
            assert_eq!(
                safe_application_data(-32004, Some(&input)).unwrap(),
                json!({"kind":"restrictions_unenforceable","methods":[]})
            );
        }
    }

    #[test]
    fn model_selection_refusal_preserves_exact_fields_through_sdk_and_protected_relay() {
        use serde_json::json;

        for available in [0, 2, u32::MAX] {
            let input = json!({
                "kind": "model_selection_required", "configId": "model",
                "availableModels": available, "retryable": false,
            });
            let error = AcpError::from_sdk_for_agent(
                SdkError::new(-32004, "private peer message").data(input.clone()),
                "session/prompt",
                Some("aelix-acp"),
            );
            assert!(matches!(&error, AcpError::ApplicationError { data: Some(data), .. } if *data == input));
            assert!(matches!(
                error.without_remote_detail("session/prompt"),
                AcpError::ApplicationError { code: -32004, message, data: Some(data) }
                    if message == "ACP request failed" && data == input
            ));
        }
    }

    #[test]
    fn model_selection_refusal_drops_extra_missing_and_invalid_fields() {
        use serde_json::json;

        let valid = json!({
            "kind": "model_selection_required", "configId": "model",
            "availableModels": 2, "retryable": false,
        });
        for (field, value) in [
            ("extra", json!("private peer data")),
            ("configId", json!("other")),
            ("configId", json!(false)),
            ("availableModels", json!(-1)),
            ("availableModels", json!(2.0)),
            ("availableModels", json!(2.5)),
            ("availableModels", json!("2")),
            ("availableModels", json!(true)),
            ("availableModels", json!(u64::from(u32::MAX) + 1)),
            ("retryable", json!(true)),
            ("retryable", json!("false")),
            ("retryable", json!(null)),
            ("kind", json!("different_kind")),
            ("kind", json!(1)),
        ] {
            let mut input = valid.clone();
            input[field] = value;
            assert_eq!(safe_application_data(-32004, Some(&input)), None, "{input}");
            if field != "kind" {
                let error = AcpError::from_sdk_for_agent(
                    SdkError::new(-32004, "refused").data(input),
                    "session/prompt",
                    Some("aelix-acp"),
                );
                assert!(matches!(error, AcpError::ApplicationError { data: None, .. }));
            }
        }
        for field in ["kind", "configId", "availableModels", "retryable"] {
            let mut input = valid.clone();
            input.as_object_mut().unwrap().remove(field);
            assert_eq!(safe_application_data(-32004, Some(&input)), None, "{input}");
        }
        assert_eq!(safe_application_data(-32000, Some(&valid)), None);
        assert_eq!(safe_application_data(-32004, Some(&json!([valid]))), None);
    }

    #[test]
    fn from_sdk_auth_required() {
        let sdk_err = SdkError::auth_required();
        let acp = AcpError::from_sdk(sdk_err, "sess-1");
        assert!(matches!(acp, AcpError::AuthRequired));
    }

    #[test]
    fn from_sdk_parse_error_preserves_protocol_error() {
        let sdk_err = SdkError::parse_error();
        let acp = AcpError::from_sdk(sdk_err, "initialize");
        assert!(matches!(acp, AcpError::ProtocolParseError { .. }));
    }

    #[test]
    fn from_sdk_invalid_request_preserves_protocol_error() {
        let sdk_err = SdkError::invalid_request();
        let acp = AcpError::from_sdk(sdk_err, "session/new");
        assert!(matches!(acp, AcpError::InvalidRequest { .. }));
    }

    #[test]
    fn from_sdk_resource_not_found() {
        let sdk_err = SdkError::resource_not_found(Some("file:///missing.txt".to_owned()));
        let acp = AcpError::from_sdk(sdk_err, "session/new");
        match acp {
            AcpError::ResourceNotFound { resource, .. } => {
                assert_eq!(resource.as_deref(), Some("file:///missing.txt"));
            }
            other => panic!("Expected ResourceNotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_session_load_resource_not_found_without_uri_is_session_not_found() {
        let sdk_err = SdkError::resource_not_found(None);
        let acp = AcpError::from_sdk(sdk_err, "session/load");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "session/load"),
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_prompt_resource_not_found_without_uri_stays_resource_not_found() {
        let sdk_err = SdkError::resource_not_found(None);
        let acp = AcpError::from_sdk(sdk_err, "session/prompt");
        assert!(
            matches!(acp, AcpError::ResourceNotFound { resource: None, .. }),
            "prompt ResourceNotFound must not clear a persisted session id: {acp:?}"
        );
    }

    #[test]
    fn from_sdk_method_not_found() {
        let sdk_err = SdkError::method_not_found();
        let acp = AcpError::from_sdk(sdk_err, "session/magic");
        match acp {
            AcpError::MethodNotFound { method } => assert_eq!(method, "session/magic"),
            other => panic!("Expected MethodNotFound, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_invalid_params() {
        let sdk_err = SdkError::invalid_params();
        let acp = AcpError::from_sdk(sdk_err, "ignored");
        assert!(matches!(acp, AcpError::InvalidParams { .. }));
    }

    /// OpenCode reports a stale session as
    /// `code: -32602 InvalidParams` with the real reason wrapped in a
    /// JSON-string `data` payload (see ELECTRON-1HQ wire dump). Re-classify
    /// to `SessionNotFound` so downstream crash detection /
    /// recovery treats it uniformly with agents that return -32600 / -32001.
    #[test]
    fn from_sdk_invalid_params_with_session_not_found_data() {
        let sdk_err = SdkError::invalid_params().data(serde_json::Value::String(
            r#"{"error":"Session not found: ses_21859c95dffefejNiDf1VYXMgU"}"#.to_owned(),
        ));
        let acp = AcpError::from_sdk(sdk_err, "session/set_mode");
        match acp {
            AcpError::SessionNotFound { session_id } => {
                assert_eq!(session_id, "ses_21859c95dffefejNiDf1VYXMgU");
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Object-shaped data should also be recognised — some agents skip the
    /// extra string-encoding round-trip and emit a JSON object directly.
    #[test]
    fn from_sdk_invalid_params_with_object_data_session_not_found() {
        let sdk_err = SdkError::invalid_params().data(serde_json::json!({
            "error": "Session not found: sess-direct"
        }));
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "sess-direct"),
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Internal-error code carrying the same payload should also re-classify;
    /// don't tie the rescue to any single `ErrorCode`.
    #[test]
    fn from_sdk_internal_with_session_not_found_data() {
        let sdk_err = SdkError::internal_error().data(serde_json::json!({
            "error": "Session not found: sess-ie"
        }));
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "sess-ie"),
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// glm-acp-agent puts the reason under `details`, not `error`. Keying the
    /// rescue on `error` alone left this shape as a bare `AgentInternal`, so
    /// `open_session_resume` never rebuilt the session and the stale id was
    /// replayed forever. Payload copied verbatim from the live wire dump
    /// (conversation `66c25606`, `session/load`, 2026-08-17T09:53:31Z).
    #[test]
    fn from_sdk_internal_with_session_not_found_under_a_details_key() {
        let sdk_err = SdkError::internal_error().data(serde_json::json!({
            "details": "Session not found: deb7c49d-914b-4581-910e-703f550ce131"
        }));
        let acp = AcpError::from_sdk(sdk_err, "session/load");
        match acp {
            AcpError::SessionNotFound { session_id } => {
                assert_eq!(session_id, "deb7c49d-914b-4581-910e-703f550ce131");
            }
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// The rescue keys on the value, so an agent that invents yet another key
    /// still recovers. This is the property that makes the fix general rather
    /// than a growing list of key names.
    #[test]
    fn from_sdk_session_not_found_is_found_under_an_unknown_key() {
        let sdk_err = SdkError::internal_error().data(serde_json::json!({
            "reason": "Session not found: sess-unknown-key"
        }));
        let acp = AcpError::from_sdk(sdk_err, "session/load");
        match acp {
            AcpError::SessionNotFound { session_id } => assert_eq!(session_id, "sess-unknown-key"),
            other => panic!("expected SessionNotFound, got {other:?}"),
        }
    }

    /// Unrelated `data` payloads must not trigger the rescue path —
    /// otherwise we'd silently rewrite `InvalidParams` for genuinely
    /// malformed requests.
    #[test]
    fn from_sdk_invalid_params_with_unrelated_data_stays_invalid_params() {
        let sdk_err = SdkError::invalid_params().data(serde_json::json!({
            "error": "Workspace path must be absolute"
        }));
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        assert!(matches!(acp, AcpError::InvalidParams { .. }));
    }

    #[test]
    fn from_sdk_internal_error() {
        let sdk_err = SdkError::internal_error();
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::AgentInternal { code, .. } => assert_eq!(code, -32603),
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_other_code_session_related() {
        let sdk_err = SdkError::new(-32001, "session expired");
        let acp = AcpError::from_sdk(sdk_err, "sess-old");
        assert!(matches!(acp, AcpError::SessionNotFound { .. }));
    }

    #[test]
    fn from_sdk_other_code_unknown() {
        let sdk_err = SdkError::new(-32099, "custom error");
        let acp = AcpError::from_sdk(sdk_err, "ctx");
        match acp {
            AcpError::OtherProtocolError { code, message, .. } => {
                assert_eq!(code, -32099);
                assert_eq!(message, "custom error");
            }
            other => panic!("Expected OtherProtocolError, got {other:?}"),
        }
    }

    #[test]
    fn display_does_not_contain_stderr() {
        let err = AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "SUPER SECRET API KEY abc123".into(),
        };
        let display = err.to_string();
        assert!(
            !display.contains("SUPER SECRET"),
            "Display should not leak stderr: {display}"
        );
    }

    #[test]
    fn startup_crash_display_includes_exit_code() {
        let err = AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: String::new(),
        };
        let display = err.to_string();
        assert!(display.contains("exit code 1"), "got {display}");
        assert!(
            display.contains("before initialize handshake"),
            "must explain when in lifecycle the crash happened; got {display}"
        );
    }

    #[test]
    fn startup_crash_display_omits_detail_when_unknown() {
        let err = AcpError::StartupCrash {
            exit_code: None,
            signal: None,
            stderr: String::new(),
        };
        let display = err.to_string();
        assert!(!display.contains("None"), "must not surface raw `None`; got {display}");
        assert!(!display.contains("()"), "must not produce empty parens; got {display}");
    }

    #[test]
    fn disconnected_display_includes_signal_when_present() {
        let err = AcpError::Disconnected {
            exit_code: None,
            signal: Some("signal:9".into()),
            stderr: String::new(),
        };
        let display = err.to_string();
        assert!(display.contains("signal:9"), "got {display}");
    }

    #[test]
    fn from_sdk_captures_data_payload() {
        let sdk_err = SdkError::internal_error().data(serde_json::json!({"reason": "rate_limited", "retry_after": 30}));
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::AgentInternal { code, message, data } => {
                assert_eq!(code, -32603);
                assert_eq!(message, "Internal error");
                let data = data.expect("data must be preserved");
                assert_eq!(data["reason"], "rate_limited");
                assert_eq!(data["retry_after"], 30);
            }
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn other_protocol_error_preserves_code_and_data() {
        let sdk_err = SdkError::new(-32099, "custom upstream error")
            .data(serde_json::json!({"reason": "rate_limited", "retry_after": 30}));
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::OtherProtocolError { code, message, data } => {
                assert_eq!(code, -32099);
                assert_eq!(message, "custom upstream error");
                let data = data.expect("data must be preserved");
                assert_eq!(data["reason"], "rate_limited");
                assert_eq!(data["retry_after"], 30);
            }
            other => panic!("Expected OtherProtocolError, got {other:?}"),
        }
    }

    #[test]
    fn from_sdk_no_data_yields_none() {
        let sdk_err = SdkError::internal_error();
        let acp = AcpError::from_sdk(sdk_err, "context");
        match acp {
            AcpError::AgentInternal { data, .. } => assert!(data.is_none()),
            other => panic!("Expected AgentInternal, got {other:?}"),
        }
    }

    #[test]
    fn agent_internal_display_uses_message_only_when_no_data() {
        let err = AcpError::AgentInternal {
            message: "API Error: Internal server error".into(),
            code: -32603,
            data: None,
        };
        assert_eq!(
            err.to_string(),
            "API Error: Internal server error",
            "Display must NOT prefix with 'Agent internal error:' when message carries upstream context"
        );
    }

    #[test]
    fn agent_internal_display_falls_back_when_message_is_sdk_default() {
        // SDK default for ErrorCode::InternalError is the plain string "Internal error".
        // When that's all we have, the user sees nothing useful, so add a hint.
        let err = AcpError::AgentInternal {
            message: "Internal error".into(),
            code: -32603,
            data: None,
        };
        let display = err.to_string();
        assert!(
            display.contains("Agent internal error"),
            "Display must include 'Agent internal error' hint when SDK gave us its default message; got {display}"
        );
        assert!(
            display.contains("-32603"),
            "Display must include the JSON-RPC code as a diagnostic when message is empty/default; got {display}"
        );
    }

    #[test]
    fn agent_internal_display_appends_data_when_message_is_sdk_default() {
        // Real-world shape: SDK returned its default `"Internal error"` but
        // attached structured data. Display must use the diagnostic header
        // AND append the data.
        let err = AcpError::AgentInternal {
            message: "Internal error".into(),
            code: -32603,
            data: Some(serde_json::json!({"retry_after": 30})),
        };
        let display = err.to_string();
        assert!(
            display.contains("Agent internal error"),
            "header must use diagnostic fallback when message is the SDK default; got {display}"
        );
        assert!(
            display.contains("-32603"),
            "header must include the code; got {display}"
        );
        assert!(display.contains("retry_after"), "data must be appended; got {display}");
        assert!(display.contains("30"), "data value must be appended; got {display}");
        assert!(!display.contains('\n'), "data must be inline; got {display}");
    }

    #[test]
    fn agent_internal_display_appends_data_inline() {
        let err = AcpError::AgentInternal {
            message: "API Error".into(),
            code: -32603,
            data: Some(serde_json::json!({"upstream_status": 503})),
        };
        let display = err.to_string();
        assert!(display.contains("API Error"), "got {display}");
        assert!(display.contains("upstream_status"), "got {display}");
        assert!(display.contains("503"), "got {display}");
        assert!(
            !display.contains('\n'),
            "data must be appended on a single line, not pretty-printed; got {display}"
        );
    }
}

#[cfg(test)]
mod application_error_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aelix_busy_is_distinct_from_standard_acp_auth_required() {
        // Verified: schema-1.5.0/src/v1/error.rs and aelix-acp@16d6555/server.py.
        for agent in [None, Some("another-agent")] {
            assert!(matches!(
                AcpError::from_sdk_for_agent(
                    SdkError::new(-32000, "Authentication required"),
                    "session/prompt",
                    agent
                ),
                AcpError::AuthRequired
            ));
        }
        let data = json!({"fixture":true});
        let busy = AcpError::from_sdk_for_agent(
            SdkError::new(-32000, "busy").data(data.clone()),
            "session/load",
            Some("aelix-acp"),
        );
        assert!(matches!(busy, AcpError::ApplicationError {code:-32000,data:Some(ref value),..} if value == &data));
        assert!(!busy.is_retryable());
    }

    #[test]
    fn structured_application_errors_win_over_session_not_found_heuristics_and_transport_loss() {
        for code in [-33001, -33002, -32004] {
            let data = json!({"profile":"Session not found: authored-profile","retryable":false});
            let error = AcpError::from_sdk_for_agent(
                SdkError::new(code, "refused").data(data.clone()),
                "session/load",
                Some("aelix-acp"),
            )
            .after_prompt_dispatch(true);
            assert!(
                matches!(error,AcpError::ApplicationError {code:actual,data:Some(ref value),..} if actual == code && value == &data)
            );
            assert!(!error.is_retryable());
        }
    }
}
