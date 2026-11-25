use aionui_db::models::{ConversationRow, MessageRow};
use aionui_db::{
    AcpTranscriptApply, AcpTranscriptRow, DbError, IConversationRepository, MessagePageDirection, MessagePageParams,
    NativeMessageKey, SqliteConversationRepository, SqlitePool, init_database_memory,
};
use serde_json::json;

const OWNER: &str = "system_default_user";

async fn setup() -> (SqliteConversationRepository, aionui_db::Database) {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    create(&repo, "conversation").await;
    (repo, db)
}

async fn create(repo: &SqliteConversationRepository, id: &str) {
    repo.create(&ConversationRow {
        id: id.into(),
        user_id: OWNER.into(),
        name: id.into(),
        r#type: "acp".into(),
        extra: "{}".into(),
        model: None,
        status: Some("finished".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
        project_id: None,
        folder_id: None,
        name_source: None,
    })
    .await
    .unwrap();
}

fn row(id: &str, hidden: bool, content: &str) -> MessageRow {
    MessageRow {
        id: id.into(),
        conversation_id: "conversation".into(),
        msg_id: Some(id.into()),
        r#type: "text".into(),
        content: json!({"content": content}).to_string(),
        position: Some("right".into()),
        status: Some("finish".into()),
        hidden,
        created_at: 2,
        backend_turn_id: None,
    }
}

fn native(entry: &str) -> NativeMessageKey {
    NativeMessageKey {
        session_id: "native".into(),
        entry_id: entry.into(),
    }
}

fn replay(id: &str, entry: &str, content: &str) -> AcpTranscriptRow {
    AcpTranscriptRow {
        message: row(id, false, content),
        native: Some(native(entry)),
    }
}

async fn visible_history(repo: &SqliteConversationRepository) -> Vec<MessageRow> {
    repo.list_messages_page(
        OWNER,
        "conversation",
        &MessagePageParams {
            limit: 50,
            direction: MessagePageDirection::InitialLatest,
        },
    )
    .await
    .unwrap()
    .items
}

async fn archived(pool: &SqlitePool) -> Vec<(String, String, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT message_id, content, hidden, acp_native_entry_id
         FROM acp_transcript_display_archive ORDER BY message_id, archive_id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn migration_preserves_legacy_hidden_rows_and_display() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    // The production migrator sets both pragmas outside its schema
    // transactions so pre-existing table renames retain valid FKs.
    sqlx::query("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON")
        .execute(&pool)
        .await
        .unwrap();
    let migrator = sqlx::migrate!("./migrations");
    let mut legacy = sqlx::migrate::Migrator {
        migrations: migrator
            .iter()
            .filter(|migration| migration.version < 47)
            .cloned()
            .collect(),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    legacy.run(&pool).await.unwrap();
    sqlx::query("INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, 'admin', '', 1, 1)")
        .bind(OWNER).execute(&pool).await.unwrap();
    let repo = SqliteConversationRepository::new(pool.clone());
    create(&repo, "conversation").await;
    let hidden = row("old-hidden", true, "same text");
    repo.insert_message(OWNER, &hidden).await.unwrap();

    legacy.migrations = migrator.migrations;
    legacy.run(&pool).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON; PRAGMA legacy_alter_table = OFF")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(visible_history(&repo).await, vec![hidden.clone()]);
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "conversation", "acp", vec![replay("new", "cli", "same text")])
            .await
            .unwrap(),
        AcpTranscriptApply::UnidentifiedHiddenInput,
    );
    assert_eq!(visible_history(&repo).await, vec![hidden]);
    assert!(archived(&pool).await.is_empty());
}

