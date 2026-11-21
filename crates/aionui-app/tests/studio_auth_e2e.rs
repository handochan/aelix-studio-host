use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_app::{AppConfig, AppServices, StudioAuth, create_studio_router};
use aionui_db::{DataDirInstanceGuard, init_database, init_database_memory};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode, header};
use std::{io::Write, path::Path, process::Stdio, time::Duration};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, client::IntoClientRequest};

const FIXTURE_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PRIVATE_HEADER: &str = "x-aionui-studio-auth";

struct StudioHost {
    client: Client,
    url: String,
    task: tokio::task::JoinHandle<()>,
    helper_token: String,
    runtime_tokens: std::sync::Arc<aionui_ai_agent::RuntimeTokenService>,
    _dir: Option<tempfile::TempDir>,
    users: std::sync::Arc<dyn aionui_db::IUserRepository>,
    #[cfg(unix)]
    conversations: aionui_conversation::ConversationService,
    #[cfg(unix)]
    projects: aionui_project::ProjectService,
    #[cfg(unix)]
    tasks: std::sync::Arc<dyn aionui_ai_agent::IWorkerTaskManager>,
}

impl StudioHost {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut host = Self::start_with_db(dir.path(), init_database_memory().await.unwrap(), true).await;
        host._dir = Some(dir);
        host
    }

    async fn start_with_db(dir: &Path, db: aionui_db::Database, seed: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = AppConfig {
            local: true,
            port,
            data_dir: dir.into(),
            work_dir: dir.into(),
            ..Default::default()
        };
        let services = AppServices::from_config(db, &config).await.unwrap();
        if seed {
            services
                .user_repo
                .set_system_user_credentials(
                    "admin",
                    &aionui_auth::hash_password("Synthetic-operator-password-1").unwrap(),
                )
                .await
                .unwrap();
        }
        let helper_token = services
            .runtime_token_service
            .issue(
                "system_default_user",
                "fixture-conversation",
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
                [RuntimeTokenScope::ConversationHelper],
            )
            .token;
        let secret = FIXTURE_TOKEN.as_bytes().try_into().unwrap();
        let (router, _runtime) = create_studio_router(&services, StudioAuth::new(port, secret))
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            client: Client::builder().no_proxy().build().unwrap(),
            url: format!("http://127.0.0.1:{port}"),
            task,
            helper_token,
            runtime_tokens: services.runtime_token_service,
            _dir: None,
            users: services.user_repo,
            #[cfg(unix)]
            conversations: services.conversation_service,
            #[cfg(unix)]
            projects: services.project_service,
            #[cfg(unix)]
            tasks: services.worker_task_manager,
        }
    }

    async fn login(&self) -> String {
        self.login_cookies("Synthetic-operator-password-1").await.0
    }

    async fn login_cookies(&self, password: &str) -> (String, String) {
        let response = self
            .client
            .post(format!("{}/login", self.url))
            .header(header::ORIGIN, &self.url)
            .json(&serde_json::json!({"username":"admin","password":password}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = |name: &str| {
            response
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|cookie| cookie.to_str().ok()?.split(';').next())
                .find(|cookie| cookie.starts_with(name))
                .unwrap()
                .to_owned()
        };
        (cookie("aionui-session="), cookie("aionui-refresh="))
    }

    fn upgrade(&self) -> axum::http::Request<()> {
        self.upgrade_path("/ws")
    }

    fn upgrade_path(&self, path: &str) -> axum::http::Request<()> {
        let url = format!("{}{path}", self.url.replacen("http:", "ws:", 1));
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert(header::ORIGIN, self.url.parse().unwrap());
        req
    }
}

impl Drop for StudioHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn api_and_static_asset_require_login_and_desktop_has_separate_access() {
    let host = StudioHost::start().await;
    for path in ["/api/system/info", "/api/assets/logos/missing.svg"] {
        let response = host.client.get(format!("{}{path}", host.url)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let cookie = host.login().await;
    let browser = host
        .client
        .get(format!("{}/api/system/info", host.url))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(browser.status(), StatusCode::OK);

    let auth_admin = host.client.get(format!("{}/api/auth/internal/users/system", host.url));
    let browser = auth_admin
        .try_clone()
        .unwrap()
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(browser.status(), StatusCode::FORBIDDEN);
    let desktop = auth_admin.header(PRIVATE_HEADER, FIXTURE_TOKEN).send().await.unwrap();
    assert_eq!(desktop.status(), StatusCode::OK);
    assert!(!desktop.headers().contains_key(PRIVATE_HEADER));
    assert!(!desktop.text().await.unwrap().contains(FIXTURE_TOKEN));
}

#[tokio::test]
async fn source_checks_cover_preflight_and_websocket_before_upgrade() {
    let host = StudioHost::start().await;
    let preflight = host
        .client
        .request(reqwest::Method::OPTIONS, format!("{}/api/system/info", host.url))
        .header(header::ORIGIN, "https://unlisted.invalid")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::FORBIDDEN);
    assert!(!preflight.headers().contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));

