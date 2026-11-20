mod common;

use std::path::Path;
use std::time::Duration;

use aionui_app::{AppConfig, AppServices, create_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{body_json, delete_with_token, get_with_token, json_with_token, setup_and_login};

const PROVIDER_ID: &str = "fixture-team-aionrs";

struct TeamMember {
    team_id: String,
    slot_id: String,
    conversation_id: String,
    token: String,
    csrf: String,
}

impl TeamMember {
    async fn request(&self, app: &Router, request: Request<Body>, expected: StatusCode) -> Value {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = body_json(response).await;
        assert_eq!(status, expected, "{body}");
        body
    }

    async fn get(&self, app: &Router, path: &str) -> Value {
        self.request(app, get_with_token(path, &self.token), StatusCode::OK)
            .await["data"]
            .clone()
    }

    async fn change(&self, app: &Router, method: &str, path: &str, body: Value, status: StatusCode) -> Value {
        self.request(
            app,
            json_with_token(method, path, body, &self.token, &self.csrf),
            status,
        )
        .await["data"]
            .clone()
    }

    fn slot_path(&self) -> String {
        format!("/api/teams/{}/agents/{}", self.team_id, self.slot_id)
    }

    fn config_path(&self) -> String {
        format!(
            "/api/teams/{}/conversations/{}/config-options",
            self.team_id, self.conversation_id
        )
    }

    async fn attach(&self, app: &Router) {
        self.change(
            app,
            "POST",
            &format!("{}/attach", self.slot_path()),
            json!({}),
            StatusCode::OK,
        )
        .await;
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(get_with_token(&self.config_path(), &self.token))
                    .await
                    .unwrap();
                if response.status() == StatusCode::OK {
                    break;
                }
                assert_eq!(response.status(), StatusCode::CONFLICT, "{}", body_json(response).await);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("Aionrs member became ready");
    }

    async fn restart(&self, app: &Router) {
        self.change(
            app,
            "POST",
            &format!("{}/runtime/restart", self.slot_path()),
            json!({}),
            StatusCode::OK,
        )
        .await;
    }

    async fn select_mode(&self, app: &Router, mode: &str) {
        let result = self
            .change(
                app,
                "PUT",
                &format!("{}/mode", self.config_path()),
                json!({"value": mode}),
                StatusCode::OK,
            )
            .await;
        assert_eq!(result["confirmation"], "observed");
        assert_eq!(result["config_options"][0]["current_value"], mode);
    }

    async fn mode(&self, app: &Router) -> Value {
        self.get(app, &self.config_path()).await["config_options"][0]["current_value"].clone()
    }

    async fn stop_team(&self, app: &Router) {
        self.request(
            app,
            delete_with_token(&format!("/api/teams/{}/session", self.team_id), &self.token, &self.csrf),
            StatusCode::OK,
        )
        .await;
    }

    async fn turn(&self, app: &Router, content: &str) {
        self.change(
            app,
            "POST",
            &format!("{}/messages", self.slot_path()),
            json!({"content": content}),
            StatusCode::OK,
        )
        .await;
    }

    async fn require_write_approval(&self, app: &Router, file: &Path) {
        let confirmations = format!("/api/conversations/{}/confirmations", self.conversation_id);
        let request = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(!file.exists(), "Write executed without asking for permission");
                let pending = self.get(app, &confirmations).await;
                if let Some(request) = pending.as_array().unwrap().first() {
                    return request.clone();
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the Write tool requested approval");
        assert!(!file.exists(), "Write ran before approval");
        assert_eq!(request["action"], "Write");
        let call_id = request["call_id"].as_str().unwrap();
        self.change(
            app,
            "POST",
            &format!("{confirmations}/{call_id}/confirm"),
            json!({"msg_id": request["id"], "data": {"value": "proceed_once"}}),
            StatusCode::OK,
        )
        .await;
    }
}

async fn mock_provider(file: &Path) -> MockServer {
    let server = MockServer::start().await;
    let args = json!({"file_path": file, "content": "approved team write"}).to_string();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            let last = body["messages"].as_array().unwrap().last().unwrap();
            let (delta, finish_reason) = if last["role"] == "tool" {
                (json!({"content": "Done"}), "stop")
            } else {
                assert!(
                    body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|tool| tool["function"]["name"] == "Write")
                );
                (
                    json!({"tool_calls": [{
                        "index": 0, "id": "call_team_write", "type": "function",
                        "function": {"name": "Write", "arguments": args}
                    }]}),
                    "tool_calls",
                )
            };
            let chunk = json!({
                "id": "chatcmpl-team", "object": "chat.completion.chunk", "created": 1,
                "model": "gpt-4o", "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
            });
            ResponseTemplate::new(200).set_body_raw(format!("data: {chunk}\n\ndata: [DONE]\n\n"), "text/event-stream")
        })
        .mount(&server)
        .await;
    server
}

