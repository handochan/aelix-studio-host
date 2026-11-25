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
    // Seed the pre-transcript schema before running the current repository.
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, msg_id, type, content, position, status, hidden, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&hidden.id)
    .bind(&hidden.conversation_id)
    .bind(&hidden.msg_id)
    .bind(&hidden.r#type)
    .bind(&hidden.content)
    .bind(&hidden.position)
    .bind(&hidden.status)
    .bind(hidden.hidden)
    .bind(hidden.created_at)
    .execute(&pool)
    .await
    .unwrap();

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

#[tokio::test]
async fn guarded_head_fork_binds_inherited_hidden_entry_to_the_child_snapshot() {
    let (repo, db) = setup().await;
    create(&repo, "child").await;
    let source = row("hidden-parent", true, "same");
    repo.insert_message(OWNER, &source).await.unwrap();
    repo.bind_acp_hidden_input(OWNER, "conversation", &source.id, &native("inherited"))
        .await
        .unwrap();
    assert_eq!(
        repo.copy_messages_up_to(OWNER, "conversation", "child", (source.created_at, &source.id), true)
            .await
            .unwrap(),
        1
    );
    let inherited: (bool, Option<String>, String) = sqlx::query_as(
        "SELECT hidden, acp_native_session_id, acp_native_entry_id FROM messages WHERE conversation_id = 'child'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(inherited, (true, None, "inherited".to_owned()));

    let child = |id, entry| {
        let mut value = replay(id, entry, "same");
        value.message.conversation_id = "child".into();
        value.native.as_mut().unwrap().session_id = "native-child".into();
        value
    };
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "child", "child-acp", vec![child("cli", "appended")])
            .await
            .unwrap(),
        AcpTranscriptApply::UnidentifiedHiddenInput
    );
    let snapshot = || vec![child("inherited-row", "inherited"), child("cli", "appended")];
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "child", "child-acp", snapshot())
            .await
            .unwrap(),
        AcpTranscriptApply::Applied {
            displayed: 2,
            archived: 1
        }
    );
    let rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT id, hidden FROM messages WHERE conversation_id = 'child' ORDER BY created_at, id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(rows, vec![("inherited-row".into(), true), ("cli".into(), false)]);
    let archived_key: (String, String) = sqlx::query_as(
        "SELECT acp_native_session_id, acp_native_entry_id FROM acp_transcript_display_archive WHERE conversation_id = 'child'",
    )
    .fetch_one(db.pool()).await.unwrap();
    assert_eq!(archived_key, ("native-child".into(), "inherited".into()));
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "child", "child-acp", snapshot())
            .await
            .unwrap(),
        AcpTranscriptApply::Unchanged
    );
    let parent_key: String =
        sqlx::query_scalar("SELECT acp_native_session_id FROM messages WHERE id = 'hidden-parent'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(parent_key, "native");
}

#[tokio::test]
async fn appending_to_sixty_thousand_rows_keeps_all_existing_cursor_slots() {
    let directory = tempfile::tempdir().unwrap();
    let db = aionui_db::init_database(&directory.path().join("append.db"))
        .await
        .unwrap();
    let repo = SqliteConversationRepository::new(db.pool().clone());
    create(&repo, "conversation").await;

    sqlx::query(
        "WITH RECURSIVE ints(n) AS (VALUES(1) UNION ALL SELECT n + 1 FROM ints WHERE n < 60000)
         INSERT INTO messages (id, conversation_id, msg_id, type, content, position, status, hidden, created_at,
                               acp_transcript_source, acp_native_session_id, acp_native_entry_id)
         SELECT 'row-' || n, 'conversation', 'row-' || n, 'text', '{\"content\":\"same\"}', 'right', 'finish', 0, n,
                'native', 'native', 'entry-' || n
         FROM ints",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("UPDATE messages SET backend_turn_id = 'head-reference' WHERE id = 'row-60000'")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE display_writes (operation TEXT, message_id TEXT);
         CREATE TRIGGER display_update AFTER UPDATE ON messages
           BEGIN INSERT INTO display_writes VALUES ('update', new.id); END;
         CREATE TRIGGER display_insert AFTER INSERT ON messages
           BEGIN INSERT INTO display_writes VALUES ('insert', new.id); END;
         CREATE TRIGGER display_delete AFTER DELETE ON messages
           BEGIN INSERT INTO display_writes VALUES ('delete', old.id); END;",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let snapshot = || {
        (1..=60001)
            .map(|n| replay(&format!("row-{n}"), &format!("entry-{n}"), "same"))
            .collect()
    };
    let rows = snapshot();
    let started = std::time::Instant::now();
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "conversation", "acp", rows)
            .await
            .unwrap(),
        AcpTranscriptApply::Applied {
            displayed: 60001,
            archived: 0
        }
    );
    eprintln!("60,000 rows plus one append, end to end: {:?}", started.elapsed());
    let writes: Vec<(String, String)> = sqlx::query_as("SELECT operation, message_id FROM display_writes")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(writes, vec![("insert".into(), "row-60001".into())]);
    let moved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE created_at != CAST(substr(id, 5) AS INTEGER)")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(moved, 0);
    let heads: Vec<String> = sqlx::query_scalar(
        "SELECT backend_turn_id FROM messages WHERE id IN ('row-60000', 'row-60001') ORDER BY created_at",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(heads, ["head-reference", "head-reference"]);
    assert_eq!(
        repo.apply_acp_transcript(OWNER, "conversation", "acp", snapshot())
            .await
            .unwrap(),
        AcpTranscriptApply::Unchanged
    );
}

