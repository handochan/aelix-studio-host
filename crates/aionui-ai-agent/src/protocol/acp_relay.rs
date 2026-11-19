//! Host-side ACP session relay. Feature payloads stay opaque at this boundary.
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeResponse, Meta, RequestId, SessionId, SessionNotification, SessionUpdate,
};
use aionui_api_types::{AcpExtensionDiscovery, AcpExtensionRequest, AgentMetadata, AgentSource};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use tokio::sync::{oneshot, watch};

use crate::protocol::events::AgentStreamEvent;

pub const DEADLINE: Duration = Duration::from_secs(120);
pub(crate) const META_KEY: &str = "aelix/sessionRelay";
const METHOD_PREFIX: &str = "_aelix/session/";
const EXT_FRAME_LIMIT: usize = 256 * 1024;
// The pinned Preview load replay measured 16 MiB + 58 bytes. Round up
// to 32 MiB for standard framing headroom in both directions.
pub(crate) const STDIO_LINE_LIMIT: usize = 32 * 1024 * 1024;

pub(crate) fn stdio_codec(candidate: bool) -> tokio_util::codec::LinesCodec {
    if candidate {
        // LinesCodec includes the CR before removing the CRLF delimiter.
        tokio_util::codec::LinesCodec::new_with_max_length(STDIO_LINE_LIMIT + 1)
    } else {
        tokio_util::codec::LinesCodec::new()
    }
}

pub(crate) fn check_stdio_line(line: String) -> Result<String, tokio_util::codec::LinesCodecError> {
    if line.len() > STDIO_LINE_LIMIT {
        Err(tokio_util::codec::LinesCodecError::MaxLineLengthExceeded)
    } else {
        Ok(line)
    }
}

pub(crate) fn is_builtin_aelix(metadata: &AgentMetadata) -> bool {
    metadata.agent_source == AgentSource::Builtin && metadata.backend.as_deref() == Some("aelix")
}

pub fn valid_method(method: &str) -> bool {
    method.len() <= 96
        && method.strip_prefix(METHOD_PREFIX).is_some_and(|path| {
            path.split('/').all(|part| {
                let mut bytes = part.bytes();
                bytes.next().is_some_and(|c| c.is_ascii_lowercase())
                    && bytes.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            })
        })
}

#[derive(Deserialize)]
struct Advertisement {
    version: u32,
    requests: Vec<String>,
    notifications: Vec<String>,
}

fn advertisement(response: &InitializeResponse) -> Option<Vec<String>> {
    if response.agent_info.as_ref()?.name != "aelix-acp" {
        return None;
    }
    let raw = response.agent_capabilities.meta.as_ref()?.get(META_KEY)?;
    if serde_json::to_vec(raw).ok()?.len() > 8 * 1024 {
        return None;
    }
    let ad: Advertisement = serde_json::from_value(raw.clone()).ok()?;
    let unique = ad.requests.iter().collect::<HashSet<_>>().len() == ad.requests.len();
    (ad.version == 1
        && ad.notifications.is_empty()
        && ad.requests.len() <= 32
        && unique
        && ad.requests.iter().all(|method| valid_method(method)))
    .then_some(ad.requests)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayError {
    NoLiveTask,
    Unavailable,
    StaleBinding,
    MethodDenied,
    RateLimited,
    TransportUnknown,
    BindingUnknown,
    DeadlineUnknown,
    Agent(i64),
}

pub(crate) struct RelayConnection {
    pub(crate) candidate: bool,
    requests: OnceLock<Vec<String>>,
    session: Mutex<RelaySession>,
    wire: Mutex<HashMap<RequestId, WireRequest>>,
}

struct RelaySession {
    open: bool,
    open_generation: u64,
    binding: Option<(SessionId, String)>,
    binding_generation: watch::Sender<Option<RelayError>>,
    prompt: Option<ActivePrompt>,
}

pub(crate) struct OpenAttempt {
    generation: u64,
    binding_id: Option<String>,
}

impl RelaySession {
    fn close(&mut self, reason: RelayError) {
        if self.binding.take().is_some() {
            self.binding_generation.send_replace(Some(reason));
            tracing::info!("ACP session extension binding closed");
        }
        self.open_generation += 1;
        self.open = false;
        self.prompt = None;
    }
}

struct ActivePrompt {
    turn_id: String,
    seen: HashSet<String>,
    request_id: RequestId,
    written: bool,
}

struct WireRequest {
    params: Option<Value>,
    reply: oneshot::Sender<Result<Value, RelayError>>,
}

/// The raw actor line is intercepted outside the ACP SDK's payload tracing.
#[derive(Deserialize)]
struct FrameHeader<'a> {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default, borrow)]
    method: Option<&'a RawValue>,
}

