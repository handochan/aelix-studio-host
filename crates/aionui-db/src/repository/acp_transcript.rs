use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use sqlx::{QueryBuilder, Sqlite, Transaction};

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
pub(super) struct StoredDisplayRow {
    #[sqlx(flatten)]
    pub(super) message: MessageRow,
    acp_transcript_source: Option<String>,
    acp_native_session_id: Option<String>,
    acp_native_entry_id: Option<String>,
}

impl StoredDisplayRow {
    fn is_hidden_input(&self) -> bool {
        self.message.hidden && self.message.r#type == "text" && self.message.position.as_deref() == Some("right")
    }

    // Guarded ACP HEAD forks keep entry IDs, and the child has a new
    // native session ID. Copy only the entry ID until a child load proves it.
    pub(super) fn fork_hidden_entry(&self) -> Option<&str> {
        self.acp_native_entry_id.as_deref().filter(|_| self.is_hidden_input())
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
/// Plan under a read transaction, then retry if a concurrent write made its
/// snapshot stale. A writer-first transaction is the contention fallback.
pub(super) async fn apply(
    pool: &SqlitePool,
    inputs: &ActiveInputs,
    user_id: &str,
    conversation_id: &str,
    session_id: &str,
    rows: Vec<AcpTranscriptRow>,
) -> Result<AcpTranscriptApply, DbError> {
    let tx = pool.begin().await?;
    match apply_snapshot(tx, inputs, user_id, conversation_id, session_id, &rows).await {
        Err(err) if err.is_busy() => {
            let tx = pool.begin_with("BEGIN IMMEDIATE").await?;
            apply_snapshot(tx, inputs, user_id, conversation_id, session_id, &rows).await
        }
        result => result,
    }
}

async fn apply_snapshot(
    mut tx: Transaction<'_, Sqlite>,
    inputs: &ActiveInputs,
    user_id: &str,
    conversation_id: &str,
    session_id: &str,
    rows: &[AcpTranscriptRow],
) -> Result<AcpTranscriptApply, DbError> {
    let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ? AND user_id = ?)")
        .bind(conversation_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    if !owned {
        return Err(DbError::NotFound("Conversation not found".into()));
    }

    let display_reset: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM acp_transcript_display_resets
                       WHERE conversation_id = ? AND session_id IN ('', ?))",
    )
    .bind(conversation_id)
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;
    if display_reset {
        return Ok(AcpTranscriptApply::DisplayReset);
    }

    // Preserve app-owned messages, including teammate text.
    let existing: Vec<StoredDisplayRow> = sqlx::query_as(
        "SELECT * FROM messages WHERE conversation_id = ?
         AND (acp_transcript_source IS NOT NULL OR type IN ('text', 'thinking', 'acp_tool_call'))
         AND (type != 'text' OR (
            json_extract(CASE WHEN json_valid(content) THEN content ELSE '{}' END, '$.teammate_message') IS NOT 1
            AND json_extract(CASE WHEN json_valid(content) THEN content ELSE '{}' END, '$.teammateMessage') IS NOT 1
         ))
         ORDER BY created_at, id",
    )
    .bind(conversation_id)
    .fetch_all(&mut *tx)
    .await?;

    let protected = inputs.lock().unwrap().clone();
    let Some((mut hidden, inherited_session)) = hidden_inputs(&existing, rows, &protected) else {
        return Ok(AcpTranscriptApply::UnidentifiedHiddenInput);
    };
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

    let now = aionui_common::now_ms();
    let incoming: HashSet<_> = rows.iter().map(|row| row.message.id.as_str()).collect();
    let mut prior = HashMap::new();
    let mut prior_order = Vec::new();
    let mut superseded = Vec::new();
    let mut earliest = now;
    let mut latest = None;
    let mut before_input = None;
    let mut fork_head = None;
    for row in &existing {
        if let Some(head) = row.message.backend_turn_id.as_deref() {
            fork_head = Some(head);
        }
        if protected.contains(&row.message.id) {
            let limit = row.message.created_at.saturating_sub(1);
            before_input = Some(before_input.map_or(limit, |value: i64| value.min(limit)));
        } else if row.acp_transcript_source.is_some() && incoming.contains(row.message.id.as_str()) {
            let timestamp = row.message.created_at;
            earliest = earliest.min(timestamp);
            latest = Some(timestamp);
            prior_order.push(row.message.id.as_str());
            prior.insert(row.message.id.as_str(), row);
        } else if row.acp_transcript_source.as_deref() != Some("adapter") {
            let timestamp = row.message.created_at;
            earliest = earliest.min(timestamp);
            latest = Some(timestamp);
            superseded.push(row.message.id.as_str());
        }
    }

    let displayed = rows.len();
    let appended = displayed.saturating_sub(prior_order.len()) as i64;
    let prefix_tail = prior_order
        .last()
        .and_then(|id| prior.get(id))
        .map(|row| row.message.created_at);
    let append_head = prefix_tail.and_then(|tail| tail.checked_add(appended));
    let keep_prefix = superseded.is_empty()
        && prior_order
            .iter()
            .copied()
            .eq(rows.iter().take(prior_order.len()).map(|row| row.message.id.as_str()))
        && append_head.is_some_and(|head| before_input.is_none_or(|limit| head <= limit));

