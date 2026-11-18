use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_app::{AppConfig, AppServices, StudioAuth, create_studio_router};
use aionui_db::{DataDirInstanceGuard, init_database_memory};
use reqwest::{Client, StatusCode, header};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::{Error as WebSocketError, client::IntoClientRequest};

const FIXTURE_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PRIVATE_HEADER: &str = "x-aionui-studio-auth";

struct StudioHost {
    client: Client,
    url: String,
    task: tokio::task::JoinHandle<()>,
    helper_token: String,
}

impl StudioHost {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = AppConfig {
            local: true,
            port,
            data_dir: dir.path().into(),
            work_dir: dir.path().into(),
            ..Default::default()
        };
        let db = init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &config).await.unwrap();
        services
            .user_repo
            .set_system_user_credentials(
                "admin",
                &aionui_auth::hash_password("Synthetic-operator-password-1").unwrap(),
            )
            .await
            .unwrap();
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
            let _dir = dir;
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            client: Client::builder().no_proxy().build().unwrap(),
            url: format!("http://127.0.0.1:{port}"),
            task,
            helper_token,
        }
    }

    async fn login(&self) -> String {
        let response = self
            .client
            .post(format!("{}/login", self.url))
            .header(header::ORIGIN, &self.url)
            .json(&serde_json::json!({"username":"admin","password":"Synthetic-operator-password-1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|cookie| cookie.to_str().ok()?.split(';').next())
            .find(|cookie| cookie.starts_with("aionui-session="))
            .unwrap()
            .to_owned()
    }

    fn upgrade(&self) -> axum::http::Request<()> {
        let url = format!("{}/ws", self.url.replacen("http:", "ws:", 1));
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
    let desktop_only = helper_get("/api/auth/internal/users/system").send().await.unwrap();
    assert_eq!(desktop_only.status(), StatusCode::FORBIDDEN);
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
