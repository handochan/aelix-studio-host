//! Host-side ACP session relay. Feature payloads stay opaque at this boundary.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeResponse, Meta, RequestId, SessionId, SessionNotification, SessionUpdate,
};
use aionui_api_types::{AcpExtensionDiscovery, AcpExtensionRequest, AgentMetadata};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use tokio::sync::{oneshot, watch};

use crate::protocol::events::AgentStreamEvent;

use super::byte_grants::{self, ByteGrantBinding, ReadRequest};

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

pub(crate) fn is_builtin_aelix(metadata: &AgentMetadata, studio_launch: bool) -> bool {
    studio_launch
        && crate::studio_aelix::is_catalog_entry(metadata)
        && metadata.enabled
        && !metadata.has_command_override
        && metadata.env_override_key_count == 0
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
    AuthorizationRevoked,
    TransportUnknown,
    BindingUnknown,
    DeadlineUnknown,
    Agent(i64),
}

pub(crate) struct RelayConnection {
    pub(crate) candidate: bool,
    pub(super) offer_host_bytes: bool,
    requests: OnceLock<Vec<String>>,
    aelix_peer: AtomicBool,
    host_bytes: AtomicBool,
    session: Mutex<RelaySession>,
    wire: Mutex<HashMap<RequestId, WireRequest>>,
    standard_pending: Mutex<HashSet<RequestId>>,
    input_history: Arc<AcpInputHistory>,
}

struct RelaySession {
    open: bool,
    open_generation: u64,
    binding: Option<(SessionId, String)>,
    binding_generation: watch::Sender<Option<RelayError>>,
    byte_grants: Option<Arc<ByteGrantBinding>>,
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
        if let Some(grants) = self.byte_grants.take() {
            grants.close();
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
    receiving_on_wire: bool,
}

/// Checks carried from authenticated admission to the first stdio write.
type DatabaseFence = dyn Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<aionui_db::AcpRelayWriteFence, aionui_db::DbError>> + Send>>
    + Send
    + Sync;
type CommitFirstWrite = dyn Fn(&mut dyn FnMut()) + Send + Sync;
pub struct RelayWriteAdmission {
    pub browser: Option<aionui_common::WsSessionGuard>,
    pub deadline: tokio::time::Instant,
    commit: Box<CommitFirstWrite>,
    database_fence: Option<Box<DatabaseFence>>,
}

impl RelayWriteAdmission {
    pub fn new(
        browser: Option<aionui_common::WsSessionGuard>,
        deadline: tokio::time::Instant,
        commit: impl Fn(&mut dyn FnMut()) + Send + Sync + 'static,
    ) -> Self {
        Self {
            browser,
            deadline,
            commit: Box::new(commit),
            database_fence: None,
        }
    }

    pub fn with_database_fence<F, Fut>(mut self, acquire: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<aionui_db::AcpRelayWriteFence, aionui_db::DbError>> + Send + 'static,
    {
        self.database_fence = Some(Box::new(move || Box::pin(acquire())));
        self
    }

    pub(super) async fn acquire(&self) -> Result<Option<Box<dyn aionui_db::AcpRelayWriteLock>>, RelayError> {
        if let Some(browser) = &self.browser
            && !browser.valid()
        {
            return Err(RelayError::AuthorizationRevoked);
        }
        let Some(fence) = &self.database_fence else {
            return Ok(None);
        };
        match fence().await {
            Ok(aionui_db::AcpRelayWriteFence::Granted(lock)) => Ok(Some(lock)),
            Ok(aionui_db::AcpRelayWriteFence::StaleConversation) => Err(RelayError::StaleBinding),
            Ok(aionui_db::AcpRelayWriteFence::SessionRevoked) => Err(RelayError::AuthorizationRevoked),
            Err(_) => Err(RelayError::Unavailable),
        }
    }

    pub(super) fn commit(&self, write: &mut dyn FnMut()) {
        (self.commit)(write);
    }
}

#[derive(Default)]
pub struct AcpInputHistory {
    latest: Mutex<Option<(String, usize)>>,
}

impl AcpInputHistory {
    pub fn received(&self, turn_id: &str) -> usize {
        self.latest
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(turn, _)| turn == turn_id)
            .map_or(0, |(_, count)| *count)
    }
}

pub(super) enum IncomingFrame {
    Sdk(String),
    Reverse(ReverseRequest),
}

pub(super) struct ReverseRequest {
    pub id: String,
    pub params: Option<ReadRequest>,
    pub known_method: bool,
}

pub(super) struct RelayFrame {
    pub id: RequestId,
    pub line: String,
    pub generation: u64,
    pub admission: RelayWriteAdmission,
}

pub(super) struct StandardFrame {
    pub line: String,
    pub prompt_id: Option<RequestId>,
}

impl StandardFrame {
    fn plain(line: String) -> Self {
        Self { line, prompt_id: None }
    }
}

pub(super) enum OutgoingFrame {
    Standard(StandardFrame),
    Relay(RelayFrame),
    Drop,
}

struct WireRequest {
    frame: Option<(Value, u64, RelayWriteAdmission)>,
    started: Arc<AtomicBool>,
    reply: oneshot::Sender<Result<Value, RelayError>>,
}

/// The raw actor line is intercepted outside the ACP SDK's payload tracing.
#[derive(Deserialize)]
struct FrameHeader<'a> {
    #[serde(default)]
    id: Option<RequestId>,
    #[serde(default, borrow)]
    method: Option<&'a RawValue>,
    #[serde(default, borrow)]
    params: Option<&'a RawValue>,
}

