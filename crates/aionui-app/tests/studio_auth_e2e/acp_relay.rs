use super::*;
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct SecretInLogs(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Write for SecretInLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.windows("relay-secret".len()).any(|bytes| bytes == b"relay-secret") {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn read(response: reqwest::Response, status: StatusCode) -> Value {
    let actual = response.status();
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(actual, status, "{body}");
    body
}

async fn code(response: reqwest::Response, status: StatusCode, code: &str) {
    assert_eq!(read(response, status).await["code"], code);
}

async fn fixture(dir: &Path) -> aionui_db::Database {
    let db = init_database_memory().await.unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp_relay.py");
    for (id, backend, source, peer) in [
        ("fixture-aelix", "aelix", "builtin", "aelix-acp"),
        ("custom-aelix", "aelix", "custom", "aelix-acp"),
        ("wrong-name-aelix", "aelix", "builtin", "another-agent"),
        ("foreign-aelix", "aelix", "builtin", "aelix-acp"),
    ] {
        sqlx::query(
            "INSERT INTO agent_metadata
             (id, agent_id, user_id, name, backend, agent_type, agent_source, command, args, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 'acp', ?, 'python3', ?, 1, 1)",
        )
        .bind(id)
        .bind(id)
        .bind((source == "custom").then_some("system_default_user"))
        .bind(id)
        .bind(backend)
        .bind(source)
        .bind(
            json!([
                script,
                dir.join(if id == "foreign-aelix" {
                    "foreign-calls"
                } else {
                    "relay-calls"
                }),
                peer
            ])
            .to_string(),
        )
        .execute(db.pool())
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO users (id, user_type, created_at, updated_at) VALUES ('other-owner', 'aionpro', 1, 1)")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, created_at, updated_at)
                 VALUES ('other-conversation', 'other-owner', 'private', 'acp', ?, 1, 1)",
    )
    .bind(json!({"agent_id":"foreign-aelix","backend":"aelix","workspace": dir,"is_custom_workspace":true}).to_string())
    .execute(db.pool())
    .await
    .unwrap();
    db
}

async fn create(host: &StudioHost, dir: &Path, agent_id: &str) -> String {
    let response = host
        .client
        .post(format!("{}/api/conversations", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .json(&json!({"type":"acp", "name":"H1 contract", "extra":{
            "agent_id": agent_id, "backend": "aelix", "workspace": dir, "custom_workspace": true
        }}))
        .send()
        .await
        .unwrap();
    read(response, StatusCode::CREATED).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn acp_relay_unprotected_listener_has_no_route() {
    use tower::ServiceExt as _;
    let dir = tempfile::tempdir().unwrap();
    let config = AppConfig {
        local: true,
        data_dir: dir.path().into(),
        work_dir: dir.path().into(),
        ..Default::default()
    };
    let services = AppServices::from_config(init_database_memory().await.unwrap(), &config)
        .await
        .unwrap();
    let router = aionui_app::create_router(&services).await.unwrap();
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/conversations/any/acp-extensions")
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn acp_relay_requires_both_builtin_backend_and_initialize_identity() {
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path()).await;
    let host = StudioHost::start_with_db(dir.path(), db, true).await;
    for agent in ["custom-aelix", "wrong-name-aelix"] {
        let id = create(&host, dir.path(), agent).await;
        let root = format!("{}/api/conversations/{id}", host.url);
        read(
            host.client
                .post(format!("{root}/runtime/ensure"))
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::OK,
        )
        .await;
        code(
            host.client
                .get(format!("{root}/acp-extensions"))
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::CONFLICT,
            "EXT_UNAVAILABLE",
        )
        .await;
    }
    assert!(!dir.path().join("relay-calls").exists());
}

#[tokio::test]
async fn acp_relay_protected_router_and_stdio_contract() {
    let secret_in_logs = SecretInLogs::default();
    let capture = secret_in_logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || capture.clone())
        .finish();
    let _capture = tracing::subscriber::set_default(subscriber);
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path()).await;
    let host = StudioHost::start_with_db(dir.path(), db, true).await;
    let id = create(&host, dir.path(), "fixture-aelix").await;
    let root = format!("{}/api/conversations/{id}", host.url);
    let endpoint = format!("{root}/acp-extensions");
    let get = || host.client.get(&endpoint);
    let desk_get = || get().header(PRIVATE_HEADER, FIXTURE_TOKEN);

    code(get().send().await.unwrap(), StatusCode::UNAUTHORIZED, "AUTH_REQUIRED").await;
    code(
        get().header(PRIVATE_HEADER, "wrong").send().await.unwrap(),
        StatusCode::UNAUTHORIZED,
        "AUTH_REQUIRED",
    )
    .await;
    code(
        desk_get().header(header::HOST, "wrong").send().await.unwrap(),
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
    )
    .await;
    code(
        desk_get()
            .header(header::ORIGIN, &host.url)
            .header(header::ORIGIN, &host.url)
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
    )
    .await;
    code(
        desk_get()
            .header("x-aionui-runtime-token", &host.helper_token)
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
    )
    .await;
    code(
        get()
            .header("x-aionui-runtime-token", &host.helper_token)
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
    )
    .await;
    code(
        desk_get().send().await.unwrap(),
        StatusCode::CONFLICT,
        "EXT_NO_LIVE_TASK",
    )
    .await;
    assert!(!dir.path().join("relay-calls").exists());

    let missing = read(
        host.client
            .get(format!("{}/api/conversations/missing/acp-extensions", host.url))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
    )
    .await;
    host.conversations
        .ensure_runtime("other-owner", "other-conversation", &host.tasks)
        .await
        .unwrap();
    let aionui_ai_agent::AgentInstance::Acp(foreign) = host.tasks.get_task("other-conversation").unwrap() else {
        panic!("foreign conversation must have a live ACP task");
    };
    let foreign_binding = foreign.relay_discovery().unwrap().binding_id;
    let request = aionui_api_types::AcpExtensionRequest {
        binding_id: foreign_binding.clone(),
        method: "_aelix/session/probe/echo".into(),
        payload: json!({"value":"owner positive control"})
            .as_object()
            .unwrap()
            .to_owned(),
    };
    let call = foreign
        .relay_dispatch(
            request,
            aionui_ai_agent::protocol::acp_relay::RelayWriteAdmission::new(
                None,
                tokio::time::Instant::now() + Duration::from_secs(5),
                |write| write(),
            ),
        )
        .unwrap();
    assert_eq!(call.reply.await.unwrap().unwrap(), "owner positive control");
    let foreign_calls = dir.path().join("foreign-calls");
    let baseline = std::fs::read(&foreign_calls).unwrap();
    let other = format!("{}/api/conversations/other-conversation/acp-extensions", host.url);
    assert_eq!(
        read(
            host.client
                .get(&other)
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::NOT_FOUND
        )
        .await,
        missing
    );
    assert_eq!(
        read(
            host.client
                .post(&other)
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .json(&json!({
                    "bindingId":foreign_binding, "method":"_aelix/session/probe/echo", "payload":{}
                }))
                .send()
                .await
                .unwrap(),
            StatusCode::NOT_FOUND
        )
        .await,
        missing
    );

    assert_eq!(std::fs::read(&foreign_calls).unwrap(), baseline);

    host.tasks.kill_and_wait("other-conversation", None).await;

    let browser = host.login().await;
    let mut websocket = host.upgrade();
    websocket.headers_mut().insert(header::COOKIE, browser.parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::connect_async(websocket).await.unwrap();

    let send = host
        .client
        .post(format!("{root}/messages"))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .json(&json!({"content":"original prompt"}))
        .send()
        .await
        .unwrap();
    let actual = send.status();
    let send_body = send.json::<Value>().await.unwrap();
    assert!(actual.is_success(), "{actual}: {send_body}");
    let turn = send_body["data"]["turn_id"].as_str().unwrap();
    let mut initial_text = false;
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(Ok(frame)) = websocket.next().await {
            let Some(msg) = frame
                .to_text()
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            else {
                continue;
            };
            if msg["name"] == "message.stream" && msg["data"]["data"]["content"] == "before input" {
                initial_text = true;
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(initial_text);

    let discovered = get().header(header::COOKIE, &browser).send().await.unwrap();
    assert_eq!(discovered.headers()[header::CACHE_CONTROL], "no-store");
    let csrf_cookie = discovered
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next())
        .find(|value| value.starts_with("aionui-csrf-token="))
        .unwrap()
        .to_owned();
    let csrf = csrf_cookie.split_once('=').unwrap().1;
    let cookies = format!("{browser}; {csrf_cookie}");
    let discovery = read(discovered, StatusCode::OK).await["data"].clone();
    let binding = discovery["bindingId"].as_str().unwrap();
    assert_eq!(discovery["version"], 1);
    assert_eq!(discovery["notifications"], json!([]));
    assert_eq!(discovery["requests"][0], "_aelix/session/probe/echo");

    let echo = "_aelix/session/probe/echo";
    let body = |method: &str, payload: Value| json!({"method":method,"bindingId":binding,"payload":payload});
    let desktop = |method: &str, payload: Value| {
        host.client
            .post(&endpoint)
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&body(method, payload))
    };
    let browser_post = || {
        host.client
            .post(&endpoint)
            .header(header::COOKIE, &cookies)
            .json(&body(echo, json!({"value": {"unknown": ["한\u{1f680}\"\\", null]}})))
    };
    code(
        browser_post().send().await.unwrap(),
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
    )
    .await;
    code(
        browser_post().header(header::ORIGIN, &host.url).send().await.unwrap(),
        StatusCode::FORBIDDEN,
        "CSRF_INVALID",
    )
    .await;
    code(
        browser_post()
            .header(header::ORIGIN, &host.url)
            .header("x-csrf-token", "wrong")
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "CSRF_INVALID",
    )
    .await;
    for method in [
        "_aelix/manage",
        "session/prompt",
        "_aelix/session/not-advertised",
        "__aelix/session/probe/echo",
        "_aelix/session/probe/Echo",
    ] {
        code(
            desktop(method, json!({})).send().await.unwrap(),
            StatusCode::FORBIDDEN,
            "EXT_METHOD_DENIED",
        )
        .await;
    }
    assert!(!dir.path().join("relay-calls").exists());
    let echoed = browser_post()
        .header(header::ORIGIN, &host.url)
        .header("x-csrf-token", csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(
        read(echoed, StatusCode::OK).await["data"]["result"],
        json!({"unknown":["한\u{1f680}\"\\",null]})
    );
    assert_eq!(
        read(
            desktop(echo, json!({"value": null})).send().await.unwrap(),
            StatusCode::OK
        )
        .await["data"]["result"],
        Value::Null
    );

    let raw = |bytes: String| {
        host.client
            .post(&endpoint)
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes)
    };
    let duplicate = format!(r#"{{"method":"{echo}","method":"{echo}","bindingId":"{binding}","payload":{{}}}}"#);
    code(
        raw(duplicate).send().await.unwrap(),
        StatusCode::BAD_REQUEST,
        "EXT_INVALID",
    )
    .await;
    code(
        raw(body(echo, json!({})).to_string())
            .header(header::CONTENT_ENCODING, "gzip")
            .send()
            .await
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "EXT_MEDIA_TYPE",
    )
    .await;
    let bytes = body(echo, json!({})).to_string();
    let at_limit = format!("{bytes}{}", " ".repeat(64 * 1024 - bytes.len()));
    read(raw(at_limit.clone()).send().await.unwrap(), StatusCode::OK).await;
    code(
        raw(at_limit.clone() + " ").send().await.unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "EXT_TOO_LARGE",
    )
    .await;
    let stream = futures_util::stream::iter([Ok::<_, std::io::Error>(at_limit), Ok(" ".into())]);
    code(
        host.client
            .post(&endpoint)
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "EXT_TOO_LARGE",
    )
    .await;

    tokio::time::sleep(Duration::from_secs(1)).await;
    let size = "_aelix/session/probe/size";
    for bytes in [262143, 262144] {
        let result = read(
            desktop(size, json!({"bytes":bytes})).send().await.unwrap(),
            StatusCode::OK,
        )
        .await;
        let result = &result["data"]["result"];
        let initial = "한\"\\";
        let empty_bytes = json!({"jsonrpc":"2.0","id":result["rpcId"],
            "result":{"rpcId":result["rpcId"],"value":initial}})
        .to_string()
        .len();
        assert_eq!(result["value"], format!("{initial}{}", "x".repeat(bytes - empty_bytes)));
    }
    code(
        desktop(size, json!({"bytes":262145})).send().await.unwrap(),
        StatusCode::BAD_GATEWAY,
        "EXT_OUTCOME_UNKNOWN",
    )
    .await;

    let write = "_aelix/session/probe/write";
    let lookup = "_aelix/session/requests/lookup";
    for _ in 0..2 {
        let result = read(
            desktop(write, json!({"requestId":"expired","text":"relay-secret"}))
                .send()
                .await
                .unwrap(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(result["data"]["result"]["effects"], 1);
    }
    let rejected = read(
        desktop(write, json!({"requestId":"expired","text":"other"}))
            .send()
            .await
            .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await;
    assert_eq!(rejected["code"], "EXT_AGENT_ERROR");
    assert_eq!(rejected["details"], json!({"rpcCode": -32602}));
    assert!(!rejected.to_string().contains("relay-secret"));
    let clock = "_aelix/session/probe/clock";
    for (elapsed, status) in [
        (599_999, "completed"),
        (600_000, "unavailable"),
        (600_001, "unavailable"),
    ] {
        read(
            desktop(clock, json!({"elapsedMs":elapsed})).send().await.unwrap(),
            StatusCode::OK,
        )
        .await;
        let lookup = read(
            desktop(lookup, json!({"requestId":"expired"})).send().await.unwrap(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(lookup["data"]["result"]["status"], status);
    }
    let expired = read(
        desktop(lookup, json!({"requestId":"expired"})).send().await.unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(expired["data"]["result"], json!({"status":"unavailable"}));
    let result = read(
        desktop(write, json!({"requestId":"new"})).send().await.unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(result["data"]["result"]["effects"], 2);

    tokio::time::sleep(Duration::from_millis(800)).await;
    let hold = "_aelix/session/probe/hold";
    let calls_file = dir.path().join("relay-calls");
    let mut slots = Vec::new();
    for _ in 0..4 {
        let request = desktop(hold, json!({}));
        slots.push(tokio::spawn(async move { request.send().await.unwrap() }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read_to_string(&calls_file)
            .unwrap()
            .lines()
            .filter(|method| *method == hold)
            .count()
            < 4
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Abandoning the HTTP request keeps its SDK execution slot occupied.
    slots.remove(0).abort();
    let limited = desktop(hold, json!({})).send().await.unwrap();
    assert_eq!(limited.headers()[header::RETRY_AFTER], "1");
    code(limited, StatusCode::TOO_MANY_REQUESTS, "EXT_RATE_LIMITED").await;
    read(
        desktop(echo, json!({"value":"still reachable"})).send().await.unwrap(),
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    for slot in slots {
        read(slot.await.unwrap(), StatusCode::OK).await;
    }
    let done = read(
        desktop("_aelix/session/probe/finish", json!({})).send().await.unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(done["data"]["result"], Value::Null);
    let observations = tokio::time::timeout(Duration::from_secs(10), async {
        let mut observed = Vec::new();
        while let Some(Ok(frame)) = websocket.next().await {
            let Some(msg) = frame
                .to_text()
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
            else {
                continue;
            };
            if msg["name"] == "message.userCreated" {
                observed.push(msg["data"]["content"].as_str().unwrap().to_owned());
            }
            if msg["name"] == "message.stream" && msg["data"]["data"]["content"] == "after input" {
                assert_eq!(msg["data"]["turn_id"], turn);
                observed.push("after input".to_owned());
            }
            if msg["name"] == "turn.completed" && msg["data"]["turn_id"] == turn {
                break;
            }
        }
        observed
    })
    .await
    .unwrap();
    assert_eq!(observations, ["same 입력", "same 입력", "after input"]);
    let history = read(
        host.client
            .get(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let rows = history["data"]["items"].as_array().unwrap();
    let index = |predicate: &dyn Fn(&Value) -> bool| rows.iter().position(predicate).unwrap();
    let tool_index = index(&|row| row["type"] == "acp_tool_call");
    let last_input = rows
        .iter()
        .rposition(|row| row["content"].to_string().contains("same 입력"))
        .unwrap();
    assert!(last_input < tool_index);
    assert!(tool_index < index(&|row| row["content"].to_string().contains("after input")));
    let transcript = history.to_string();
    assert_eq!(transcript.matches("original prompt").count(), 1);
    assert_eq!(transcript.matches("same 입력").count(), 2);
    assert!(transcript.find("before input") < transcript.find("same 입력"));
    assert!(transcript.rfind("same 입력") < transcript.find("after input"));
    for forbidden in ["session replay", "ignored", "relay-secret"] {
        assert!(!transcript.contains(forbidden));
    }

    read(
        host.client
            .post(format!("{root}/runtime/restart"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let rebound = read(desk_get().send().await.unwrap(), StatusCode::OK).await;
    let fresh = rebound["data"]["bindingId"].as_str().unwrap();
    assert_ne!(fresh, binding);
    code(
        desktop(echo, json!({"value":"stale"})).send().await.unwrap(),
        StatusCode::CONFLICT,
        "EXT_STALE_BINDING",
    )
    .await;
    let after_replay = read(
        host.client
            .get(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(after_replay, history);

    read(
        host.client
            .post(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"content":"next ordinary turn"}))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    let next = read(
        host.client
            .get(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let rows = next["data"]["items"].as_array().unwrap();
    let previous = rows
        .iter()
        .position(|row| row["content"].to_string().contains("after input"))
        .unwrap();
    let ordinary = rows
        .iter()
        .position(|row| row["content"].to_string().contains("next ordinary turn"))
        .unwrap();
    assert!(previous < ordinary);

    code(
        desk_get().header(header::COOKIE, &browser).send().await.unwrap(),
        StatusCode::FORBIDDEN,
        "SOURCE_FORBIDDEN",
    )
    .await;
    host.users
        .increment_session_generation("system_default_user")
        .await
        .unwrap();
    code(
        get().header(header::COOKIE, &browser).send().await.unwrap(),
        StatusCode::UNAUTHORIZED,
        "AUTH_REQUIRED",
    )
    .await;
    assert!(!secret_in_logs.0.load(std::sync::atomic::Ordering::Relaxed));
}

async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture phase reached");
}

async fn ensure_and_discover(host: &StudioHost, root: &str) -> String {
    read(
        host.client
            .post(format!("{root}/runtime/ensure"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    read(
        host.client
            .get(format!("{root}/acp-extensions"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await["data"]["bindingId"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn invoke(host: &StudioHost, root: &str, binding: &str, name: &str, payload: Value) -> reqwest::RequestBuilder {
    host.client.post(format!("{root}/acp-extensions")).json(&json!({
        "bindingId":binding, "method":format!("_aelix/session/probe/{name}"), "payload": payload,
    }))
}

#[tokio::test]
async fn acp_relay_command_override_does_not_inherit_the_builtin_launch_identity() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path()).await;
    let host = StudioHost::start_with_db(dir.path(), db, true).await;
    let path = dir.path().join("override");
    std::fs::write(
        &path,
        "#!/bin/sh\ntouch \"$(dirname \"$0\")/overridden-launch\"\nexec python3 \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    read(
        host.client
            .put(format!("{}/api/agents/fixture-aelix/overrides", host.url))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"command_override":path}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let id = create(&host, dir.path(), "fixture-aelix").await;
    let root = format!("{}/api/conversations/{id}", host.url);
    read(
        host.client
            .post(format!("{root}/runtime/ensure"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(dir.path().join("overridden-launch").is_file());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("relay-calls.initialize")).unwrap(),
        "standard"
    );
    code(
        host.client
            .get(format!("{root}/acp-extensions"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::CONFLICT,
        "EXT_UNAVAILABLE",
    )
    .await;
    assert!(!dir.path().join("relay-calls").exists());
}

#[derive(Clone, Default)]
struct QueuedRequest(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Write for QueuedRequest {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let line = String::from_utf8_lossy(buf);
        if line.contains("outgoing_protocol_actor") && line.contains("probe/queued") {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn acp_relay_logout_rejects_a_request_queued_behind_a_blocked_stdio_prompt() {
    rejects_queued_request(QueuedRevocation::Logout).await;
}

#[tokio::test]
async fn acp_relay_restart_admission_rejects_a_request_before_the_old_pipe_closes() {
    rejects_queued_request(QueuedRevocation::Restart).await;
}

#[tokio::test]
async fn acp_relay_generation_change_rejects_a_queued_request_with_a_still_valid_jwt() {
    rejects_queued_request(QueuedRevocation::DatabaseGeneration).await;
}

#[derive(Clone, Copy)]
enum QueuedRevocation {
    Logout,
    Restart,
    DatabaseGeneration,
}

async fn rejects_queued_request(revocation: QueuedRevocation) {
    let queued = QueuedRequest::default();
    let logs = queued.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || logs.clone())
        .finish();
    let _capture = tracing::subscriber::set_default(subscriber);
    let dir = tempfile::tempdir().unwrap();
    let db = fixture(dir.path()).await;
    let host = StudioHost::start_with_db(dir.path(), db, true).await;
    let id = create(&host, dir.path(), "fixture-aelix").await;
    let root = format!("{}/api/conversations/{id}", host.url);
    let binding = ensure_and_discover(&host, &root).await;
    let browser = host.login().await;
    let discovery = host
        .client
        .get(format!("{root}/acp-extensions"))
        .header(header::COOKIE, &browser)
        .send()
        .await
        .unwrap();
    let csrf_cookie = discovery
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next())
        .find(|value| value.starts_with("aionui-csrf-token="))
        .unwrap()
        .to_owned();
    read(discovery, StatusCode::OK).await;
    let cookies = format!("{browser}; {csrf_cookie}");
    let csrf = csrf_cookie.split_once('=').unwrap().1;
    read(
        invoke(&host, &root, &binding, "pause", json!({}))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    read(
        host.client
            .post(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"content": "p".repeat(512 * 1024)}))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    wait_until(|| dir.path().join("relay-calls.pipe-blocked").exists()).await;
    let request = invoke(&host, &root, &binding, "queued", json!({"value":"forbidden"}))
        .header(header::COOKIE, &cookies)
        .header(header::ORIGIN, &host.url)
        .header("x-csrf-token", csrf);
    let pending = tokio::spawn(async move { request.send().await.unwrap() });
    wait_until(|| queued.0.load(std::sync::atomic::Ordering::Acquire)).await;
    assert!(!pending.is_finished());
    let runtime = host.conversations.runtime_state();
    match revocation {
        QueuedRevocation::Restart => {
            // Keep the old pipe alive while the production restart gate
            // excludes the first write.
            runtime.begin_restart(&id).unwrap();
        }
        QueuedRevocation::DatabaseGeneration => {
            // Revoke after the initial HTTP session lookup and SDK admission.
            // Direct repository mutation leaves the JWT signature and local
            // blacklist alone, just like an out-of-process database writer.
            host.users
                .increment_session_generation("system_default_user")
                .await
                .unwrap();
        }
        QueuedRevocation::Logout => {
            read(
                host.client
                    .post(format!("{}/logout", host.url))
                    .header(header::COOKIE, &browser)
                    .header(header::ORIGIN, &host.url)
                    .send()
                    .await
                    .unwrap(),
                StatusCode::OK,
            )
            .await;
        }
    }
    std::fs::write(dir.path().join("relay-calls.resume-read"), "").unwrap();
    let response = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .unwrap()
        .unwrap();
    if matches!(revocation, QueuedRevocation::Restart) {
        code(response, StatusCode::CONFLICT, "EXT_STALE_BINDING").await;
        runtime.clear_restarting(&id);
    } else {
        code(response, StatusCode::UNAUTHORIZED, "AUTH_REQUIRED").await;
    }
    wait_until(|| dir.path().join("relay-calls.prompt-started").exists()).await;
    read(
        invoke(&host, &root, &binding, "finish", json!({}))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let calls = std::fs::read_to_string(dir.path().join("relay-calls")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["_aelix/session/probe/pause", "_aelix/session/probe/finish"]
    );
}

#[tokio::test]
async fn acp_relay_turn_reports_input_loss_for_a_slow_database_and_for_an_insert_failure() {
    use aionui_conversation::{ConversationAgentTurnRequest, ConversationAgentTurnStatus};

    for scenario in ["lag", "insert"] {
        let dir = tempfile::tempdir().unwrap();
        let db = fixture(dir.path()).await;
        let pool = db.pool().clone();
        if scenario == "insert" {
            sqlx::query(
                "CREATE TRIGGER reject_consumed_input BEFORE INSERT ON messages
                 WHEN NEW.position = 'right' AND instr(NEW.content,'blocked history') > 0
                 BEGIN SELECT RAISE(ABORT, 'fixture rejected history'); END",
            )
            .execute(&pool)
            .await
            .unwrap();
        }
        let host = StudioHost::start_with_db(dir.path(), db, true).await;
        let id = create(&host, dir.path(), "fixture-aelix").await;
        let root = format!("{}/api/conversations/{id}", host.url);
        let binding = ensure_and_discover(&host, &root).await;
        let convo = host.conversations.clone();
        let turn = tokio::spawn({
            let id = id.clone();
            async move {
                convo
                    .run_agent_turn(ConversationAgentTurnRequest {
                        user_id: "system_default_user".into(),
                        conversation_id: id,
                        content: "review fixture".into(),
                        files: vec![],
                        inject_skills: vec![],
                        required_runtime_mode: None,
                        persist_user_message: true,
                        user_message_hidden: false,
                        on_started: None,
                    })
                    .await
                    .unwrap()
            }
        });
        wait_until(|| dir.path().join("relay-calls.prompt-started").exists()).await;
        if scenario == "lag" {
            read(
                invoke(&host, &root, &binding, "burst", json!({}))
                    .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                    .send()
                    .await
                    .unwrap(),
                StatusCode::OK,
            )
            .await;
            // The one-connection pool keeps the relay parked on the first
            // history write while the adapter overflows the broadcast buffer.
            let guard = pool.acquire().await.unwrap();
            let turn_id = host.conversations.runtime_state().active_turn_id_for(&id).unwrap();
            let aionui_ai_agent::AgentInstance::Acp(agent) = host.tasks.get_task(&id).unwrap() else {
                panic!("expected live ACP runtime");
            };
            let history = agent.relay_input_history();
            std::fs::write(dir.path().join("relay-calls.emit"), "").unwrap();
            wait_until(|| {
                history.received(&turn_id) == 2
                    && host.tasks.get_task(&id).unwrap().status() == Some(aionui_common::ConversationStatus::Finished)
            })
            .await;
            assert!(!turn.is_finished(), "turn settled before history persistence");
            drop(guard);
        } else {
            read(
                invoke(&host, &root, &binding, "finish", json!({"failInsert":true}))
                    .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                    .send()
                    .await
                    .unwrap(),
                StatusCode::OK,
            )
            .await;
            // Test persistence failure independently of a concurrent
            // retirement of the binding before the HTTP receipt is sent.
            std::fs::write(dir.path().join("relay-calls.reject-insert"), "").unwrap();
        }
        let outcome = tokio::time::timeout(Duration::from_secs(15), turn)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed, "{scenario}");
        assert!(
            outcome.error_message.unwrap().contains("Consumed ACP user input"),
            "{scenario}"
        );
        let reloaded = read(
            host.client
                .get(format!("{root}/messages"))
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::OK,
        )
        .await
        .to_string();
        if scenario == "lag" {
            assert!(reloaded.contains("held input"));
        }
        assert!(!reloaded.contains(if scenario == "lag" {
            "lagged input"
        } else {
            "blocked history"
        }));
    }
}
