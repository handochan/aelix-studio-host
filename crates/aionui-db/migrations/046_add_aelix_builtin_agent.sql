-- Studio supplies the runtime at host startup. Keep the catalog identity
-- independent of installation paths and outside the short-hex custom ID space.
INSERT INTO agent_metadata
    (id, agent_id, icon, name, backend, agent_type, agent_source, sort_order, created_at, updated_at)
VALUES
    ('builtin-aelix', 'builtin-aelix', '/api/assets/logos/brand/aelix.svg', 'Aelix',
     'aelix', 'acp', 'builtin', 3090, unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(id) DO NOTHING;
