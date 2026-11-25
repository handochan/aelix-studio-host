//! Transcript v1 wire extension from aelix-acp 7b485386, docs/studio-integration.md.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use agent_client_protocol::schema::v1::{ContentBlock, Meta, RequestId, SessionId, SessionNotification};
use aionui_db::NativeMessageKey;
use serde::Deserialize;
use serde_json::Value;

pub(crate) const META: &str = "aelix/transcript";
pub(crate) const CAPABILITY: &str = "aelix/transcriptCapability";
const SETTLED: &str = "_aelix/transcript/settled";
// ACP bounds native and adapter updates separately. Also bound the staging
// overhead from wire envelopes and tool metadata on the Host side.
const MAX_FRAMES: usize = 262_144;
const MAX_WIRE_BYTES: usize = 256 * 1024 * 1024;

pub(crate) fn version_one(meta: Option<&Meta>, key: &str) -> bool {
    meta.and_then(|meta| meta.get(key))
        .and_then(|value| value.get("version"))
        .and_then(Value::as_u64)
        == Some(1)
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeEntry {
    pub native_session_id: String,
    pub entry_id: String,
    parent_entry_id: Option<String>,
    sequence: usize,
    pub role: Role,
}

impl NativeEntry {
    pub(crate) fn key(&self) -> NativeMessageKey {
        NativeMessageKey {
            session_id: self.native_session_id.clone(),
            entry_id: self.entry_id.clone(),
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Role {
    User,
    Assistant,
    ToolResult,
}

#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(crate) enum Coordinate {
    Replay {
        #[serde(flatten)]
        entry: NativeEntry,
        part: usize,
    },
    Adapter {
        event_id: String,
        binding_id: String,
        sequence: usize,
    },
}

pub(crate) struct ReplayFrame {
    pub coord: Coordinate,
    pub notification: SessionNotification,
}

pub(crate) struct Snapshot {
    pub frames: Vec<ReplayFrame>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
    version: u64,
    native_session_id: String,
    leaf_id: Option<String>,
    total_messages: usize,
    recovered: bool,
}

#[derive(Default)]
pub(crate) struct TranscriptCapture {
    session: Mutex<Option<SessionId>>,
    load: Mutex<Option<WireRequest<ReplayStage>>>,
    hidden: Mutex<Option<WireRequest<HiddenInputStage>>>,
}

pub(crate) struct ReplayStage {
    failed: bool,
    bytes: usize,
    frames: Vec<ReplayFrame>,
}

impl ReplayStage {
    pub(crate) fn finish(self, meta: Option<&Meta>) -> Result<Option<Snapshot>, &'static str> {
        let Some(marker) = meta.and_then(|meta| meta.get(META)) else {
            return Ok(None);
        };
        let marker: Marker = serde_json::from_value(marker.clone()).map_err(|_| "invalid_marker")?;
        if marker.version != 1
            || marker.native_session_id.is_empty()
            || marker.total_messages > MAX_FRAMES
            || (marker.total_messages > 0 && marker.leaf_id.as_deref().is_none_or(str::is_empty))
        {
            return Err("invalid_marker");
        }
        if marker.recovered {
            return Err("recovered_snapshot");
        }
        if self.failed || !valid_order(&self.frames, &marker) {
            return Err("incomplete_snapshot");
        }
        Ok(Some(Snapshot { frames: self.frames }))
    }
}

fn valid_order(frames: &[ReplayFrame], marker: &Marker) -> bool {
    let mut current: Option<(&NativeEntry, usize)> = None;
    let mut entries = HashSet::new();
    let mut adapter_events = HashSet::new();
    let mut adapter_sequences = HashMap::new();
    for frame in frames {
        match &frame.coord {
            Coordinate::Replay { entry, part } => {
                let sequence = current.map_or(1, |(prev, _)| prev.sequence + 1);
                if let Some((prev, next_part)) = current
                    && prev == entry
                    && next_part == *part
                {
                    current = Some((entry, part + 1));
                    continue;
                }
                if entry.native_session_id != marker.native_session_id
                    || entry.entry_id.is_empty()
                    || entry.sequence != sequence
                    || *part != 0
                    || !entries.insert(entry.entry_id.as_str())
                {
                    return false;
                }
                current = Some((entry, 1));
            }
            Coordinate::Adapter {
                event_id,
                binding_id,
                sequence,
            } => {
                let last = adapter_sequences.entry(binding_id).or_insert(0);
                if event_id.is_empty()
                    || binding_id.is_empty()
                    || !adapter_events.insert(event_id)
                    || *sequence <= *last
                {
                    return false;
                }
                *last = *sequence;
            }
        }
    }
    entries.len() == marker.total_messages
}

struct WireRequest<T> {
    session: SessionId,
    id: Option<Value>,
    sealed: bool,
    stage: T,
}

impl<T> WireRequest<T> {
    fn observe_response(&mut self, frame: &Value) {
        if self.id.is_some() && frame.get("method").is_none() && frame.get("id") == self.id.as_ref() {
            self.sealed = true;
        }
    }

    fn matches_notification(&self, frame: &Value) -> bool {
        self.id.is_some()
            && !self.sealed
            && frame.get("id").is_none()
            && frame["params"]["sessionId"].as_str() == Some(&self.session.0)
    }
}

pub(crate) struct CaptureGuard<'a, T> {
    slot: Option<&'a Mutex<Option<WireRequest<T>>>>,
}

impl<'a, T> CaptureGuard<'a, T> {
    fn start(slot: &'a Mutex<Option<WireRequest<T>>>, session: SessionId, stage: T) -> Option<Self> {
        let mut active = slot.lock().unwrap();
        if active.is_some() {
            return None;
        }
        *active = Some(WireRequest {
            session,
            id: None,
            sealed: false,
            stage,
        });
        Some(Self { slot: Some(slot) })
    }

    // Hold the lock through SDK registration so a fast reply on another
    // executor thread cannot pass the raw wire observer first.
    pub(crate) fn register<R>(&self, send: impl FnOnce() -> (RequestId, R)) -> R {
        let mut staged = self.slot.map(|slot| slot.lock().unwrap());
        let (id, sent) = send();
        if let Some(request) = staged.as_deref_mut().and_then(Option::as_mut) {
            request.id = Some(serde_json::to_value(id).expect("ACP request ID serializes"));
        }
        sent
    }

    pub(crate) fn take(mut self) -> Option<T> {
        let request = self.slot.take()?.lock().unwrap().take()?;
        request.sealed.then_some(request.stage)
    }
}

impl<T> Drop for CaptureGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            slot.lock().unwrap().take();
        }
    }
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct LiveEntry {
    #[serde(flatten)]
    entry: NativeEntry,
    binding_id: String,
    stream_id: String,
}

