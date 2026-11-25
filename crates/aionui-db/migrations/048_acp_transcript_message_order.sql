-- Locate replay-enabled display histories without scanning legacy rows.
CREATE INDEX IF NOT EXISTS idx_acp_transcript_message_order
    ON messages(conversation_id) WHERE acp_transcript_source IS NOT NULL;