impl FrameHeader<'_> {
    fn method_name(&self) -> Option<String> {
        self.method.and_then(|raw| serde_json::from_str(raw.get()).ok())
    }
}

#[derive(Deserialize)]
struct UpdateHeader {
    update: UpdateKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateKind {
    session_update: String,
}

pub struct RelayCall {
    pub reply: oneshot::Receiver<Result<Value, RelayError>>,
    pub binding_generation: watch::Receiver<Option<RelayError>>,
    pub started: Arc<AtomicBool>,
}

impl RelayConnection {
    pub(crate) fn new(candidate: bool, standalone: bool) -> Self {
        Self {
            candidate,
            offer_host_bytes: candidate && standalone,
            requests: OnceLock::new(),
            aelix_peer: AtomicBool::new(false),
            host_bytes: AtomicBool::new(false),
            session: Mutex::new(RelaySession {
                open: false,
                open_generation: 0,
                binding: None,
                binding_generation: watch::channel(None).0,
                prompt: None,
                byte_grants: None,
            }),
            wire: Mutex::new(HashMap::new()),
            standard_pending: Mutex::new(HashSet::new()),
            input_history: Arc::default(),
        }
    }

    pub(crate) fn negotiate(&self, response: &InitializeResponse) {
        self.aelix_peer.store(
            response
                .agent_info
                .as_ref()
                .is_some_and(|info| info.name == "aelix-acp"),
            Ordering::Release,
        );
        if self.candidate
            && let Some(requests) = advertisement(response)
        {
            let _ = self.requests.set(requests);
            self.host_bytes.store(
                self.offer_host_bytes
                    && response
                        .agent_capabilities
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.get(byte_grants::META_KEY))
                        .is_some_and(|offer| offer == &json!({"version": 1})),
                Ordering::Release,
            );
            tracing::info!("ACP session extension requests negotiated");
        }
    }

