use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn builtin_seed_is_path_independent_and_preserves_existing_custom_aelix() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let seed = repo.get("builtin-aelix").await.unwrap().expect("seeded builtin Aelix");
    let logical_id: String = sqlx::query_scalar("SELECT agent_id FROM agent_metadata WHERE agent_id = 'builtin-aelix'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(logical_id, seed.id);
    assert_eq!(seed.name, "Aelix");
    assert_eq!(seed.icon.as_deref(), Some("/api/assets/logos/brand/aelix.svg"));
    assert_eq!(seed.backend.as_deref(), Some("aelix"));
    assert_eq!(seed.agent_source, "builtin");
    assert_eq!(seed.agent_type, "acp");
    assert!(seed.enabled);
    assert!(seed.user_id.is_none());
    assert!(seed.command.is_none());
    assert!(seed.args.is_none());
    assert!(seed.env.is_none());
    assert!(seed.command_override.is_none());
    assert!(seed.env_override.is_none());

    sqlx::raw_sql(
        "DELETE FROM agent_metadata WHERE agent_id = 'builtin-aelix';
         INSERT INTO agent_metadata
             (id, agent_id, user_id, name, backend, agent_type, agent_source, command, args, created_at, updated_at)
         VALUES ('ea21421a', 'ea21421a', 'system_default_user', 'Aelix · example/model', 'aelix', 'acp',
                 'custom', '/legacy/launcher', '[\"--model=example\"]', 1, 1);",
    )
    .execute(db.pool())
    .await
    .unwrap();

    for _ in 0..2 {
        sqlx::raw_sql(include_str!("../migrations/046_add_aelix_builtin_agent.sql"))
            .execute(db.pool())
            .await
            .unwrap();
    }

    let mut aelix_rows: Vec<_> = repo
        .list_all()
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.backend.as_deref() == Some("aelix"))
        .collect();
    aelix_rows.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(aelix_rows.len(), 2);
    assert_eq!(aelix_rows[0].id, seed.id);
    assert_eq!(aelix_rows[1].id, "ea21421a");
    assert_eq!(aelix_rows[1].agent_source, "custom");
    assert_eq!(aelix_rows[1].command.as_deref(), Some("/legacy/launcher"));
    assert_eq!(aelix_rows[1].args.as_deref(), Some(r#"["--model=example"]"#));
}

async fn collision_upgrade(physical_id: &str, old_logical_id: &str) {
    use sqlx::migrate::Migrator;
    use std::borrow::Cow;
    use std::path::Path;

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let v45 = Migrator {
        migrations: Cow::Owned(
            full.migrations
                .iter()
                .filter(|migration| migration.version <= 45)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    v45.run(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO agent_metadata
        (id,agent_id,user_id,name,backend,agent_type,agent_source,command,created_at,updated_at)
        VALUES (?1,?2,'system_default_user','Legacy Aelix','aelix','acp','custom','legacy-command',1,1)",
    )
    .bind(physical_id)
    .bind(old_logical_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id,user_id,name,type,status,pinned,extra,created_at,updated_at)
        VALUES ('legacy-conversation','system_default_user','Legacy conversation','acp','pending',0,
                json_object('agent_id',?1,'agentId',?1,'custom_agent_id',?1,'note','builtin-aelix'),1,1)",
    )
    .bind(old_logical_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO acp_session (conversation_id,agent_source,agent_id)
        VALUES ('legacy-conversation','custom',?1)",
    )
    .bind(old_logical_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_definitions
        (id,user_id,assistant_id,source,owner_type,source_ref,name,agent_id,rule_resource_type,
         default_model_mode,default_permission_mode,default_skills_mode,default_mcps_mode,created_at,updated_at)
        VALUES ('legacy-assistant','system_default_user','legacy-assistant','generated','user',?1,'Legacy',?1,
                'none','auto','auto','auto','auto',1,1)",
    )
    .bind(old_logical_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO assistant_overlays
        (user_id,assistant_definition_id,agent_id_override,created_at,updated_at)
        VALUES ('system_default_user','legacy-assistant',?1,1,1)",
    )
    .bind(old_logical_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_assistant_snapshots
        (conversation_id,assistant_definition_id,assistant_id,assistant_source,agent_id,
         default_model_mode,default_permission_mode,default_skills_mode,default_mcps_mode,created_at,updated_at)
        VALUES ('legacy-conversation','legacy-assistant','legacy-assistant','generated',?1,
                'auto','auto','auto','auto',1,1)",
    )
    .bind(old_logical_id)
    .execute(&pool)
    .await
    .unwrap();

    full.run(&pool).await.unwrap();
    let staff: (String, String) =
        sqlx::query_as("SELECT id, agent_source FROM agent_metadata WHERE agent_id = 'builtin-aelix'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(staff.1, "builtin");
    assert_ne!(staff.0, "builtin-aelix");
    let legacy: (String, String) = sqlx::query_as("SELECT agent_id, command FROM agent_metadata WHERE id = ?")
        .bind(physical_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(legacy.1, "legacy-command");
    if old_logical_id == "builtin-aelix" {
        assert_ne!(legacy.0, old_logical_id);
    } else {
        assert_eq!(legacy.0, old_logical_id);
    }
    for sql in [
        "SELECT agent_id FROM acp_session WHERE conversation_id = 'legacy-conversation'",
        "SELECT agent_id FROM conversation_assistant_snapshots WHERE conversation_id = 'legacy-conversation'",
        "SELECT agent_id FROM assistant_definitions WHERE id = 'legacy-assistant'",
        "SELECT source_ref FROM assistant_definitions WHERE id = 'legacy-assistant'",
        "SELECT agent_id_override FROM assistant_overlays WHERE assistant_definition_id = 'legacy-assistant'",
        "SELECT json_extract(extra, '$.agent_id') FROM conversations WHERE id = 'legacy-conversation'",
        "SELECT json_extract(extra, '$.agentId') FROM conversations WHERE id = 'legacy-conversation'",
        "SELECT json_extract(extra, '$.custom_agent_id') FROM conversations WHERE id = 'legacy-conversation'",
    ] {
        let target: String = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
        assert_eq!(target, legacy.0, "{sql}");
    }
    let note: String = sqlx::query_scalar("SELECT json_extract(extra, '$.note') FROM conversations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(note, "builtin-aelix");
}

#[tokio::test]
async fn v45_upgrade_preserves_a_physical_id_collision() {
    collision_upgrade("builtin-aelix", "legacy-custom-aelix").await;
}

#[tokio::test]
async fn v45_upgrade_rekeys_a_logical_id_collision_and_its_references() {
    collision_upgrade("old-row-key", "builtin-aelix").await;
}

#[tokio::test]
async fn v45_upgrade_rekeys_a_combined_collision_and_its_references() {
    collision_upgrade("builtin-aelix", "builtin-aelix").await;
}
