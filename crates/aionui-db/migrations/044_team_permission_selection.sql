-- Keep explicit permission choices independent of conversation runtime/MCP JSON.
-- Older Team modes lack per-slot consent, so do not backfill these columns.
ALTER TABLE acp_session ADD COLUMN team_permission_mode TEXT
    CHECK (team_permission_mode IN ('default', 'yolo'));
ALTER TABLE teams ADD COLUMN aelix_permission_mode TEXT
    CHECK (aelix_permission_mode IN ('default', 'yolo'));
