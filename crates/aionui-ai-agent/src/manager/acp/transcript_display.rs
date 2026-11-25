use std::collections::HashMap;

use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate};
use aionui_db::models::MessageRow;
use aionui_db::{AcpTranscriptRow, NativeMessageKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::protocol::acp_transcript::{Coordinate, Role, Snapshot};
use crate::protocol::events::{AgentStreamEvent, session_notification_to_events};

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextKind {
    User,
    Assistant,
    Thought,
}

struct DisplayRow {
    id: String,
    native: Option<NativeMessageKey>,
    content: Content,
}

enum Content {
    Text(TextKind, String),
    Tool { content: Value, status: &'static str },
    Plan(Value),
}

impl DisplayRow {
    fn into_row(self, conversation_id: &str) -> AcpTranscriptRow {
        let (kind, position, status, content) = match self.content {
            Content::Text(TextKind::User, text) => ("text", "right", "finish", json!({"content": text})),
            Content::Text(TextKind::Assistant, text) => ("text", "left", "finish", json!({"content": text})),
            Content::Text(TextKind::Thought, text) => (
                "thinking",
                "left",
                "finish",
                json!({"content": text, "status": "done", "duration_ms": 0}),
            ),
            Content::Tool { content, status } => ("acp_tool_call", "left", status, content),
            Content::Plan(content) => ("plan", "left", "finish", content),
        };
        AcpTranscriptRow {
            message: MessageRow {
                id: self.id.clone(),
                msg_id: Some(self.id),
                conversation_id: conversation_id.to_owned(),
                r#type: kind.to_owned(),
                position: Some(position.to_owned()),
                status: Some(status.to_owned()),
                content: content.to_string(),
                hidden: false,
                created_at: 0, // Storage fits the replay order into display cursor slots.
                backend_turn_id: None,
            },
            native: self.native,
        }
    }
}

/// Convert one complete native snapshot to the same row shapes as the live
/// stream. Only ACP's verified card association is used to fold tool results.
pub(super) fn project(snapshot: Snapshot, conversation_id: &str) -> Result<Vec<AcpTranscriptRow>, &'static str> {
    let mut rows: Vec<DisplayRow> = Vec::new();
    let mut tools = HashMap::new();
    let mut contiguous_text = false;
    for frame in snapshot.frames {
        let (native, role, identity) = match &frame.coord {
            Coordinate::Replay { entry, part } => (
                Some(entry.key()),
                Some(entry.role),
                json!([conversation_id, "native", entry.native_session_id, entry.entry_id, part]),
            ),
            Coordinate::Adapter { event_id, .. } => (None, None, json!([conversation_id, "adapter", event_id])),
        };
        let id = || format!("acp-transcript-{}", hex::encode(Sha256::digest(identity.to_string())));
        let text = match &frame.notification.update {
            SessionUpdate::UserMessageChunk(chunk) if matches!(role, None | Some(Role::User)) => {
                Some((TextKind::User, &chunk.content))
            }
            SessionUpdate::AgentMessageChunk(chunk) if role != Some(Role::User) => {
                Some((TextKind::Assistant, &chunk.content))
            }
            SessionUpdate::AgentThoughtChunk(chunk) if matches!(role, None | Some(Role::Assistant)) => {
                Some((TextKind::Thought, &chunk.content))
            }
            _ => None,
        };
        if let Some((kind, content)) = text {
            let text = match content {
                ContentBlock::Text(content) => content.text.as_str(),
                // The native projection emits text placeholders. ACP-only
                // image echoes can still arrive as ordinary ACP image blocks.
                ContentBlock::Image(_) if native.is_none() => "[Image omitted from recovered history.]",
                _ => return Err("unsupported_transcript_content"),
            };
            if !text.is_empty() {
                if contiguous_text
                    && native.is_some()
                    && let Some(last) = rows.last_mut()
                    && last.native == native
                    && let Content::Text(previous_kind, previous_text) = &mut last.content
                    && *previous_kind == kind
                {
                    previous_text.push_str(text);
                } else {
                    rows.push(DisplayRow {
                        id: id(),
                        native,
                        content: Content::Text(kind, text.to_owned()),
                    });
                }
            }
            contiguous_text = !text.is_empty();
            continue;
        }
        contiguous_text = false;

        match session_notification_to_events(&frame.notification).into_iter().next() {
            Some(AgentStreamEvent::AcpToolCall(data)) if role != Some(Role::User) => {
                let call_id = data.update.tool_call_id.clone();
                let (patch, display_status) = data.message_fields();
                if matches!(frame.notification.update, SessionUpdate::ToolCall(_)) {
                    if call_id.is_empty() || tools.insert(call_id, rows.len()).is_some() {
                        return Err("duplicate_transcript_tool_card");
                    }
                    rows.push(DisplayRow {
                        id: id(),
                        native,
                        content: Content::Tool {
                            content: patch,
                            status: display_status,
                        },
                    });
                } else {
                    let index = *tools.get(&call_id).ok_or("missing_transcript_tool_card")?;
                    let Content::Tool { content, status } = &mut rows[index].content else {
                        unreachable!("only tool cards are indexed");
                    };
                    merge_patch(content, patch);
                    if data.update.status.is_some() {
                        *status = display_status;
                    }
                }
            }
            Some(AgentStreamEvent::Plan(plan)) if native.is_none() => {
                rows.push(DisplayRow {
                    id: id(),
                    native,
                    content: Content::Plan(json!(plan)),
                });
            }
            _ => return Err("unsupported_transcript_update"),
        }
    }
    Ok(rows.into_iter().map(|row| row.into_row(conversation_id)).collect())
}

// Same RFC 7396 semantics as a live ACP tool upsert. In particular, a
// status-only frame keeps _meta, null removes a key, and arrays replace.
fn merge_patch(target: &mut Value, patch: Value) {
    if let Value::Object(fields) = patch {
        if !target.is_object() {
            *target = json!({});
        }
        let target = target.as_object_mut().unwrap();
        for (key, value) in fields {
            if value.is_null() {
                target.remove(&key);
            } else {
                merge_patch(target.entry(key).or_insert(Value::Null), value);
            }
        }
    } else {
        *target = patch;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::acp_transcript::ReplayFrame;

    fn frame(coordinate: Value, update: Value) -> ReplayFrame {
        ReplayFrame {
            coord: serde_json::from_value(coordinate).unwrap(),
            notification: serde_json::from_value(json!({"sessionId": "session", "update": update})).unwrap(),
        }
    }

    fn replay(entry: &str, sequence: usize, part: usize, role: &str, update: Value) -> ReplayFrame {
        frame(
            json!({
                "source": "replay", "nativeSessionId": "native", "entryId": entry, "parentEntryId": null,
                "sequence": sequence, "part": part, "role": role,
            }),
            update,
        )
    }

    #[test]
    fn projection_keeps_segments_tools_and_adapter_display_in_order() {
        let text = |text| json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":text}});
        let frames = vec![
            replay("assistant", 1, 0, "assistant", text("a")),
            replay("assistant", 1, 1, "assistant", text("b")),
            replay(
                "assistant",
                1,
                2,
                "assistant",
                json!({
                    "sessionUpdate":"tool_call", "toolCallId":"tool", "title":"agent", "status":"in_progress",
                    "_meta":{"aelix/subagents":{"children":[{"id":"fixture","currentTool":"read"}]}},
                }),
            ),
            replay("assistant", 1, 3, "assistant", text("c")),
            replay(
                "result",
                2,
                0,
                "toolResult",
                json!({
                    "sessionUpdate":"tool_call_update", "toolCallId":"tool", "status":"completed",
                    "content":[{"type":"content","content":{"type":"text","text":"done"}}],
                }),
            ),
            frame(
                json!({"source":"adapter","eventId":"event","bindingId":"binding","sequence":1}),
                text("adapter"),
            ),
        ];
        let rows = project(Snapshot { frames }, "conversation").unwrap();
        assert_eq!(rows.len(), 4);
        let display = |index: usize| serde_json::from_str::<Value>(&rows[index].message.content).unwrap();
        assert_eq!(display(0)["content"], "ab");
        assert_eq!(rows[1].message.r#type, "acp_tool_call");
        assert_eq!(rows[1].message.status.as_deref(), Some("finish"));
        assert_eq!(display(1)["update"]["content"][0]["content"]["text"], "done");
        assert_eq!(
            display(1)["_meta"]["aelix/subagents"]["children"][0]["currentTool"],
            "read"
        );
        assert_eq!(rows[1].native.as_ref().unwrap().entry_id, "assistant");
        assert_eq!(display(2)["content"], "c");
        assert_eq!(display(3)["content"], "adapter");
        assert!(rows[3].native.is_none());
    }
}