    // When the projection is reordered, fit it into the previous display
    // span. Pure appends keep the existing cursor slots, with new slots
    // after the old head and before any in-flight composer echo.
    let end_limit = before_input.map_or(now, |limit| limit.min(now));
    let last = latest.unwrap_or(end_limit).min(end_limit);
    let steps = displayed.saturating_sub(1) as i64;
    let first = earliest.min(last.saturating_sub(steps));
    let append_first = prefix_tail.unwrap_or(last).saturating_add(1);
    let mut changes = Vec::new();
    for (offset, row) in rows.iter().enumerate() {
        let mut message = row.message.clone();
        let previous = prior.get(message.id.as_str()).copied();
        if keep_prefix {
            if let Some(previous) = previous {
                message.created_at = previous.message.created_at;
                message.backend_turn_id.clone_from(&previous.message.backend_turn_id);
            } else {
                message.created_at = append_first + (offset - prior_order.len()) as i64;
            }
        } else {
            message.created_at = if steps == 0 {
                last
            } else {
                let span = i128::from(last) - i128::from(first);
                (i128::from(first) + span * offset as i128 / i128::from(steps)) as i64
            };
        }
        // Keep the last optimistic fork reference for the display head.
        // The adapter verifies its saved native digest before forking.
        if offset + 1 == displayed {
            message.backend_turn_id = fork_head.map(str::to_owned);
        }
        if message.r#type == "text" && message.position.as_deref() == Some("right") {
            message.hidden = row.native.as_ref().is_some_and(|native| hidden.contains(native));
        }
        if let Some(previous) = previous {
            if message.r#type == "acp_tool_call" {
                preserve_verified_meta(&previous.message, &mut message)?;
            }
            if previous.matches(&message, row.native.as_ref()) {
                continue;
            }
        }
        changes.push((message, row.native.as_ref()));
    }

    // All planning is complete before upgrading to a write transaction.
    if !superseded.is_empty() {
        let ids = serde_json::to_string(&superseded).expect("string IDs serialize");
        sqlx::query(
            "INSERT INTO acp_transcript_display_archive
             (conversation_id, message_id, msg_id, type, content, position, status, hidden, created_at,
              backend_turn_id, acp_transcript_source, acp_native_session_id, acp_native_entry_id)
             SELECT conversation_id, id, msg_id, type, content, position, status, hidden, created_at,
                    backend_turn_id, acp_transcript_source,
                    CASE WHEN hidden = 1 AND type = 'text' AND position = 'right' AND acp_native_entry_id IS NOT NULL
                         THEN COALESCE(acp_native_session_id, ?) ELSE acp_native_session_id END,
                    acp_native_entry_id
             FROM messages WHERE conversation_id = ? AND id IN (SELECT value FROM json_each(?))",
        )
        .bind(inherited_session)
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

    write_changes(&mut tx, conversation_id, &changes).await?;
    let changed = !superseded.is_empty() || !changes.is_empty();
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

/// Resolve pending inherited entry IDs against verified child user messages.
/// The child native session ID is stored when the inherited rows are archived.
fn hidden_inputs<'a>(
    existing: &[StoredDisplayRow],
    rows: &'a [AcpTranscriptRow],
    protected: &HashSet<String>,
) -> Option<(HashSet<NativeMessageKey>, Option<&'a str>)> {
    let mut hidden = HashSet::new();
    let mut child_keys = None;
    let mut child_session = None;
    for row in existing.iter().filter(|row| row.is_hidden_input()) {
        if let Some(native) = row.native_key() {
            hidden.insert(native);
        } else if let Some(entry) = row.fork_hidden_entry() {
            let keys: &HashMap<_, _> = child_keys.get_or_insert_with(|| {
                rows.iter()
                    .filter(|row| row.message.r#type == "text" && row.message.position.as_deref() == Some("right"))
                    .filter_map(|row| row.native.as_ref().map(|key| (key.entry_id.as_str(), key)))
                    .collect()
            });
            let child = *keys.get(entry)?;
            if child_session.is_some_and(|session| session != child.session_id) {
                return None;
            }
            child_session = Some(child.session_id.as_str());
            hidden.insert(child.clone());
        } else if !protected.contains(&row.message.id) {
            return None;
        }
    }
    Some((hidden, child_session))
}

async fn write_changes(
    tx: &mut Transaction<'_, Sqlite>,
    conversation_id: &str,
    changes: &[(MessageRow, Option<&NativeMessageKey>)],
) -> Result<(), DbError> {
    // At most 832 bound values, below SQLite's original 999 limit.
    for chunk in changes.chunks(64) {
        let mut query = QueryBuilder::<Sqlite>::new(
            "INSERT INTO messages
               (id, conversation_id, msg_id, type, content, position, status, hidden, created_at,
                backend_turn_id, acp_transcript_source, acp_native_session_id, acp_native_entry_id) ",
        );
        query.push_values(chunk, |mut values, (message, native)| {
            values
                .push_bind(&message.id)
                .push_bind(conversation_id)
                .push_bind(&message.msg_id)
                .push_bind(&message.r#type)
                .push_bind(&message.content)
                .push_bind(&message.position)
                .push_bind(&message.status)
                .push_bind(message.hidden)
                .push_bind(message.created_at)
                .push_bind(&message.backend_turn_id)
                .push_bind(if native.is_some() { "native" } else { "adapter" })
                .push_bind(native.map(|key| &key.session_id))
                .push_bind(native.map(|key| &key.entry_id));
        });
        query.push(
            " ON CONFLICT(id) DO UPDATE SET
                content = excluded.content, status = excluded.status, hidden = excluded.hidden,
                created_at = excluded.created_at, backend_turn_id = excluded.backend_turn_id
              WHERE messages.conversation_id = excluded.conversation_id
                AND messages.acp_transcript_source = excluded.acp_transcript_source
                AND messages.acp_native_session_id IS excluded.acp_native_session_id
                AND messages.acp_native_entry_id IS excluded.acp_native_entry_id",
        );
        if query.build().execute(&mut **tx).await?.rows_affected() != chunk.len() as u64 {
            return Err(DbError::Conflict("ACP transcript display identity conflicted".into()));
        }
    }
    Ok(())
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