fn stored_user(value: &Value) -> Option<LiveEntry> {
    if value["source"] != "live" || value["role"] != "user" || value["status"] != "stored" {
        return None;
    }
    let entry: LiveEntry = serde_json::from_value(value.clone()).ok()?;
    (!entry.binding_id.is_empty()
        && !entry.stream_id.is_empty()
        && entry.entry.sequence > 0
        && !entry.entry.native_session_id.is_empty()
        && !entry.entry.entry_id.is_empty())
    .then_some(entry)
}

pub(crate) struct HiddenInputStage {
    expected_text: String,
    matched_bytes: usize,
    next_part: u64,
    chunks: Option<LiveEntry>,
    receipt: Option<NativeMessageKey>,
    ambiguous: bool,
}

impl HiddenInputStage {
    fn observe_user_chunk(&mut self, update: &Value) {
        let coordinates = &update["_meta"][META];
        if coordinates["source"] != "live" {
            return;
        }
        let valid = stored_user(coordinates).filter(|entry| {
            self.receipt.is_none()
                && self.chunks.as_ref().is_none_or(|prior| prior == entry)
                && coordinates["part"].as_u64() == Some(self.next_part)
        });
        if let Some(entry) = valid
            && update["content"]["type"] == "text"
            && let Some(text) = update["content"]["text"].as_str()
            && self.expected_text.as_bytes()[self.matched_bytes..].starts_with(text.as_bytes())
        {
            self.matched_bytes += text.len();
            self.next_part += 1;
            self.chunks = Some(entry);
        } else {
            self.ambiguous = true;
        }
    }

    fn observe_receipt(&mut self, params: &Value) {
        if params["source"] != "live" || params["role"] != "user" {
            return;
        }
        // The candidate can emit more than one user message_end per prompt
        // (aelix-ai 7ebb6463 harness/core.py, before_agent_start and next_turn).
        // Verify the matching chunks and wait for the response boundary.
        if self.receipt.is_none()
            && self.matched_bytes == self.expected_text.len()
            && let Some(entry) = stored_user(params)
            && self.chunks.as_ref() == Some(&entry)
        {
            self.receipt = Some(entry.entry.key());
        } else {
            self.ambiguous = true;
        }
    }

    pub(crate) fn finish(self) -> Option<NativeMessageKey> {
        self.receipt.filter(|_| !self.ambiguous)
    }
}

