-- Legacy startup seeds cannot establish per-slot permission consent.
ALTER TABLE conversations ADD COLUMN aionrs_team_permission_mode TEXT
    CHECK (aionrs_team_permission_mode IN ('default', 'auto_edit', 'yolo'));