    let result = tokio_tungstenite::connect_async(host.upgrade_path("/api/stt/stream")).await;
    assert!(matches!(result, Err(WebSocketError::Http(response)) if response.status() == StatusCode::UNAUTHORIZED));

    let result = tokio_tungstenite::connect_async(host.upgrade()).await;
    assert!(matches!(result, Err(WebSocketError::Http(response)) if response.status() == StatusCode::UNAUTHORIZED));

    let mut browser = host.upgrade();
    browser
        .headers_mut()
        .insert(header::COOKIE, host.login().await.parse().unwrap());
    let (_, response) = tokio_tungstenite::connect_async(browser).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

    let mut desktop = host.upgrade();
    desktop
        .headers_mut()
        .insert(PRIVATE_HEADER, FIXTURE_TOKEN.parse().unwrap());
    let (_, response) = tokio_tungstenite::connect_async(desktop).await.unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert!(!response.headers().contains_key(PRIVATE_HEADER));
}

#[tokio::test]
async fn conversation_helper_retains_api_access_without_desktop_authority() {
    let host = StudioHost::start().await;
    let helper_get = |path: &str| {
        host.client
            .get(format!("{}{path}", host.url))
            .header("x-aionui-runtime-token", &host.helper_token)
            .header("x-aionui-user-id", "system_default_user")
            .header("x-aionui-conversation-id", "fixture-conversation")
    };
    let normal = helper_get("/api/system/info").send().await.unwrap();
    assert_eq!(normal.status(), StatusCode::OK);
    let config = helper_get("/api/settings/client").send().await.unwrap();
    assert_eq!(config.status(), StatusCode::OK);

    let desktop_only = helper_get("/api/auth/internal/users/system").send().await.unwrap();
    assert_eq!(desktop_only.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        helper_get("/api/conversations/other").send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let approval = host
        .client
        .post(format!(
            "{}/api/conversations/fixture-conversation/confirmations/fixture-call/confirm",
            host.url
        ))
        .header("x-aionui-runtime-token", &host.helper_token)
        .header("x-aionui-user-id", "system_default_user")
        .header("x-aionui-conversation-id", "fixture-conversation")
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(approval.status(), StatusCode::FORBIDDEN);
}

#[test]
fn offline_password_command_refuses_running_data_dir_before_prompting() {
    let dir = tempfile::tempdir().unwrap();
    let _running = DataDirInstanceGuard::try_acquire(&dir.path().join("aionui-backend.db"))
        .unwrap()
        .unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_aelix-studio-host"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["user", "set-password", "--require-stopped", "--password-stdin"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("Stop WebUI before resetting its password."), "{stderr}");
}

#[tokio::test]
async fn stopped_reset_revokes_access_and_refresh_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("aionui-backend.db");
    let first = StudioHost::start_with_db(dir.path(), init_database(&db_path).await.unwrap(), true).await;
    let (access, refresh) = first.login_cookies("Synthetic-operator-password-1").await;
    first.task.abort();
    let mut first = first;
    let _ = (&mut first.task).await;
    drop(first);

    let mut reset = std::process::Command::new(env!("CARGO_BIN_EXE_aelix-studio-host"))
        .arg("--data-dir")
        .arg(dir.path())
        .args(["user", "set-password", "--require-stopped", "--password-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    reset
        .stdin
        .take()
        .unwrap()
        .write_all(b"Synthetic-new-password-2\n")
        .unwrap();
    assert!(reset.wait().unwrap().success());

    let second = StudioHost::start_with_db(dir.path(), init_database(&db_path).await.unwrap(), false).await;
    let denied = second
        .client
        .get(format!("{}/api/system/info", second.url))
        .header(header::COOKIE, access)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let denied = second
        .client
        .post(format!("{}/api/auth/refresh", second.url))
        .header(header::ORIGIN, &second.url)
        .header(header::COOKIE, refresh)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    second.login_cookies("Synthetic-new-password-2").await;
}

#[tokio::test]
async fn logging_out_closes_only_the_browser_socket() {
    use tokio_tungstenite::tungstenite::Message;
    let host = StudioHost::start().await;
    let cookie = host.login().await;
    let mut browser_upgrade = host.upgrade();
    browser_upgrade
        .headers_mut()
        .insert(header::COOKIE, cookie.parse().unwrap());
    let (mut browser, _) = tokio_tungstenite::connect_async(browser_upgrade).await.unwrap();
    let mut desktop_upgrade = host.upgrade();
    desktop_upgrade
        .headers_mut()
        .insert(PRIVATE_HEADER, FIXTURE_TOKEN.parse().unwrap());
    let (mut desktop, _) = tokio_tungstenite::connect_async(desktop_upgrade).await.unwrap();

    let logout = host
        .client
        .post(format!("{}/logout", host.url))
        .header(header::ORIGIN, &host.url)
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), StatusCode::OK);
    let closed = tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(frame)) = browser.next().await {
            if let Message::Close(Some(frame)) = frame {
                return Some(frame.code);
            }
        }
        None
    })
    .await
    .unwrap();
    assert_eq!(
        closed,
        Some(tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy)
    );
    desktop.send(Message::Ping(vec![1].into())).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(2), desktop.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(reply, Message::Pong(_)));
}

