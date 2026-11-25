use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn builtin_seed_is_path_independent_and_preserves_existing_custom_aelix() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());
    let seed = repo.get("builtin-aelix").await.unwrap().expect("seeded builtin Aelix");
    let logical_id: String = sqlx::query_scalar("SELECT agent_id FROM agent_metadata WHERE id = 'builtin-aelix'")
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
        "DELETE FROM agent_metadata WHERE id = 'builtin-aelix';
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