impl TranscriptCapture {
    pub(crate) fn set_session(&self, session: SessionId, capable: bool) {
        *self.session.lock().unwrap() = capable.then_some(session);
    }

    pub(crate) fn begin_load(&self, session: SessionId) -> Option<CaptureGuard<'_, ReplayStage>> {
        CaptureGuard::start(
            &self.load,
            session,
            ReplayStage {
                failed: false,
                bytes: 0,
                frames: Vec::new(),
            },
        )
    }

    pub(crate) fn begin_hidden_prompt(
        &self,
        session: &SessionId,
        prompt: &[ContentBlock],
    ) -> Option<CaptureGuard<'_, HiddenInputStage>> {
        if self.session.lock().unwrap().as_ref() != Some(session) {
            return None;
        }
        // ACP 7b485386 session.py _prompt joins text blocks and sends
        // images after the joined text. server.py native_message_updates
        // projects each stored image as a text placeholder.
        let mut text = Vec::new();
        let mut images = 0;
        for block in prompt {
            match block {
                ContentBlock::Text(block) => text.push(block.text.as_str()),
                ContentBlock::Image(_) => images += 1,
                _ => return None,
            }
        }
        let mut expected_text = text.join("\n\n");
        expected_text.push_str(&"[Image omitted from recovered history.]".repeat(images));
        CaptureGuard::start(
            &self.hidden,
            session.clone(),
            HiddenInputStage {
                expected_text,
                matched_bytes: 0,
                next_part: 0,
                chunks: None,
                receipt: None,
                ambiguous: false,
            },
        )
    }

    /// Observe in wire order before the SDK can dispatch a result.
    /// Historical display frames stay off the streaming UI path.
    pub(crate) fn observe(&self, line: &str) -> bool {
        let mut load = self.load.lock().unwrap();
        let mut hidden = self.hidden.lock().unwrap();
        if load.is_none() && hidden.is_none() && !line.contains(SETTLED) {
            return false;
        }
        let Ok(mut frame) = serde_json::from_str::<Value>(line) else {
            if let Some(pending) = load.as_mut().filter(|p| p.id.is_some() && !p.sealed) {
                pending.stage.failed = true;
            }
            if let Some(pending) = hidden.as_mut().filter(|p| p.id.is_some() && !p.sealed) {
                pending.stage.ambiguous = true;
            }
            return false;
        };
        if let Some(pending) = hidden.as_mut() {
            pending.observe_response(&frame);
            if pending.matches_notification(&frame) {
                let params = &frame["params"];
                if frame["method"] == SETTLED {
                    pending.stage.observe_receipt(params);
                } else if frame["method"] == "session/update"
                    && params["update"]["sessionUpdate"] == "user_message_chunk"
                {
                    pending.stage.observe_user_chunk(&params["update"]);
                }
            }
        }
        if frame["method"] == SETTLED && frame.get("id").is_none() {
            return true;
        }
        let Some(pending) = load.as_mut() else {
            return false;
        };
        pending.observe_response(&frame);
        if !pending.matches_notification(&frame) || frame["method"] != "session/update" {
            return false;
        }
        let meta = &frame["params"]["update"]["_meta"][META];
        if !matches!(meta["source"].as_str(), Some("replay" | "adapter")) {
            return false;
        }
        let replay = &mut pending.stage;
        replay.bytes = replay.bytes.saturating_add(line.len());
        if replay.failed || replay.frames.len() >= MAX_FRAMES || replay.bytes > MAX_WIRE_BYTES {
            replay.failed = true;
        } else {
            match (
                serde_json::from_value(meta.clone()),
                serde_json::from_value(frame["params"].take()),
            ) {
                (Ok(coord), Ok(notification)) => replay.frames.push(ReplayFrame { coord, notification }),
                _ => replay.failed = true,
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn marker(total: usize) -> Meta {
        json!({META: {"version": 1, "nativeSessionId": "native", "leafId": "leaf", "totalMessages": total, "recovered": false}})
            .as_object().unwrap().clone()
    }

    fn update(entry: &str, sequence: usize, part: usize) -> Value {
        json!({
            "jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "acp",
                "update": {
                    "sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "same"},
                    "_meta": {META: {"source": "replay", "nativeSessionId": "native", "entryId": entry,
                        "parentEntryId": null, "sequence": sequence, "role": "user", "part": part}}
                }
            }
        })
    }

    #[test]
    fn snapshot_requires_the_matching_wire_result_and_complete_order() {
        let capture = TranscriptCapture::default();
        let guard = capture.begin_load(SessionId::new("acp")).unwrap();
        guard.register(|| (RequestId::Number(27), ()));
        assert!(capture.observe(&update("one", 1, 0).to_string()));
        assert!(!capture.observe(&json!({"jsonrpc":"2.0", "id":1, "result":{}}).to_string()));
        assert!(capture.observe(&update("one", 1, 1).to_string()));
        assert!(capture.observe(&update("two", 2, 0).to_string()));
        assert!(!capture.observe(&json!({"jsonrpc":"2.0", "id":27, "result":{}}).to_string()));
        assert!(!capture.observe(&update("too-late", 3, 0).to_string()));
        let snapshot = guard.take().unwrap().finish(Some(&marker(2))).unwrap().unwrap();
        assert_eq!(snapshot.frames.len(), 3);
    }

    #[test]
    fn rejects_missing_marker_partial_parts_recovery_and_missing_entries() {
        for (coordinates, total, recovered, expected) in [
            (vec![("one", 1, 0)], None, false, Ok(false)),
            (vec![("one", 1, 0)], Some(2), false, Err("incomplete_snapshot")),
            (
                vec![("one", 1, 0), ("one", 1, 2)],
                Some(1),
                false,
                Err("incomplete_snapshot"),
            ),
            (
                vec![("one", 1, 0), ("one", 2, 0)],
                Some(2),
                false,
                Err("incomplete_snapshot"),
            ),
            (vec![("one", 1, 0)], Some(1), true, Err("recovered_snapshot")),
            (vec![], Some(0), false, Ok(true)),
        ] {
            let capture = TranscriptCapture::default();
            let guard = capture.begin_load(SessionId::new("acp")).unwrap();
            guard.register(|| (RequestId::Number(3), ()));
            for (id, sequence, part) in coordinates {
                assert!(capture.observe(&update(id, sequence, part).to_string()));
            }
            assert!(!capture.observe(&json!({"jsonrpc":"2.0", "id":3, "result":{}}).to_string()));
            let marker = total.map(|total| {
                let mut meta = marker(total);
                meta.get_mut(META).unwrap()["recovered"] = json!(recovered);
                meta
            });
            assert_eq!(
                guard
                    .take()
                    .unwrap()
                    .finish(marker.as_ref())
                    .map(|snapshot| snapshot.is_some()),
                expected
            );
        }
    }

    fn user_receipt(entry: &str, status: &str) -> Value {
        json!({"jsonrpc":"2.0", "method":SETTLED, "params":{
            "sessionId":"acp", "source":"live", "role":"user", "status":status,
            "nativeSessionId":"native", "bindingId":"binding", "streamId":entry,
            "entryId":entry, "parentEntryId":null, "sequence":7
        }})
    }

    fn user_chunk(receipt: &Value, text: &str, part: u64) -> Value {
        let mut metadata = receipt["params"].clone();
        metadata["part"] = json!(part);
        json!({"jsonrpc":"2.0", "method":"session/update", "params":{"sessionId":"acp", "update":{
            "sessionUpdate":"user_message_chunk", "content":{"type":"text","text":text}, "_meta":{META:metadata}
        }}})
    }

    #[test]
    fn only_an_unambiguous_stored_user_receipt_binds_the_hidden_prompt() {
        let capture = TranscriptCapture::default();
        let session = SessionId::new("acp");
        let prompt = [ContentBlock::from("hidden input")];
        assert!(capture.begin_hidden_prompt(&session, &prompt).is_none());
        capture.set_session(session.clone(), true);
        for (twice, status, expected) in [
            (false, "stored", true),
            (true, "stored", false),
            (false, "notStored", false),
        ] {
            let guard = capture.begin_hidden_prompt(&session, &prompt).unwrap();
            guard.register(|| (RequestId::Number(9), ()));
            let receipt = user_receipt("input", status);
            assert!(!capture.observe(&user_chunk(&receipt, "hidden ", 0).to_string()));
            assert!(!capture.observe(&user_chunk(&receipt, "input", 1).to_string()));
            assert!(capture.observe(&receipt.to_string()));
            if twice {
                let receipt = user_receipt("injected", "stored");
                assert!(!capture.observe(&user_chunk(&receipt, "hidden input", 0).to_string()));
                assert!(capture.observe(&receipt.to_string()));
            }
            assert!(!capture.observe(&json!({"jsonrpc":"2.0", "id":9, "result":{}}).to_string()));
            assert_eq!(guard.take().unwrap().finish().is_some(), expected);
        }
    }
}