#[tokio::test]
async fn hidden_receipt_controls_visibility_with_identical_cli_input_and_old_echoes() {
    let (repo, db) = setup().await;
    let hidden = row("new-hidden", true, "same text");
    let mut legacy = row("old-visible", false, "same text");
    legacy.backend_turn_id = Some("verified-by-adapter".into());
    let mut app_message = row("app", false, "a teammate message");
    app_message.content = json!({"content": "a teammate message", "teammate_message": true}).to_string();
    for message in [&hidden, &legacy, &app_message] {
        repo.insert_message(OWNER, message).await.unwrap();
    }
    repo.bind_acp_hidden_input(OWNER, "conversation", &hidden.id, &native("stored-hidden"))
        .await
        .unwrap();
    let snapshot = || {
        vec![
            replay("native-old", "old", "same text"),
            replay("native-hidden", "stored-hidden", "same text"),
            replay("native-cli", "cli", "same text"),
        ]
    };
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "conversation", "acp", snapshot())
            .await
            .unwrap(),
        AcpTranscriptApply::Applied {
            displayed: 3,
            archived: 2
        },
    );
    let history = visible_history(&repo).await;
    assert_eq!(
        history
            .iter()
            .filter(|message| message.id != app_message.id)
            .map(|message| (message.id.as_str(), message.hidden))
            .collect::<Vec<_>>(),
        vec![("native-old", false), ("native-hidden", true), ("native-cli", false)],
    );
    assert!(history.contains(&app_message));
    assert_eq!(
        history.last().unwrap().backend_turn_id.as_deref(),
        Some("verified-by-adapter")
    );
    assert_eq!(
        archived(db.pool()).await,
        vec![
            (hidden.id, hidden.content, true, Some("stored-hidden".into())),
            (legacy.id, legacy.content, false, None),
        ]
    );

    // Reopening the repository models restart. The ids and cursor slots
    // are unchanged when no new native or adapter history arrived.
    let restarted = SqliteConversationRepository::new(db.pool().clone());
    assert_eq!(
        restarted
            .apply_acp_transcript(OWNER, "conversation", "acp", snapshot())
            .await
            .unwrap(),
        AcpTranscriptApply::Unchanged,
    );
    assert_eq!(visible_history(&restarted).await, history);

    // A branch that omits the hidden entry can be loaded and then replaced
    // again. Its hidden receipt survives in the first display archive.
    restarted
        .apply_acp_transcript(
            OWNER,
            "conversation",
            "acp",
            vec![replay("native-cli", "cli", "same text")],
        )
        .await
        .unwrap();
    restarted
        .apply_acp_transcript(OWNER, "conversation", "acp", snapshot())
        .await
        .unwrap();
    assert!(
        restarted
            .get_message(OWNER, "conversation", "native-hidden")
            .await
            .unwrap()
            .unwrap()
            .hidden
    );
}

#[tokio::test]
async fn mixed_legacy_and_bound_hidden_inputs_keep_the_db_view() {
    let (repo, db) = setup().await;
    let unbound = row("old-hidden", true, "unbound");
    let bound = row("new-hidden", true, "bound");
    let public = row("public", false, "public");
    for message in [&unbound, &bound, &public] {
        repo.insert_message(OWNER, message).await.unwrap();
    }
    repo.bind_acp_hidden_input(OWNER, "conversation", &bound.id, &native("new"))
        .await
        .unwrap();
    let prior = visible_history(&repo).await;
    assert_eq!(
        repo.apply_acp_transcript(
            OWNER,
            "conversation",
            "acp",
            vec![
                replay("bound-native", "new", "bound"),
                replay("from-cli", "cli", "public"),
            ]
        )
        .await
        .unwrap(),
        AcpTranscriptApply::UnidentifiedHiddenInput,
    );
    assert_eq!(visible_history(&repo).await, prior);
    assert!(archived(db.pool()).await.is_empty());
}