async fn create_member(
    app: &mut Router,
    services: &AppServices,
    workspace: &Path,
    provider: &MockServer,
) -> TeamMember {
    let (token, csrf) = setup_and_login(app, services, "admin", "StrongP@ss1").await;
    register_provider(app, &token, &csrf, provider).await;

    let assistant_request = json_with_token(
        "POST",
        "/api/assistants",
        json!({"id": "team-aionrs", "name": "Aionrs test assistant", "agent_id": "632f31d2"}),
        &token,
        &csrf,
    );
    let response = app.clone().oneshot(assistant_request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED, "{}", body_json(response).await);

    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/teams",
            json!({"name": "Team approvals", "workspace": workspace, "agents": [
                {"name": "Leader", "role": "lead", "assistant_id": "team-aionrs", "model": "gpt-4o"},
                {"name": "Writer", "role": "teammate", "assistant_id": "team-aionrs", "model": "gpt-4o"}
            ]}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    let status = response.status();
    let body = body_json(response).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let team = &body["data"];
    let member = team["assistants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|slot| slot["name"] == "Writer")
        .unwrap();
    TeamMember {
        team_id: team["id"].as_str().unwrap().to_owned(),
        slot_id: member["slot_id"].as_str().unwrap().to_owned(),
        conversation_id: member["conversation_id"].as_str().unwrap().to_owned(),
        token,
        csrf,
    }
}

async fn register_provider(app: &Router, token: &str, csrf: &str, provider: &MockServer) {
    let request = json_with_token(
        "POST",
        "/api/providers",
        json!({
            "id": PROVIDER_ID, "platform": "openai", "name": "Team approval fixture",
            "base_url": provider.uri(), "api_key": "fixture-key", "models": ["gpt-4o"],
            "model_settings": {"gpt-4o": {"openai_api_mode": "chat_completions"}}
        }),
        token,
        csrf,
    );
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED, "{}", body_json(response).await);
}

async fn expect_written(file: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !file.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("Write created the file");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "approved team write");
}

async fn start_host(db: aionui_db::Database, data: &Path) -> (Router, AppServices) {
    start_host_with_binary(db, data, Path::new(env!("CARGO_BIN_EXE_aelix-studio-host"))).await
}

async fn start_host_with_binary(db: aionui_db::Database, data: &Path, binary: &Path) -> (Router, AppServices) {
    let config = AppConfig {
        data_dir: data.to_owned(),
        work_dir: data.to_owned(),
        ..Default::default()
    };
    let services = AppServices::from_config_with_backend_binary_path(db, &config, binary.to_owned())
        .await
        .unwrap();
    (create_router(&services).await.unwrap(), services)
}

