use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::models::MessageRow;
use crate::{DbError, SqlitePool};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NativeMessageKey {
    pub session_id: String,
    pub entry_id: String,
}

/// The order of these rows is the adapter's display order. A row without a
/// native key has a stable adapter event identity instead.
pub struct AcpTranscriptRow {
    pub message: MessageRow,
    pub native: Option<NativeMessageKey>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AcpTranscriptApply {
    Applied { displayed: usize, archived: usize },
    Unchanged,
    UnidentifiedHiddenInput,
    DisplayReset,
}

pub(super) type ActiveInputs = Arc<Mutex<HashSet<String>>>;

/// Retains the composer echo while a new turn opens its session. The claim
/// is in memory only: after a restart an unsettled hidden input fails closed.
#[derive(Debug)]
pub struct AcpTranscriptInputGuard {
    inputs: ActiveInputs,
    message_id: String,
}

impl AcpTranscriptInputGuard {
    pub(super) fn new(inputs: &ActiveInputs, message_id: &str) -> Self {
        inputs.lock().unwrap().insert(message_id.to_owned());
        Self {
            inputs: inputs.clone(),
            message_id: message_id.to_owned(),
        }
    }
}

impl Drop for AcpTranscriptInputGuard {
    fn drop(&mut self) {
        self.inputs.lock().unwrap().remove(&self.message_id);
    }
}

#[derive(sqlx::FromRow)]
struct StoredDisplayRow {
    #[sqlx(flatten)]
    message: MessageRow,
    acp_transcript_source: Option<String>,
    acp_native_session_id: Option<String>,
    acp_native_entry_id: Option<String>,
}

impl StoredDisplayRow {
    fn is_hidden_input(&self) -> bool {
        self.message.hidden && self.message.r#type == "text" && self.message.position.as_deref() == Some("right")
    }

    fn native_key(&self) -> Option<NativeMessageKey> {
        Some(NativeMessageKey {
            session_id: self.acp_native_session_id.clone()?,
            entry_id: self.acp_native_entry_id.clone()?,
        })
    }