#[tokio::test]
async fn in_flight_echo_survives_load_and_archive_is_atomic_with_active_history() {
    let (repo, db) = setup().await;
    let old = row("prior", false, "prior");
    let pending = row("pending", true, "just submitted");
    let _guard = repo.protect_acp_transcript_input(&pending.id);
    repo.insert_message(OWNER, &old).await.unwrap();
    repo.insert_message(OWNER, &pending).await.unwrap();

    let result = repo
        .apply_acp_transcript(
            "another-user",
            "conversation",
            "acp",
            vec![replay("first", "1", "first")],
        )
        .await;
    assert!(matches!(result, Err(DbError::NotFound(_))));
    create(&repo, "another-conversation").await;
    let mut collision = row("collision", false, "elsewhere");
    collision.conversation_id = "another-conversation".into();
    repo.insert_message(OWNER, &collision).await.unwrap();
    let result = repo
        .apply_acp_transcript(
            OWNER,
            "conversation",
            "acp",
            vec![replay("first", "1", "first"), replay("collision", "2", "collision")],
        )
        .await;
    assert!(matches!(result, Err(DbError::Conflict(_))));
    assert_eq!(visible_history(&repo).await, vec![pending.clone(), old.clone()]);
    assert!(archived(db.pool()).await.is_empty());

    repo.apply_acp_transcript(OWNER, "conversation", "acp", vec![replay("first", "1", "first")])
        .await
        .unwrap();
    let history = visible_history(&repo).await;
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].id, "first");
    assert_eq!(history[1], pending);
    assert_eq!(archived(db.pool()).await, vec![(old.id, old.content, false, None)]);
}

#[tokio::test]
async fn explicit_display_reset_fences_the_existing_acp_binding() {
    let (repo, db) = setup().await;
    sqlx::query(
        "INSERT INTO acp_session (conversation_id, agent_source, agent_id, session_id)
         VALUES ('conversation', 'builtin', 'fixture', 'acp')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let hidden = row("hidden-before-reset", true, "hidden");
    repo.insert_message(OWNER, &hidden).await.unwrap();
    repo.bind_acp_hidden_input(OWNER, "conversation", &hidden.id, &native("hidden"))
        .await
        .unwrap();
    repo.apply_acp_transcript(
        OWNER,
        "conversation",
        "acp",
        vec![replay("hidden-row", "hidden", "hidden")],
    )
    .await
    .unwrap();
    assert!(visible_history(&repo).await[0].hidden);

    repo.delete_messages_by_conversation(OWNER, "conversation")
        .await
        .unwrap();
    assert_eq!(
        repo.apply_acp_transcript(
            OWNER,
            "conversation",
            "acp",
            vec![replay("hidden-row", "hidden", "hidden")]
        )
        .await
        .unwrap(),
        AcpTranscriptApply::DisplayReset,
    );
    assert!(visible_history(&repo).await.is_empty());
    assert!(archived(db.pool()).await.is_empty());

    let fresh = repo
        .apply_acp_transcript(
            OWNER,
            "conversation",
            "new-session",
            vec![replay("new-row", "new", "new")],
        )
        .await
        .unwrap();
    assert_eq!(
        fresh,
        AcpTranscriptApply::Applied {
            displayed: 1,
            archived: 0
        }
    );
}

#[tokio::test]
async fn replay_preserves_display_head_after_app_messages() {
    let (repo, _db) = setup().await;
    let mut early = row("early", false, "early");
    early.created_at = 100;
    let mut tip = row("app-tip", false, "app tip");
    tip.r#type = "tips".into();
    tip.created_at = 500;
    let mut old_head = row("old-head", false, "answer");
    old_head.created_at = 1000;
    old_head.backend_turn_id = Some("existing-fork-head".into());
    for message in [&early, &tip, &old_head] {
        repo.insert_message(OWNER, message).await.unwrap();
    }
    repo.apply_acp_transcript(
        OWNER,
        "conversation",
        "acp",
        vec![replay("new-early", "1", "early"), replay("new-head", "2", "answer")],
    )
    .await
    .unwrap();
    let history = visible_history(&repo).await;
    assert_eq!(history[1], tip);
    assert_eq!(history[2].id, "new-head");
    assert_eq!(history[2].backend_turn_id.as_deref(), Some("existing-fork-head"));
}