#[tokio::test]
async fn guarded_fork_keeps_the_display_reset_of_its_native_prefix() {
    use aionui_db::{IAcpSessionRepository, SqliteAcpSessionRepository};

    let (repo, db) = setup().await;
    let sessions = SqliteAcpSessionRepository::new(db.pool().clone());
    create(&repo, "child").await;
    create(&repo, "grandchild").await;
    for (conversation, session) in [
        ("conversation", Some("parent-acp")),
        ("child", None),
        ("grandchild", None),
    ] {
        sqlx::query("INSERT INTO acp_session (conversation_id, agent_source, agent_id, session_id) VALUES (?, 'builtin', 'fixture', ?)")
            .bind(conversation).bind(session).execute(db.pool()).await.unwrap();
    }

    let hidden = row("old-hidden", true, "removed from the local display");
    repo.insert_message(OWNER, &hidden).await.unwrap();
    repo.delete_messages_by_conversation(OWNER, "conversation")
        .await
        .unwrap();
    let recent = row("visible", false, "after reset");
    repo.insert_message(OWNER, &recent).await.unwrap();
    repo.copy_messages_up_to(OWNER, "conversation", "child", (recent.created_at, &recent.id), true)
        .await
        .unwrap();
    sessions
        .update_session_id_for_user(OWNER, "child", "child-acp")
        .await
        .unwrap();
    repo.copy_messages_up_to(OWNER, "child", "grandchild", (i64::MAX, "unused"), true)
        .await
        .unwrap();
    sessions
        .update_session_id_for_user(OWNER, "grandchild", "grandchild-acp")
        .await
        .unwrap();

    for (conversation, session) in [("child", "child-acp"), ("grandchild", "grandchild-acp")] {
        let result = repo
            .apply_acp_transcript(
                OWNER,
                conversation,
                session,
                vec![replay("old-native", "old-hidden", "removed")],
            )
            .await
            .unwrap();
        assert_eq!(result, AcpTranscriptApply::DisplayReset);
        let active: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ?")
            .bind(conversation)
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(active, [recent.content.clone()]);
    }
}

#[tokio::test]
async fn live_input_stays_on_latest_page_after_a_five_thousand_row_replay_append() {
    let (repo, _db) = setup().await;
    let snapshot = |count| {
        (1..=count)
            .map(|n| replay(&format!("replay-{n}"), &format!("entry-{n}"), "history"))
            .collect()
    };
    repo.apply_acp_transcript(OWNER, "conversation", "acp", snapshot(2))
        .await
        .unwrap();
    let prefix = visible_history(&repo).await;
    assert_eq!(prefix.len(), 2);
    let wall_time = prefix[1].created_at;

    repo.apply_acp_transcript(OWNER, "conversation", "acp", snapshot(5002))
        .await
        .unwrap();
    for original in &prefix {
        assert_eq!(
            repo.get_message(OWNER, "conversation", &original.id)
                .await
                .unwrap()
                .as_ref(),
            Some(original)
        );
    }
    let tail = repo
        .get_message(OWNER, "conversation", "replay-5002")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tail.created_at, wall_time + 5000);

    // As in /messages, the caller starts with wall time and publishes the
    // timestamp actually committed by the repository.
    let mut live = row("live-input", false, "next Host input");
    live.created_at = wall_time + 1;
    let committed = repo.insert_message_returning_created_at(OWNER, &live).await.unwrap();
    assert_eq!(committed, tail.created_at + 1);
    let page = visible_history(&repo).await;
    assert_eq!(&page[page.len() - 2], &tail);
    assert_eq!(page.last().unwrap().id, "live-input");
    assert_eq!(page.last().unwrap().created_at, committed);

    // Streaming upserts share the same order and preserve the existing
    // display slot when the tool updates.
    let mut tool = row("live-tool", false, "tool progress");
    tool.r#type = "acp_tool_call".into();
    tool.position = Some("left".into());
    tool.created_at = committed;
    repo.upsert_message(OWNER, &tool).await.unwrap();
    tool.status = Some("error".into());
    repo.upsert_message(OWNER, &tool).await.unwrap();
    let page = visible_history(&repo).await;
    assert_eq!(page[page.len() - 2].id, "live-input");
    assert_eq!(page.last().unwrap().id, "live-tool");
    assert_eq!(page.last().unwrap().created_at, committed + 1);
    assert_eq!(page.last().unwrap().status.as_deref(), Some("error"));

    // The durable head still governs a fresh insertion when its caller
    // clock has crossed the replay tail but trails a live tool slot.
    let mut next = row("next-input", false, "another Host input");
    next.created_at = committed;
    repo.insert_message(OWNER, &next).await.unwrap();
    let page = visible_history(&repo).await;
    assert_eq!(page[page.len() - 2].id, "live-tool");
    assert_eq!(page.last().unwrap().id, "next-input");
    assert_eq!(page.last().unwrap().created_at, committed + 2);

    // Conversation-list recency follows activity time even when a display
    // time from an upsert stretches well into the future.
    tool.created_at = aionui_common::now_ms() + 60_000;
    repo.upsert_message(OWNER, &tool).await.unwrap();
    let conversation = repo.get(OWNER, "conversation").await.unwrap().unwrap();
    assert!(conversation.updated_at <= aionui_common::now_ms());
}