    pub(super) fn is_aelix_peer(&self) -> bool {
        self.aelix_peer.load(Ordering::Acquire)
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
                if self.host_bytes.load(Ordering::Acquire) {
                    session.byte_grants = Some(Arc::default());
                }
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
            host_bytes: session.byte_grants.is_some(),
        })
    }

    pub(crate) fn byte_grants(&self, binding_id: &str) -> Result<Arc<ByteGrantBinding>, RelayError> {
        let session = self.session.lock().unwrap();
        let (_, current) = session.binding.as_ref().ok_or(RelayError::Unavailable)?;
        if binding_id != current {
            return Err(RelayError::StaleBinding);
        }
        session.byte_grants.clone().ok_or(RelayError::Unavailable)
    }

    fn reverse_request(line: &str, method: &str, id: RequestId) -> Option<ReverseRequest> {
        let id = serde_json::to_string(&id).ok()?;
        if id.len() > 64 * 1024 {
            return None;
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Envelope<'a> {
            jsonrpc: &'a str,
            #[serde(rename = "id")]
            _id: &'a RawValue,
            #[serde(rename = "method")]
            _method: &'a RawValue,
            #[serde(borrow)]
            params: &'a RawValue,
        }
        let known_method = method == byte_grants::METHOD;
        let params = if known_method && id.len() <= byte_grants::MAX_ID_BYTES && line.len() <= 64 * 1024 {
            serde_json::from_str::<Envelope>(line)
                .ok()
                .filter(|envelope| envelope.jsonrpc == "2.0")
                .and_then(|envelope| ReadRequest::parse(envelope.params))
        } else {
            None
        };
        Some(ReverseRequest {
            id,
            params,
            known_method,
        })
    }

    pub(super) fn negotiated_byte_source(&self) -> bool {
        self.host_bytes.load(Ordering::Acquire)
    }

    pub(super) fn reverse_binding(&self, request: &ReadRequest) -> Option<Arc<ByteGrantBinding>> {
        let session = self.session.lock().unwrap();
        let (session_id, binding_id) = session.binding.as_ref()?;
        if session_id.0.as_ref() != request.session_id || *binding_id != request.binding_id {
            return None;
        }
        session.byte_grants.clone()
    }

    pub(crate) fn dispatch(
        &self,
        request: AcpExtensionRequest,
        admission: RelayWriteAdmission,
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
        let started = Arc::new(AtomicBool::new(false));
        let mut wire = self.wire.lock().unwrap();
        // The transport sink takes the same lock before it writes the SDK frame.
        let id = send(&request.method);
        wire.insert(
            id,
            WireRequest {
                frame: Some((params, session.open_generation, admission)),
                started: started.clone(),
                reply,
            },
        );
        Ok(RelayCall {
            reply: receiver,
            binding_generation: session.binding_generation.subscribe(),
            started,
        })
    }

    pub(super) fn outgoing(&self, line: String) -> std::io::Result<OutgoingFrame> {
        if !self.candidate {
            return Ok(OutgoingFrame::Standard(StandardFrame::plain(line)));
        }
        let Ok(header) = serde_json::from_str::<FrameHeader>(&line) else {
            return Ok(OutgoingFrame::Standard(StandardFrame::plain(line)));
        };
        let (Some(id), Some(method)) = (header.id.as_ref(), header.method_name()) else {
            return Ok(OutgoingFrame::Standard(StandardFrame::plain(line)));
        };
        if method.starts_with(METHOD_PREFIX) {
            let Some((params, generation, admission)) = self
                .wire
                .lock()
                .unwrap()
                .get_mut(id)
                .and_then(|request| request.frame.take())
            else {
                return Ok(OutgoingFrame::Drop);
            };
            let line =
                serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                    .map_err(std::io::Error::other)?;
            return Ok(OutgoingFrame::Relay(RelayFrame {
                id: id.clone(),
                line,
                generation,
                admission,
            }));
        }
        self.standard_pending.lock().unwrap().insert(id.clone());
        Ok(OutgoingFrame::Standard(StandardFrame {
            line,
            prompt_id: (method == "session/prompt").then(|| id.clone()),
        }))
    }

    pub(super) fn write_prompt_delimiter(
        &self,
        id: &RequestId,
        write: impl FnOnce() -> std::task::Poll<std::io::Result<usize>>,
    ) -> std::task::Poll<std::io::Result<usize>> {
        // The peer can parse the new prompt as soon as the newline reaches
        // the pipe. Order that physical write with our stdout admission,
        // including when the child responds on another thread immediately.
        let mut session = self.session.lock().unwrap();
        let result = write();
        if matches!(result, std::task::Poll::Ready(Ok(1)))
            && let Some(prompt) = session.prompt.as_mut()
            && &prompt.request_id == id
        {
            prompt.written = true;
            prompt.receiving_on_wire = true;
        }
        result
    }

    pub(super) fn first_write(
        &self,
        id: &RequestId,
        generation: u64,
        write: impl FnOnce() -> std::task::Poll<std::io::Result<usize>>,
    ) -> Option<std::task::Poll<std::io::Result<usize>>> {
        let session = self.session.lock().unwrap();
        if session.open_generation != generation || session.binding.is_none() {
            return None;
        }
        let mut wire = self.wire.lock().unwrap();
        let request = wire.get_mut(id)?;
        let result = write();
        if matches!(result, std::task::Poll::Ready(Ok(bytes)) if bytes > 0) {
            request.started.store(true, Ordering::Release);
        }
        Some(result)
    }

    pub(super) fn reject_unsent(&self, id: &RequestId, error: RelayError) -> Option<String> {
        let request = self.wire.lock().unwrap().remove(id)?;
        let _ = request.reply.send(Err(error));
        Some(sdk_ack(id))
    }

    pub(crate) fn input_history(&self) -> Arc<AcpInputHistory> {
        self.input_history.clone()
    }

    pub(super) fn classify_incoming(&self, line: String) -> Option<IncomingFrame> {
        let input_on_wire = self
            .session
            .lock()
            .unwrap()
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.receiving_on_wire);
        let frame_bytes = line.len();
        let Ok(header) = serde_json::from_str::<FrameHeader>(&line) else {
            // ACP stdio uses one JSON object per line. Never pass a batch or
            // malformed raw line into the SDK's tracing or parse-error data.
            if line.trim().is_empty() {
                return None;
            }
            return Some(IncomingFrame::Sdk("{}".into()));
        };
        let method = header.method_name();
        if let Some(name) = method.as_deref()
            && name.starts_with("_aelix/host/")
        {
            return Self::reverse_request(&line, name, header.id?).map(IncomingFrame::Reverse);
        }
        if method.as_deref().is_some_and(|name| name.starts_with(METHOD_PREFIX)) {
            // Request-only negotiation. Ignore notifications and give reverse
            // requests the SDK's method-not-found path with an empty body.
            return header.id.map(|id| {
                IncomingFrame::Sdk(
                    json!({"jsonrpc":"2.0","id":id,"method":"_aelix/session/unsupported","params":{}}).to_string(),
                )
            });
        }
        if header.method.is_some() {
            if method.as_deref() == Some("session/update") {
                // The SDK first parses into a JSON object and collapses
                // duplicate keys. Require an unambiguous update header before
                // forwarding so both stages classify user input consistently.
                let params = header.params?;
                let UpdateHeader { update } = serde_json::from_str(params.get()).ok()?;
                if update.session_update == "user_message_chunk" && !input_on_wire {
                    return None;
                }
            }
            return Some(IncomingFrame::Sdk(line));
        }
        let Some(id) = header.id else {
            return Some(IncomingFrame::Sdk(line));
        };
        let request = {
            let mut wire = self.wire.lock().unwrap();
            match wire.get(&id) {
                Some(request) if !request.started.load(Ordering::Acquire) => return None,
                Some(_) => wire.remove(&id).unwrap(),
                // Only responses to standard requests actually written on this
                // transport may reach SDK tracing. This also drops duplicates
                // after the extension's correlation entry has been reclaimed.
                None => {
                    drop(wire);
                    if !self.standard_pending.lock().unwrap().remove(&id) {
                        return None;
                    }
                    let mut session = self.session.lock().unwrap();
                    if let Some(prompt) = session.prompt.as_mut()
                        && prompt.request_id == id
                    {
                        // Keep written for updates that already entered the
                        // ordered SDK queue before this response.
                        prompt.receiving_on_wire = false;
                    }
                    return Some(IncomingFrame::Sdk(line));
                }
            }
        };
        let reply = if frame_bytes <= EXT_FRAME_LIMIT {
            decode_reply(&line)
        } else {
            Err(RelayError::TransportUnknown)
        };
        let _ = request.reply.send(reply);
        // Resolve the SDK's correlation slot without sending agent data into
        // its tracing, error mapping, or serde diagnostics.
        Some(IncomingFrame::Sdk(sdk_ack(&id)))
    }

    #[cfg(test)]
    pub(crate) fn incoming(&self, line: String) -> Option<String> {
        match self.classify_incoming(line)? {
            IncomingFrame::Sdk(line) => Some(line),
            IncomingFrame::Reverse(_) => None,
        }
    }

    pub(crate) fn discard_request(&self, id: &RequestId) {
        self.wire.lock().unwrap().remove(id);
    }

    pub(crate) fn disconnected(&self) {
        self.session.lock().unwrap().close(RelayError::TransportUnknown);
        self.wire.lock().unwrap().clear();
        self.standard_pending.lock().unwrap().clear();
    }

    pub(crate) fn begin_prompt<T>(&self, sid: &SessionId, turn_id: &str, send: impl FnOnce() -> (RequestId, T)) -> T {
        let mut session = self.session.lock().unwrap();
        let (request_id, result) = send();
        if session.binding.as_ref().is_some_and(|(bound, _)| bound == sid) {
            *self.input_history.latest.lock().unwrap() = Some((turn_id.to_owned(), 0));
            session.prompt = Some(ActivePrompt {
                turn_id: turn_id.to_owned(),
                seen: HashSet::new(),
                request_id,
                written: false,
                receiving_on_wire: false,
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
        if !prompt.seen.insert(id.to_string()) {
            return None;
        }
        if let Some((_, count)) = self.input_history.latest.lock().unwrap().as_mut() {
            *count += 1;
        }
        Some(AgentStreamEvent::AcpConsumedUserInput {
            turn_id: prompt.turn_id.clone(),
            text: text.text.clone(),
        })
    }
}

fn sdk_ack(id: &RequestId) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": null }).to_string()
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

    #[test]
    fn byte_grant_capability_requires_a_built_in_standalone_offer() {
        let response: InitializeResponse = serde_json::from_value(json!({
            "protocolVersion": 1,
            "agentInfo": {"name": "aelix-acp", "version": "fixture"},
            "agentCapabilities": {"_meta": {
                "aelix/sessionRelay": {"version": 1, "requests": ["_aelix/session/probe"], "notifications": []},
                "aelix/hostBytes": {"version": 1}
            }}
        }))
        .unwrap();
        for standalone in [true, false] {
            let relay = RelayConnection::new(true, standalone);
            relay.negotiate(&response);
            let mut meta = None;
            let attempt = relay.prepare_open(&mut meta);
            relay.opened(SessionId::new("s"), attempt, meta.as_ref());
            let discovery = relay.discovery().unwrap();
            assert_eq!(discovery.requests, ["_aelix/session/probe"]);
            assert_eq!(discovery.host_bytes, standalone);
            assert_eq!(relay.byte_grants(&discovery.binding_id).is_ok(), standalone);
        }
    }

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