    fn matches(&self, message: &MessageRow, native: Option<&NativeMessageKey>) -> bool {
        self.message == *message && self.native_key().as_ref() == native
    }
}

/// Apply a completed replay and move the superseded display rows atomically.
/// The writer lock orders this against concurrent composer writes, reset,
/// deletion and readers of the active messages table.
pub(super) async fn apply(
    pool: &SqlitePool,
    inputs: &ActiveInputs,
    user_id: &str,
    conversation_id: &str,
    session_id: &str,
    rows: Vec<AcpTranscriptRow>,
) -> Result<AcpTranscriptApply, DbError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ? AND user_id = ?)")
        .bind(conversation_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    if !owned {
        return Err(DbError::NotFound("Conversation not found".into()));
    }

    let display_reset: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM acp_transcript_display_resets WHERE conversation_id = ? AND session_id = ?)",
    )
    .bind(conversation_id)
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;
    if display_reset {
        return Ok(AcpTranscriptApply::DisplayReset);
    }

    // Teammate messages are app-authored text. Other app rows (tips, plans,
    // permission cards, job artifacts, etc.) live outside this replacement.
    let existing: Vec<StoredDisplayRow> = sqlx::query_as(
        "SELECT * FROM messages WHERE conversation_id = ?
         AND (acp_transcript_source IS NOT NULL OR type IN ('text', 'thinking', 'acp_tool_call'))
         AND (type != 'text' OR (
            json_extract(CASE WHEN json_valid(content) THEN content ELSE '{}' END, '$.teammate_message') IS NOT 1
            AND json_extract(CASE WHEN json_valid(content) THEN content ELSE '{}' END, '$.teammateMessage') IS NOT 1
         ))",
    )
    .bind(conversation_id)
    .fetch_all(&mut *tx)
    .await?;

    let protected = inputs.lock().unwrap().clone();
    if existing
        .iter()
        .any(|row| row.is_hidden_input() && row.native_key().is_none() && !protected.contains(&row.message.id))
    {
        return Ok(AcpTranscriptApply::UnidentifiedHiddenInput);
    }
    let mut hidden: HashSet<NativeMessageKey> = existing
        .iter()
        .filter(|row| row.is_hidden_input())
        .filter_map(StoredDisplayRow::native_key)
        .collect();
    let archived_keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT acp_native_session_id, acp_native_entry_id
         FROM acp_transcript_display_archive
         WHERE conversation_id = ? AND hidden = 1 AND type = 'text' AND position = 'right'
           AND acp_native_session_id IS NOT NULL AND acp_native_entry_id IS NOT NULL",
    )
    .bind(conversation_id)
    .fetch_all(&mut *tx)
    .await?;
    hidden.extend(
        archived_keys
            .into_iter()
            .map(|(session_id, entry_id)| NativeMessageKey { session_id, entry_id }),
    );

    // Preserve the Host's last optimistic fork reference for the display
    // head. ACP still checks its saved native digest before writing any fork.
    let fork_head = existing
        .iter()
        .filter(|row| row.message.backend_turn_id.is_some())
        .max_by_key(|row| (row.message.created_at, &row.message.id))
        .and_then(|row| row.message.backend_turn_id.clone());
    let incoming: HashSet<_> = rows.iter().map(|row| row.message.id.as_str()).collect();
    let mut prior = HashMap::new();
    let mut superseded = Vec::new();
    let now = aionui_common::now_ms();
    let mut earliest = now;
    let mut latest = None;
    let mut before_input = now;
    for row in existing {
        if protected.contains(&row.message.id) {
            before_input = before_input.min(row.message.created_at.saturating_sub(1));
        } else if row.acp_transcript_source.as_deref() != Some("adapter") || incoming.contains(row.message.id.as_str())
        {
            let timestamp = row.message.created_at;
            earliest = earliest.min(timestamp);
            latest = Some(latest.map_or(timestamp, |latest: i64| latest.max(timestamp)));
            if row.acp_transcript_source.is_some() && incoming.contains(row.message.id.as_str()) {
                prior.insert(row.message.id.clone(), row);
            } else {
                superseded.push(row.message.id);
            }
        }
    }

    if !superseded.is_empty() {
        let ids = serde_json::to_string(&superseded).expect("string IDs serialize");
        sqlx::query(
            "INSERT INTO acp_transcript_display_archive
             (conversation_id, message_id, msg_id, type, content, position, status, hidden, created_at,
              backend_turn_id, acp_transcript_source, acp_native_session_id, acp_native_entry_id)
             SELECT conversation_id, id, msg_id, type, content, position, status, hidden, created_at,
                    backend_turn_id, acp_transcript_source, acp_native_session_id, acp_native_entry_id
             FROM messages WHERE conversation_id = ? AND id IN (SELECT value FROM json_each(?))",
        )
        .bind(conversation_id)
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM messages WHERE conversation_id = ? AND id IN (SELECT value FROM json_each(?))")
            .bind(conversation_id)
            .bind(&ids)
            .execute(&mut *tx)
            .await?;
    }

    // Replay carries no display timestamps. Preserve the old display span so
    // retained app messages do not leap past its head. Leave room for an
    // in-flight composer echo and assign distinct cursor slots within the span.
    let displayed = rows.len();
    let last = latest.unwrap_or(before_input).min(before_input);
    let steps = displayed.saturating_sub(1) as i64;
    let first = earliest.min(last.saturating_sub(steps));
    let mut changed = !superseded.is_empty();
    for (offset, row) in rows.into_iter().enumerate() {
        let AcpTranscriptRow { mut message, native } = row;
        message.created_at = if steps == 0 {
            last
        } else {
            let span = i128::from(last) - i128::from(first);
            (i128::from(first) + span * offset as i128 / i128::from(steps)) as i64
        };
        if offset + 1 == displayed {
            message.backend_turn_id = fork_head.clone();
        }
        if message.r#type == "text" && message.position.as_deref() == Some("right") {
            message.hidden = native.as_ref().is_some_and(|native| hidden.contains(native));
        }
        if let Some(previous) = prior.get(&message.id) {
            if message.r#type == "acp_tool_call" {
                preserve_verified_meta(&previous.message, &mut message)?;
            }
            if previous.matches(&message, native.as_ref()) {
                continue;
            }
        }

        let source = if native.is_some() { "native" } else { "adapter" };
        let written = sqlx::query(
            "INSERT INTO messages
               (id, conversation_id, msg_id, type, content, position, status, hidden, created_at,
                backend_turn_id, acp_transcript_source, acp_native_session_id, acp_native_entry_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                content = excluded.content, status = excluded.status, hidden = excluded.hidden,
                created_at = excluded.created_at, backend_turn_id = excluded.backend_turn_id
             WHERE messages.conversation_id = excluded.conversation_id
               AND messages.acp_transcript_source = excluded.acp_transcript_source
               AND messages.acp_native_session_id IS excluded.acp_native_session_id
               AND messages.acp_native_entry_id IS excluded.acp_native_entry_id",
        )
        .bind(&message.id)
        .bind(conversation_id)
        .bind(&message.msg_id)
        .bind(&message.r#type)
        .bind(&message.content)
        .bind(&message.position)
        .bind(&message.status)
        .bind(message.hidden)
        .bind(message.created_at)
        .bind(&message.backend_turn_id)
        .bind(source)
        .bind(native.as_ref().map(|key| &key.session_id))
        .bind(native.as_ref().map(|key| &key.entry_id))
        .execute(&mut *tx)
        .await?;
        if written.rows_affected() != 1 {
            return Err(DbError::Conflict("ACP transcript display identity conflicted".into()));
        }
        changed = true;
    }
    if changed {
        sqlx::query("UPDATE conversations SET updated_at = MAX(updated_at, ?) WHERE id = ?")
            .bind(now)
            .bind(conversation_id)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;
    Ok(if changed {
        AcpTranscriptApply::Applied {
            displayed,
            archived: superseded.len(),
        }
    } else {
        AcpTranscriptApply::Unchanged
    })
}

fn preserve_verified_meta(previous: &MessageRow, message: &mut MessageRow) -> Result<(), DbError> {
    let previous: serde_json::Value = serde_json::from_str(&previous.content)
        .map_err(|_| DbError::Conflict("Persisted ACP tool card is invalid".into()))?;
    let Some(meta) = previous.get("_meta").and_then(serde_json::Value::as_object) else {
        return Ok(());
    };
    let mut current: serde_json::Value = serde_json::from_str(&message.content)
        .map_err(|_| DbError::Conflict("Replayed ACP tool card is invalid".into()))?;
    if let Some(current_meta) = current["_meta"].as_object_mut() {
        // These rows share the same stable ACP replay identity. Prefer the
        // current adapter fields and keep only missing opaque fields.
        for (key, value) in meta {
            if key != "aelix/transcript" && !current_meta.contains_key(key) {
                current_meta.insert(key.clone(), value.clone());
            }
        }
        message.content = current.to_string();
    }
    Ok(())
}

pub(super) async fn bind_hidden_input(
    pool: &SqlitePool,
    user_id: &str,
    conversation_id: &str,
    message_id: &str,
    native: &NativeMessageKey,
) -> Result<(), DbError> {
    let written = sqlx::query(
        "UPDATE messages SET acp_native_session_id = ?, acp_native_entry_id = ?
         WHERE conversation_id = ? AND id = ? AND type = 'text' AND position = 'right' AND hidden = 1
           AND acp_transcript_source IS NULL
           AND ((acp_native_session_id IS NULL AND acp_native_entry_id IS NULL)
                OR (acp_native_session_id = ? AND acp_native_entry_id = ?))
           AND EXISTS (SELECT 1 FROM conversations WHERE id = messages.conversation_id AND user_id = ?)",
    )
    .bind(&native.session_id)
    .bind(&native.entry_id)
    .bind(conversation_id)
    .bind(message_id)
    .bind(&native.session_id)
    .bind(&native.entry_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    if written.rows_affected() == 0 {
        return Err(DbError::Conflict("ACP hidden input could not be bound".into()));
    }
    Ok(())
}
