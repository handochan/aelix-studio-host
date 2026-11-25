-- Preserve a legacy row's physical key and its existing bindings when it
-- owns the reserved logical ID. Re-key the bindings in the same transaction.
CREATE TEMP TABLE IF NOT EXISTS studio_aelix_legacy_id (replacement TEXT NOT NULL);

INSERT INTO studio_aelix_legacy_id (replacement)
SELECT 'legacy-aelix-' || lower(hex(randomblob(16)))
FROM agent_metadata
WHERE agent_id = 'builtin-aelix'
  AND NOT (agent_source = 'builtin' AND backend IS 'aelix'
           AND agent_source_info IS '{"studio_aelix_seed":1}');

UPDATE agent_metadata
SET agent_id = (SELECT replacement FROM studio_aelix_legacy_id)
WHERE agent_id = 'builtin-aelix' AND EXISTS (SELECT 1 FROM studio_aelix_legacy_id);

UPDATE acp_session
SET agent_id = (SELECT replacement FROM studio_aelix_legacy_id)
WHERE agent_id = 'builtin-aelix' AND EXISTS (SELECT 1 FROM studio_aelix_legacy_id);

UPDATE assistant_definitions
SET agent_id = (SELECT replacement FROM studio_aelix_legacy_id)
WHERE agent_id = 'builtin-aelix' AND EXISTS (SELECT 1 FROM studio_aelix_legacy_id);

UPDATE assistant_definitions
SET source_ref = (SELECT replacement FROM studio_aelix_legacy_id)
WHERE source = 'generated' AND source_ref = 'builtin-aelix'
  AND EXISTS (SELECT 1 FROM studio_aelix_legacy_id);

UPDATE assistant_overlays
SET agent_id_override = (SELECT replacement FROM studio_aelix_legacy_id)
WHERE agent_id_override = 'builtin-aelix' AND EXISTS (SELECT 1 FROM studio_aelix_legacy_id);

UPDATE conversation_assistant_snapshots
SET agent_id = (SELECT replacement FROM studio_aelix_legacy_id)
WHERE agent_id = 'builtin-aelix' AND EXISTS (SELECT 1 FROM studio_aelix_legacy_id);

UPDATE conversations
SET extra = json_set(extra, '$.agent_id', (SELECT replacement FROM studio_aelix_legacy_id))
WHERE EXISTS (SELECT 1 FROM studio_aelix_legacy_id)
  AND CASE WHEN json_valid(extra) THEN json_extract(extra, '$.agent_id') END = 'builtin-aelix';

UPDATE conversations
SET extra = json_set(extra, '$.agentId', (SELECT replacement FROM studio_aelix_legacy_id))
WHERE EXISTS (SELECT 1 FROM studio_aelix_legacy_id)
  AND CASE WHEN json_valid(extra) THEN json_extract(extra, '$.agentId') END = 'builtin-aelix';

UPDATE conversations
SET extra = json_set(extra, '$.custom_agent_id', (SELECT replacement FROM studio_aelix_legacy_id))
WHERE EXISTS (SELECT 1 FROM studio_aelix_legacy_id)
  AND CASE WHEN json_valid(extra) THEN json_extract(extra, '$.custom_agent_id') END = 'builtin-aelix';

DROP TABLE studio_aelix_legacy_id;

-- The physical key is independent of the externally visible catalog ID.
INSERT INTO agent_metadata
    (id, agent_id, icon, name, backend, agent_type, agent_source, agent_source_info,
     sort_order, created_at, updated_at)
SELECT 'studio-aelix-' || lower(hex(randomblob(16))), 'builtin-aelix',
       '/api/assets/logos/brand/aelix.svg', 'Aelix', 'aelix', 'acp', 'builtin',
       '{"studio_aelix_seed":1}', 3090, unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000
WHERE NOT EXISTS (SELECT 1 FROM agent_metadata WHERE agent_id = 'builtin-aelix');