#[tokio::test]
async fn desktop_credential_does_not_override_a_forged_origin() {
    let host = StudioHost::start().await;
    for method in [reqwest::Method::GET, reqwest::Method::OPTIONS] {
        let denied = host
            .client
            .request(method, format!("{}/api/system/info", host.url))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .header(header::ORIGIN, "https://unlisted.invalid")
            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }

    let mut upgrade = host.upgrade();
    upgrade
        .headers_mut()
        .insert(PRIVATE_HEADER, FIXTURE_TOKEN.parse().unwrap());
    upgrade
        .headers_mut()
        .insert(header::ORIGIN, "https://unlisted.invalid".parse().unwrap());
    let denied = tokio_tungstenite::connect_async(upgrade).await;
    assert!(matches!(denied, Err(WebSocketError::Http(response)) if response.status() == StatusCode::FORBIDDEN));
}

#[tokio::test]
async fn changed_session_generation_closes_an_idle_browser_socket() {
    use tokio_tungstenite::tungstenite::Message;
    let host = StudioHost::start().await;
    let mut request = host.upgrade();
    request
        .headers_mut()
        .insert(header::COOKIE, host.login().await.parse().unwrap());
    let (mut browser, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    host.users
        .increment_session_generation("system_default_user")
        .await
        .unwrap();

    let closed = tokio::time::timeout(Duration::from_secs(4), async {
        while let Some(Ok(frame)) = browser.next().await {
            if let Message::Close(Some(frame)) = frame {
                return Some(frame.code);
            }
        }
        None
    })
    .await
    .unwrap();
    assert_eq!(
        closed,
        Some(tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy)
    );
}

fn helper_cli(host: &StudioHost, conversation_id: &str, token: &str) -> tokio::process::Command {
    let mut cli = tokio::process::Command::new(env!("CARGO_BIN_EXE_aelix-studio-host"));
    cli.kill_on_drop(true)
        .env("AIONUI_BASE_URL", &host.url)
        .env("AIONUI_CONVERSATION_ID", conversation_id)
        .env("AIONUI_USER_ID", "system_default_user")
        .env("AIONUI_RUNTIME_TOKEN", token)
        .env("NO_PROXY", "127.0.0.1");
    cli
}

#[tokio::test]
async fn diagnose_health_and_overview_work_with_a_conversation_helper_token() {
    let host = StudioHost::start().await;
    for (action, health_selector) in [("health", "/data/status"), ("overview", "/data/health/status")] {
        let result = helper_cli(&host, "fixture-conversation", &host.helper_token)
            .args(["diagnose", action])
            .output()
            .await
            .unwrap();
        assert!(result.status.success(), "diagnose {action} returned {}", result.status);
        let response: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(response.pointer(health_selector), Some(&serde_json::json!("ok")));
    }

    let denied = host
        .client
        .get(format!("{}/health", host.url))
        .header("x-aionui-runtime-token", "invalid-fixture-token")
        .header("x-aionui-user-id", "system_default_user")
        .header("x-aionui-conversation-id", "fixture-conversation")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
}

#[cfg(unix)]
#[tokio::test]
async fn studio_release_rejects_foreign_origin_and_valid_conversation_helper() {
    use serde_json::{Value, json};

    let host = StudioHost::start().await;
    let dir = host._dir.as_ref().unwrap().path();
    let adapter = dir.join("release_agent.py");
    std::fs::write(
        &adapter,
        r#"import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    result = {
        'initialize': {'protocolVersion':1, 'agentInfo':{'name':'aelix-acp','version':'test'},
                       'agentCapabilities':{'sessionCapabilities':{'close':{}}}},
        'session/new': {'sessionId':'fixture-session'},
    }.get(req.get('method'), {})
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#,
    )
    .unwrap();
    let registered = host
        .client
        .post(format!("{}/api/agents/custom", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .json(&json!({"name":"Release agent","command":"python3","args":[adapter],"env":[],"icon":"A"}))
        .send()
        .await
        .unwrap();
    assert_eq!(registered.status(), StatusCode::OK);
    let catalog: Value = host
        .client
        .get(format!("{}/api/assistants", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let assistant = catalog["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "Release agent")
        .unwrap();
    let created = host
        .client
        .post(format!("{}/api/conversations", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .json(&json!({"name":"Release admission","assistant":{"id":assistant["id"]},
                      "extra":{"workspace":dir,"custom_workspace":true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let body: Value = created.json().await.unwrap();
    let id = body["data"]["id"].as_str().unwrap();
    let route = format!("{}/api/conversations/{id}", host.url);
    let cookie = host.login().await;
    let token = host
        .runtime_tokens
        .issue(
            "system_default_user",
            id,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;
    let browser_post = |path: &str, origin: &str| {
        host.client
            .post(format!("{route}/{path}"))
            .header(header::ORIGIN, origin)
            .header(header::COOKIE, &cookie)
    };
    let view = json!({"view_id":"fixture-view"});
    let ready = browser_post("runtime/ensure?view_id=fixture-view", &host.url)
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);

    let helper = |request: reqwest::RequestBuilder| {
        request
            .header("x-aionui-runtime-token", &token)
            .header("x-aionui-user-id", "system_default_user")
            .header("x-aionui-conversation-id", id)
    };
    let permitted_read = helper(host.client.get(&route)).send().await.unwrap();
    assert_eq!(permitted_read.status(), StatusCode::OK);

    for attempted in [
        browser_post("runtime/release", "https://foreign.invalid"),
        helper(host.client.post(format!("{route}/runtime/release"))),
    ] {
        let denied = attempted.json(&view).send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        let err: Value = denied.json().await.unwrap();
        assert_eq!(err["code"], "SOURCE_FORBIDDEN");
    }

    let renewal = browser_post("active-lease", &host.url)
        .json(&view)
        .send()
        .await
        .unwrap();
    assert_eq!(renewal.status(), StatusCode::OK);
    let preserved: Value = host
        .client
        .get(&route)
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preserved["data"]["runtime"]["has_task"], true);
    let allowed = browser_post("runtime/release", &host.url)
        .json(&view)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    let reply: Value = allowed.json().await.unwrap();
    assert_eq!(reply["data"]["outcome"], "released");
}

#[tokio::test]
async fn helper_rename_changes_only_the_name_of_its_own_conversation() {
    use tokio::io::AsyncWriteExt;

    let host = StudioHost::start().await;
    let response = host
        .client
        .post(format!("{}/api/conversations", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .json(&serde_json::json!({
            "type": "aionrs",
            "name": "Original",
            "extra": { "fixture_key": "original" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: serde_json::Value = response.json().await.unwrap();
    let id = created["data"]["id"].as_str().unwrap();
    let token = host
        .runtime_tokens
        .issue(
            "system_default_user",
            id,
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            [RuntimeTokenScope::ConversationHelper],
        )
        .token;

    let mut rename = helper_cli(&host, id, &token)
        .args(["config", "conversation", "rename"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let payload = serde_json::to_vec(&serde_json::json!({"conversation_id": id, "name": "Renamed"})).unwrap();
    rename.stdin.take().unwrap().write_all(&payload).await.unwrap();
    assert!(rename.wait_with_output().await.unwrap().status.success());

    let patch = |target: &str, body: String| {
        host.client
            .patch(format!("{}/api/conversations/{target}", host.url))
            .header("x-aionui-runtime-token", &token)
            .header("x-aionui-user-id", "system_default_user")
            .header("x-aionui-conversation-id", id)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
    };
    for body in [
        r#"{"name":"Unexpected","extra":{"fixture_key":"changed"}}"#.to_owned(),
        r#"{"name":"Unexpected","name_source":"auto"}"#.to_owned(),
        r#"{"name":"First","name":"Second"}"#.to_owned(),
        r#"{"name":null}"#.to_owned(),
        format!(r#"{{"name":"{}"}}"#, "x".repeat(16 * 1024)),
    ] {
        let denied = patch(id, body).send().await.unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    }
    let denied = patch("other-conversation", r#"{"name":"Unexpected"}"#.to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let current: serde_json::Value = host
        .client
        .get(format!("{}/api/conversations/{id}", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(current["data"]["name"], "Renamed");
    assert_eq!(current["data"]["extra"]["fixture_key"], "original");
}

#[cfg(unix)]
#[path = "studio_auth_e2e/acp_relay.rs"]
mod acp_relay;