#[tokio::test]
async fn aionrs_team_member_requests_write_approval_after_restart() {
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("restart-approval.txt");
    let provider = mock_provider(&file).await;
    let db = aionui_db::init_database_memory().await.unwrap();
    let (mut app, services) = start_host(db, data.path()).await;
    let member = create_member(&mut app, &services, data.path(), &provider).await;

    member.attach(&app).await;
    assert_eq!(member.mode(&app).await, "default");
    member
        .change(
            &app,
            "POST",
            &format!("/api/teams/{}/session-mode", member.team_id),
            json!({"mode": "yolo"}),
            StatusCode::OK,
        )
        .await;
    member.restart(&app).await;
    assert_eq!(member.mode(&app).await, "yolo");
    member.select_mode(&app, "default").await;
    member.restart(&app).await;
    member.turn(&app, "Write the test file").await;
    member.require_write_approval(&app, &file).await;
    expect_written(&file).await;
}

#[tokio::test]
async fn aionrs_team_member_restores_explicit_permissions_and_revokes_yolo_live() {
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("legacy-approval.txt");
    let provider = mock_provider(&file).await;
    let db = aionui_db::init_database_memory().await.unwrap();
    let (mut app, services) = start_host(db.clone(), data.path()).await;
    let member = create_member(&mut app, &services, data.path(), &provider).await;

    // Both legacy startup fields could contain implicit Yolo.
    sqlx::query("UPDATE conversations SET extra = json_set(extra, '$.session_mode', 'yolo') WHERE id = ?")
        .bind(&member.conversation_id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE teams SET session_mode = 'yolo' WHERE id = ?")
        .bind(&member.team_id)
        .execute(db.pool())
        .await
        .unwrap();
    member.attach(&app).await;
    assert_eq!(member.mode(&app).await, "default");

    let direct_change = member
        .request(
            &app,
            json_with_token(
                "PUT",
                &format!("/api/conversations/{}/config-options/mode", member.conversation_id),
                json!({"value": "yolo"}),
                &member.token,
                &member.csrf,
            ),
            StatusCode::BAD_REQUEST,
        )
        .await;
    assert_eq!(
        direct_change["error"],
        "Team permission mode must be selected through the Team API"
    );

    member.select_mode(&app, "yolo").await;
    member.restart(&app).await;
    assert_eq!(member.mode(&app).await, "yolo");
    member.stop_team(&app).await;
    drop((app, services));

    let (app, services) = start_host(db.clone(), data.path()).await;
    member.attach(&app).await;
    assert_eq!(member.mode(&app).await, "yolo");
    member.select_mode(&app, "default").await;
    member.turn(&app, "Write the test file after switching to Ask").await;
    member.require_write_approval(&app, &file).await;
    expect_written(&file).await;
    member.stop_team(&app).await;
    drop((app, services));

    let (app, _services) = start_host(db, data.path()).await;
    member.attach(&app).await;
    assert_eq!(member.mode(&app).await, "default");
    member.stop_team(&app).await;
}

#[tokio::test]
async fn aionrs_team_first_write_after_disk_upgrade_requires_approval() {
    use sqlx::migrate::Migrator;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    let data = tempfile::tempdir().unwrap();
    let db_path = data.path().join("host.db");
    let file = data.path().join("first-upgraded-write.txt");
    let provider = mock_provider(&file).await;
    let team_id = "legacy-team";
    let slot_id = "legacy-writer";
    let conversation_id = "legacy-conversation";
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&db_path).create_if_missing(true))
        .await
        .unwrap();
    // Same PRAGMA setup as the production migrator's historical table rebuilds.
    sqlx::raw_sql("PRAGMA foreign_keys = OFF; PRAGMA legacy_alter_table = ON;")
        .execute(&pool)
        .await
        .unwrap();
    let mut migrator: Migrator = sqlx::migrate!("../aionui-db/migrations");
    migrator.migrations.to_mut().retain(|migration| migration.version <= 44);
    migrator.run(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at)
         VALUES ('system_default_user', 'admin', '', 1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, model, created_at, updated_at)
         VALUES (?, 'system_default_user', 'Legacy Writer', 'aionrs', ?, ?, 1, 1)",
    )
    .bind(conversation_id)
    .bind(
        json!({
            "teamId": team_id, "slot_id": slot_id, "role": "lead", "backend": "aionrs",
            "session_mode": "yolo", "workspace": data.path()
        })
        .to_string(),
    )
    .bind(json!({"provider_id": PROVIDER_ID, "model": "gpt-4o"}).to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, agents, lead_agent_id, session_mode,
                           agents_version, created_at, updated_at)
         VALUES (?, 'system_default_user', 'Legacy Team', ?, ?, ?, 'yolo', '1.0.1', 1, 1)",
    )
    .bind(team_id)
    .bind(data.path().to_str().unwrap())
    .bind(
        json!([{
            "slot_id": slot_id, "conversation_id": conversation_id, "name": "Legacy Writer",
            "role": "lead", "backend": "aionrs", "model": "gpt-4o"
        }])
        .to_string(),
    )
    .bind(slot_id)
    .execute(&pool)
    .await
    .unwrap();
    let latest: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(latest, 44);
    pool.close().await;

    let upgraded = aionui_db::init_database(&db_path).await.unwrap();
    let applied: bool = sqlx::query_scalar("SELECT success FROM _sqlx_migrations WHERE version = 45")
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
    assert!(applied);
    let (mut app, services) = start_host(upgraded, data.path()).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    register_provider(&app, &token, &csrf, &provider).await;
    let member = TeamMember {
        team_id: team_id.to_owned(),
        slot_id: slot_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
        token,
        csrf,
    };
    member.attach(&app).await;
    member.turn(&app, "First Write after upgrading data").await;
    member.require_write_approval(&app, &file).await;
    expect_written(&file).await;
}