impl FrameHeader<'_> {
    fn is_relay_method(&self) -> bool {
        self.method
            .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
            .is_some_and(|method| method.starts_with(METHOD_PREFIX))
    }
}

pub struct RelayCall {
    pub reply: oneshot::Receiver<Result<Value, RelayError>>,
    pub binding_generation: watch::Receiver<Option<RelayError>>,
}

impl RelayConnection {
    pub(crate) fn new(candidate: bool) -> Self {
        Self {
            candidate,
            requests: OnceLock::new(),
            session: Mutex::new(RelaySession {
                open: false,
                open_generation: 0,
                binding: None,
                binding_generation: watch::channel(None).0,
                prompt: None,
            }),
            wire: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn negotiate(&self, response: &InitializeResponse) {
        if self.candidate
            && let Some(requests) = advertisement(response)
        {
            let _ = self.requests.set(requests);
            tracing::info!("ACP session extension requests negotiated");
        }
    }

    pub(crate) fn prepare_open(&self, meta: &mut Option<Meta>) -> OpenAttempt {
        let mut session = self.session.lock().unwrap();
        session.close(RelayError::BindingUnknown);
        let id = self.requests.get().map(|_| {
            let id = uuid::Uuid::new_v4().to_string();
            meta.get_or_insert_with(Meta::new)
                .insert(META_KEY.into(), json!({ "bindingId": id }));
            id
        });
        OpenAttempt {
            generation: session.open_generation,
            binding_id: id,
        }
    }

    pub(crate) fn opened(&self, session_id: SessionId, attempt: OpenAttempt, meta: Option<&Meta>) {
        let echo = meta
            .and_then(|m| m.get(META_KEY))
            .and_then(|v| v.get("bindingId"))
            .and_then(Value::as_str);
        let mut session = self.session.lock().unwrap();
        if session.open_generation != attempt.generation {
            return;
        }
        session.open = true;
        if let Some(id) = attempt.binding_id {
            if echo == Some(id.as_str()) {
                session.binding_generation = watch::channel(None).0;
                session.binding = Some((session_id, id));
                tracing::info!("ACP session extension binding opened");
            } else {
                tracing::warn!("ACP session extension binding echo did not match");
            }
        }
    }

    pub(crate) fn invalidate(&self) {
        self.session.lock().unwrap().close(RelayError::BindingUnknown);
    }

    pub(crate) fn discovery(&self) -> Result<AcpExtensionDiscovery, RelayError> {
        let session = self.session.lock().unwrap();
        if !session.open {
            return Err(RelayError::NoLiveTask);
        }
        let requests = self.requests.get().ok_or(RelayError::Unavailable)?;
        Ok(AcpExtensionDiscovery {
            version: 1,
            binding_id: session.binding.as_ref().ok_or(RelayError::Unavailable)?.1.clone(),
            requests: requests.clone(),
            notifications: Vec::new(),
        })
    }

    pub(crate) fn dispatch(
        &self,
        request: AcpExtensionRequest,
        send: impl FnOnce(&str) -> RequestId,
    ) -> Result<RelayCall, RelayError> {
        let session = self.session.lock().unwrap();
        let (session_id, binding_id) = session.binding.as_ref().ok_or(RelayError::StaleBinding)?;
        if binding_id != &request.binding_id {
            return Err(RelayError::StaleBinding);
        }
        if !valid_method(&request.method)
            || !self
                .requests
                .get()
                .is_some_and(|methods| methods.contains(&request.method))
        {
            return Err(RelayError::MethodDenied);
        }
        let params = json!({ "sessionId": session_id, "bindingId": binding_id, "payload": request.payload });
        let (reply, receiver) = oneshot::channel();
        let mut wire = self.wire.lock().unwrap();
        // The transport sink takes the same lock before it writes the SDK frame.
        let id = send(&request.method);
        wire.insert(
            id,
            WireRequest {
                params: Some(params),
                reply,
            },
        );
        Ok(RelayCall {
            reply: receiver,
            binding_generation: session.binding_generation.subscribe(),
        })
    }

    pub(crate) fn outgoing(&self, line: String) -> std::io::Result<String> {
        if self.candidate && line.contains("\"session/prompt\"") {
            // Wait for the ordered reply callback to be installed before the
            // peer can see this request, then arm receipt for this prompt.
            let mut session = self.session.lock().unwrap();
            if let Some(prompt) = session.prompt.as_mut()
                && let Ok(header) = serde_json::from_str::<FrameHeader>(&line)
                && header.id.as_ref() == Some(&prompt.request_id)
                && header.method.is_some_and(|method| method.get() == "\"session/prompt\"")
            {
                prompt.written = true;
            }
        }
        if !line.contains(METHOD_PREFIX) {
            return Ok(line);
        }
        let Ok(header) = serde_json::from_str::<FrameHeader>(&line) else {
            return Ok(line);
        };
        if !header.is_relay_method() {
            return Ok(line);
        }
        let Some(id) = header.id else {
            return Ok(line);
        };
        let params = self
            .wire
            .lock()
            .unwrap()
            .get_mut(&id)
            .and_then(|request| request.params.take());
        let Some(params) = params else {
            return Ok(line);
        };
        let method: String = serde_json::from_str(header.method.unwrap().get()).map_err(std::io::Error::other)?;
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .map_err(std::io::Error::other)
    }

    pub(crate) fn incoming(&self, line: String) -> Option<String> {
        let frame_bytes = line.len();
        let Ok(header) = serde_json::from_str::<FrameHeader>(&line) else {
            // ACP stdio uses one JSON object per line. Never pass a batch or
            // malformed raw line into the SDK's tracing or parse-error data.
            if line.trim().is_empty() {
                return None;
            }
            return Some("{}".into());
        };
        if header.is_relay_method() {
            // Request-only negotiation. Ignore notifications and give reverse
            // requests the SDK's method-not-found path with an empty body.
            return header.id.map(|id| {
                json!({"jsonrpc":"2.0","id":id,"method":"_aelix/session/unsupported","params":{}}).to_string()
            });
        }
        if header.method.is_some() {
            return Some(line);
        }
        let Some(id) = header.id else {
            return Some(line);
        };
        let Some(request) = self.wire.lock().unwrap().remove(&id) else {
            return Some(line);
        };
        let reply = if frame_bytes <= EXT_FRAME_LIMIT {
            decode_reply(&line)
        } else {
            Err(RelayError::TransportUnknown)
        };
        let _ = request.reply.send(reply);
        // Resolve the SDK's correlation slot without sending agent data into
        // its tracing, error mapping, or serde diagnostics.
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": null }).to_string())
    }

    pub(crate) fn discard_request(&self, id: &RequestId) {
        self.wire.lock().unwrap().remove(id);
    }

    pub(crate) fn disconnected(&self) {
        self.session.lock().unwrap().close(RelayError::TransportUnknown);
        self.wire.lock().unwrap().clear();
    }

    pub(crate) fn begin_prompt<T>(&self, sid: &SessionId, turn_id: &str, send: impl FnOnce() -> (RequestId, T)) -> T {
        let mut session = self.session.lock().unwrap();
        let (request_id, result) = send();
        if session.binding.as_ref().is_some_and(|(bound, _)| bound == sid) {
            session.prompt = Some(ActivePrompt {
                turn_id: turn_id.to_owned(),
                seen: HashSet::new(),
                request_id,
                written: false,
            });
        }
        result
    }

    pub(crate) fn end_prompt(&self, turn_id: &str) {
        let mut session = self.session.lock().unwrap();
        if session.prompt.as_ref().is_some_and(|prompt| prompt.turn_id == turn_id) {
            session.prompt = None;
        }
    }

    pub(crate) fn consumed_input(&self, notification: &SessionNotification) -> Option<AgentStreamEvent> {
        let SessionUpdate::UserMessageChunk(chunk) = &notification.update else {
            return None;
        };
        let ContentBlock::Text(text) = &chunk.content else {
            return None;
        };
        let id = &chunk.message_id.as_ref()?.0;
        if id.is_empty() || text.text.is_empty() {
            return None;
        }
        let mut session = self.session.lock().unwrap();
        if session.binding.as_ref()?.0 != notification.session_id {
            return None;
        }
        let prompt = session.prompt.as_mut().filter(|prompt| prompt.written)?;
        prompt
            .seen
            .insert(id.to_string())
            .then(|| AgentStreamEvent::AcpConsumedUserInput {
                turn_id: prompt.turn_id.clone(),
                text: text.text.clone(),
            })
    }
}

fn decode_reply(line: &str) -> Result<Value, RelayError> {
    if !valid_json_bounds(line.as_bytes()) {
        return Err(RelayError::TransportUnknown);
    }
    let frame: Value = serde_json::from_str(line).map_err(|_| RelayError::TransportUnknown)?;
    if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(RelayError::TransportUnknown);
    }
    match (frame.get("result"), frame.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => match error.get("code").and_then(Value::as_i64) {
            Some(-33100) | None => Err(RelayError::TransportUnknown),
            Some(code) => Err(RelayError::Agent(code)),
        },
        _ => Err(RelayError::TransportUnknown),
    }
}

/// Bound nesting and check integer literals before serde converts very large
/// integers into floating-point values. Serde validates the JSON syntax.
pub fn valid_json_bounds(json: &[u8]) -> bool {
    let mut depth = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut cursor = 0;
    while let Some(&byte) = json.get(cursor) {
        cursor += 1;
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'[' | b'{' => {
                depth += 1;
                if depth > 32 {
                    return false;
                }
            }
            b']' | b'}' => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            b'-' | b'0'..=b'9' => {
                let first = cursor - 1;
                while json
                    .get(cursor)
                    .is_some_and(|c| matches!(c, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-'))
                {
                    cursor += 1;
                }
                let number = &json[first..cursor];
                if !number.iter().any(|c| matches!(c, b'.' | b'e' | b'E'))
                    && std::str::from_utf8(number)
                        .ok()
                        .and_then(|n| n.parse::<i64>().ok())
                        .is_none_or(|n| n.unsigned_abs() >= (1 << 53))
                {
                    return false;
                }
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio_util::codec::FramedRead;

    #[tokio::test]
    async fn acp_relay_standard_stdio_line_limit_counts_bytes_without_delimiter() {
        let start = r#"{"jsonrpc":"2.0","id":9,"result":{"large":"한"#;
        let end = r#""}}"#;
        for (bytes, delimiter, accepted) in [
            (16 * 1024 * 1024 + 58, "\n", true),
            (STDIO_LINE_LIMIT - 1, "\n", true),
            (STDIO_LINE_LIMIT, "\r\n", true),
            (STDIO_LINE_LIMIT + 1, "\n", false),
            (STDIO_LINE_LIMIT + 1, "\r\n", false),
        ] {
            let mut frame = Vec::with_capacity(bytes + delimiter.len());
            frame.extend_from_slice(start.as_bytes());
            frame.resize(bytes - end.len(), b'x');
            frame.extend_from_slice(end.as_bytes());
            frame.extend_from_slice(delimiter.as_bytes());
            let mut reader = FramedRead::new(frame.as_slice(), stdio_codec(true));
            let received = reader
                .next()
                .await
                .expect("frame or size error")
                .and_then(check_stdio_line);
            assert_eq!(received.is_ok(), accepted);
            if let Ok(received) = received {
                assert_eq!(received.len(), bytes);
            }
        }
    }

    #[test]
    fn acp_relay_json_bounds_ignore_escaped_strings_and_reject_unsafe_integers() {
        assert!(valid_json_bounds(
            br#"{"text":"\\\"]}]","integer":9007199254740991,"real":1.2e47}"#
        ));
        for number in ["9007199254740992", "-9007199254740992", "99999999999999999999999"] {
            assert!(!valid_json_bounds(number.as_bytes()));
        }
        let nested = |depth| format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        assert!(valid_json_bounds(nested(32).as_bytes()));
        assert!(!valid_json_bounds(nested(33).as_bytes()));
    }
}
