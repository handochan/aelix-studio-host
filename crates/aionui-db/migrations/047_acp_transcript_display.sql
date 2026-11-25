-- Added only to recognize native-backed display rows and stored user receipts.
-- Existing rows keep their original representation and have no native key.
ALTER TABLE messages ADD COLUMN acp_transcript_source TEXT CHECK (acp_transcript_source IN ('native', 'adapter'));
ALTER TABLE messages ADD COLUMN acp_native_session_id TEXT;
ALTER TABLE messages ADD COLUMN acp_native_entry_id TEXT;

CREATE TABLE IF NOT EXISTS acp_transcript_display_archive (
    archive_id            INTEGER PRIMARY KEY,
    conversation_id       TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    message_id            TEXT NOT NULL,
    msg_id                TEXT,
    type                  TEXT NOT NULL,
    content               TEXT NOT NULL,
    position              TEXT,
    status                TEXT,
    hidden                INTEGER NOT NULL,
    created_at            INTEGER NOT NULL,
    backend_turn_id       TEXT,
    acp_transcript_source TEXT,
    acp_native_session_id TEXT,
    acp_native_entry_id   TEXT
);
CREATE INDEX IF NOT EXISTS idx_acp_transcript_archive_conversation
    ON acp_transcript_display_archive(conversation_id);

-- The Host reset clears the local display while the ACP session can still
-- resume its native context. Keep that explicit reset across future loads.
CREATE TABLE IF NOT EXISTS acp_transcript_display_resets (
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    session_id      TEXT NOT NULL,
    PRIMARY KEY (conversation_id, session_id)
);