#[tokio::test]
async fn aionrs_team_ask_ignores_workspace_tool_allow_list() {
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join(".aionrs.toml"), "[tools]\nallow_list = [\"Write\"]\n").unwrap();
    let file = data.path().join("configured-write.txt");
    let provider = mock_provider(&file).await;
    let db = aionui_db::init_database_memory().await.unwrap();
    let (mut app, services) = start_host(db, data.path()).await;
    let member = create_member(&mut app, &services, data.path(), &provider).await;

    member.attach(&app).await;
    member.turn(&app, "Write with a workspace allow-list").await;
    member.require_write_approval(&app, &file).await;
    expect_written(&file).await;
}

#[tokio::test]
async fn aionrs_team_legacy_cron_full_auto_cannot_override_ask() {
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("scheduled-write.txt");
    let provider = mock_provider(&file).await;
    let db = aionui_db::init_database_memory().await.unwrap();
    let (mut app, services) = start_host(db.clone(), data.path()).await;
    let member = create_member(&mut app, &services, data.path(), &provider).await;
    member.attach(&app).await;
    let now = aionui_common::now_ms();
    let user_id = services
        .conversation_repo
        .owner_user_id(&member.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let cron_repo = aionui_db::SqliteCronRepository::new(db.pool().clone());
    aionui_db::ICronRepository::insert(
        &cron_repo,
        &aionui_db::models::CronJobRow {
            id: "cron_legacy_team_write".to_owned(),
            user_id,
            name: "Legacy Team Write".to_owned(),
            enabled: true,
            schedule_kind: "every".to_owned(),
            schedule_value: "86400000".to_owned(),
            schedule_tz: None,
            schedule_description: None,
            payload_message: "Write from a stored schedule".to_owned(),
            execution_mode: "existing".to_owned(),
            agent_config: Some(
                json!({
                    "name": "Legacy Team Write", "assistant_id": "team-aionrs", "mode": "yolo"
                })
                .to_string(),
            ),
            conversation_id: member.conversation_id.clone(),
            conversation_title: None,
            created_by: "user".to_owned(),
            skill_content: None,
            description: None,
            created_at: now,
            updated_at: now,
            next_run_at: Some(now + 86_400_000),
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        },
    )
    .await
    .unwrap();

    member
        .change(
            &app,
            "POST",
            "/api/cron/jobs/cron_legacy_team_write/run",
            json!({}),
            StatusCode::OK,
        )
        .await;
    member.require_write_approval(&app, &file).await;
    expect_written(&file).await;
    assert_eq!(member.mode(&app).await, "default");
}

#[cfg(unix)]
#[tokio::test]
async fn aionrs_team_ask_wins_over_paused_turn_rebuild() {
    use std::os::unix::fs::PermissionsExt;

    let data = tempfile::tempdir().unwrap();
    let gate = tempfile::tempdir().unwrap();
    let release = gate.path().join("release");
    // Unblock and reap the wrapper on assertion failure as well.
    struct ReleaseGate(std::path::PathBuf);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, []);
        }
    }
    let _release_gate = ReleaseGate(release.clone());
    let wrapper = gate.path().join("host-wrapper");
    std::fs::write(gate.path().join("host-binary"), env!("CARGO_BIN_EXE_aelix-studio-host")).unwrap();
    std::fs::write(
        &wrapper,
        r#"#!/bin/sh
set -eu
gate=$(dirname "$0")
if [ "${1:-}" = mcp-team-stdio ] && [ -f "$gate/armed" ] &&
    [ "${TEAM_AGENT_SLOT_ID:-}" = "$(cat "$gate/armed")" ] && [ ! -f "$gate/release" ]; then
    touch "$gate/entered"
    attempts=0
    while [ ! -f "$gate/release" ]; do
        attempts=$((attempts + 1))
        [ "$attempts" -le 500 ] || exit 1
        sleep 0.04
    done
fi
exec "$(cat "$gate/host-binary")" "$@"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();

    let file = data.path().join("racing-write.txt");
    let provider = mock_provider(&file).await;
    let db = aionui_db::init_database_memory().await.unwrap();
    let (mut app, services) = start_host_with_binary(db.clone(), data.path(), &wrapper).await;
    let member = create_member(&mut app, &services, data.path(), &provider).await;
    member.attach(&app).await;
    member.select_mode(&app, "yolo").await;

    // Evict the task while the Team slot stays Ready, as a model update does.
    services
        .worker_task_manager
        .kill_and_wait(&member.conversation_id, None)
        .await;
    assert!(services.worker_task_manager.get_task(&member.conversation_id).is_none());
    std::fs::write(gate.path().join("armed"), &member.slot_id).unwrap();
    // The queued Team turn has passed HTTP readiness reconciliation. Dispatch
    // it via the same conversation entry point the Team turn adapter uses.
    let user_id = services
        .conversation_repo
        .owner_user_id(&member.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let service = services.conversation_service.clone();
    let conversation_id = member.conversation_id.clone();
    let turn = tokio::spawn(async move {
        service
            .run_agent_turn(aionui_conversation::ConversationAgentTurnRequest {
                user_id,
                conversation_id,
                content: "Write from the rebuilding runtime".to_owned(),
                files: Vec::new(),
                inject_skills: Vec::new(),
                required_runtime_mode: None,
                persist_user_message: false,
                user_message_hidden: false,
                on_started: None,
            })
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !gate.path().join("entered").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new runtime reached the MCP initialization gate");
    assert!(services.worker_task_manager.get_task(&member.conversation_id).is_none());

    member
        .change(
            &app,
            "POST",
            &format!("/api/teams/{}/session-mode", member.team_id),
            json!({"mode": "default"}),
            StatusCode::OK,
        )
        .await;
    let selection: String = sqlx::query_scalar("SELECT aionrs_team_permission_mode FROM conversations WHERE id = ?")
        .bind(&member.conversation_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(selection, "default");
    assert!(!file.exists());
    std::fs::write(&release, []).unwrap();

    member.require_write_approval(&app, &file).await;
    expect_written(&file).await;
    let outcome = tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.status,
        aionui_conversation::ConversationAgentTurnStatus::Completed
    );
    assert_eq!(member.mode(&app).await, "default");
}
