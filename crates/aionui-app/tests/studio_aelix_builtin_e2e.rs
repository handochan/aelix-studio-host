//! Product launch and admission regressions against the real host executable and an isolated ACP peer.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};
use tokio::process::{Child, Command};

const OPERATOR: &str = "system_default_user";
const DESKTOP_SECRET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const JWT_SECRET: &str = "host67-fixture-jwt-key-used-only-by-isolated-test";
const ENTRY: &str = "builtin-aelix";
const HEALTH: &str = "/api/agents/builtin-aelix/health-check";
const SENTINEL: &str = "/PRIVATE/HOST67_SENTINEL/runtime/worker.py";

const PEER: &str = r#"
import json, os, pathlib, sys, time

here = pathlib.Path(__file__).parent
(here / 'spawned').write_text('private' if 'AIONUI_STUDIO_AELIX_LAUNCH' not in os.environ else 'inherited')
fail = (here / 'fail-check').is_file()
for raw in sys.stdin:
    request = json.loads(raw)
    method = request.get('method')
    params = request.get('params', {})
    reply = {'jsonrpc': '2.0', 'id': request.get('id')}
    if method == 'initialize':
        offered = params.get('clientCapabilities', {}).get('_meta', {}).get('aelix/sessionRelay')
        (here / 'relay-offer').write_text('offered' if offered else 'closed')
        reply['result'] = {
            'protocolVersion': 1,
            'agentInfo': {'name': 'aelix-acp', 'version': '0.0.1'},
            'agentCapabilities': {'_meta': {'aelix/sessionRelay': {
                'version': 1, 'requests': ['_aelix/session/state'], 'notifications': []}}}
        }
        client_meta = params.get('clientCapabilities', {}).get('_meta', {})
        if (here / 'socket-path-check').is_file() and client_meta.get('aelix/hostBytes') == {'version': 1}:
            reply['result']['agentCapabilities']['_meta']['aelix/hostBytes'] = {'version': 1}
    elif method == 'session/new':
        if (here / 'terminal-check').is_file():
            marker = here / 'terminal-descriptor-visibility'
            command = 'if [ "${AIONUI_STUDIO_AELIX_LAUNCH+x}" = x ]; then printf inherited; else printf private; fi > "$1"'
            print(json.dumps({'jsonrpc': '2.0', 'id': 991, 'method': 'terminal/create',
                 'params': {'sessionId': 'fixture-session', 'command': '/bin/sh',
                 'args': ['-c', command, 'terminal-fixture', str(marker)]}}), flush=True)
            terminal = json.loads(sys.stdin.readline())
            assert terminal.get('result', {}).get('terminalId'), terminal
            deadline = time.monotonic() + 5
            while not marker.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
        if (here / 'socket-path-check').is_file():
            reply['error'] = {'code': -33004, 'message': '/PRIVATE/HOST72_SENTINEL/peer-message',
                              'data': {'kind': 'socket_path_too_long', 'limitBytes': 104,
                                       'path': '/PRIVATE/HOST72_SENTINEL/socket', 'token': 'fixture-secret',
                                       'stderr': '/PRIVATE/HOST72_SENTINEL/stderr'}}
        elif fail:
            reply['error'] = {'code': -32603, 'message': '/PRIVATE/HOST67_SENTINEL/runtime/worker.py',
                              'data': {'code': 'FIXTURE_FAILURE', 'detail': '/PRIVATE/HOST67_SENTINEL/runtime/worker.py'}}
        else:
            reply['result'] = {'sessionId': 'fixture-session'}
            binding = params.get('_meta', {}).get('aelix/sessionRelay')
            if binding is not None:
                reply['result']['_meta'] = {'aelix/sessionRelay': binding}
    else:
        continue
    print(json.dumps(reply), flush=True)
"#;

fn python() -> PathBuf {
    aionui_runtime::resolve_command_path("python3").expect("python3 for the fixture")
}

fn runtime(root: &Path, name: &str, failing: bool) -> (PathBuf, String) {
    let runtime = root.join(name);
    std::fs::create_dir_all(&runtime).unwrap();
    let launcher = runtime.join("launch.py");
    std::fs::write(&launcher, PEER).unwrap();
    if failing {
        std::fs::write(runtime.join("fail-check"), "").unwrap();
    }
    let descriptor = json!({
        "command": python(),
        "args": ["-I","-S","-B","-X","utf8",launcher,"acp","--profile","coding"],
        "env": {
            "AELIX_CODING_AGENT_DIR": root.join("agent state"),
            "AELIX_ACP_STUDIO_MANAGED": "1"
        }
    });
    (runtime, descriptor.to_string())
}

struct Host {
    child: Child,
    base: String,
    log: PathBuf,
    client: Client,
    protected: bool,
}

impl Host {
    async fn start(root: &Path, descriptor: &str, protected: bool) -> Self {
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let log = root.join("host-capture.log");
        let log_file = std::fs::File::create(&log).unwrap();
        let binary = std::env::var_os("HOST67_REVIEW_BASELINE")
            .map(PathBuf::from)
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_aelix-studio-host").into());
        let mut command = Command::new(binary);
        command
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                "0",
                "--log-level",
                "debug",
                "--data-dir",
            ])
            .arg(root)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("JWT_SECRET", JWT_SECRET)
            .env(aionui_runtime::STUDIO_AELIX_LAUNCH_ENV, descriptor)
            .env("AIONUI_EXTENSIONS_PATH", root.join("extensions"))
            .stdin(Stdio::piped())
            .stdout(log_file.try_clone().unwrap())
            .stderr(log_file)
            .kill_on_drop(true);
        if protected {
            command.args(["--local", "--studio-auth"]);
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        if protected {
            let mut stdin = stdin;
            stdin.write_all(DESKTOP_SECRET.as_bytes()).await.unwrap();
        }
        let client = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(35))
            .build()
            .unwrap();
        let port = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let lines = std::fs::read_to_string(&log).unwrap();
                if let Some(port) = lines.lines().find_map(|line| {
                    let json = line.strip_prefix("AIONCORE_LISTENING ")?;
                    serde_json::from_str::<Value>(json).ok()?["port"].as_u64()
                }) {
                    let mut request = client
                        .get(format!("http://127.0.0.1:{port}/health"))
                        .timeout(Duration::from_secs(1));
                    if protected {
                        request = request.header("x-aionui-studio-auth", DESKTOP_SECRET);
                    }
                    if request
                        .send()
                        .await
                        .is_ok_and(|response| response.status().is_success())
                    {
                        break port;
                    }
                }
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "host exited before listening: {lines}"
                );
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .expect("host did not listen");
        Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            log,
            client,
            protected,
        }
    }

    async fn call(&self, method: Method, path: &str, body: Value, token: Option<&str>) -> (StatusCode, Value) {
        let mut request = self.client.request(method, format!("{}{path}", self.base));
        if self.protected {
            request = request.header("x-aionui-studio-auth", DESKTOP_SECRET);
        }
        if let Some(token) = token {
            request = request
                .bearer_auth(token)
                .header("x-csrf-token", "host67-test-csrf")
                .header("cookie", "aionui-csrf-token=host67-test-csrf");
        }
        if !body.is_null() {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn ok(&self, method: Method, path: &str, body: Value) -> Value {
        let (status, body) = self.call(method, path, body, None).await;
        assert!(status.is_success(), "{path} {status}: {body}");
        body["data"].clone()
    }

    async fn conversation(&self, id: &str, root: &Path) -> String {
        self.ok(
            Method::POST,
            "/api/conversations",
            json!({
                "type":"acp", "name":"Aelix fixture", "extra": {
                    "agent_id":id, "backend":"aelix", "workspace":root, "custom_workspace":true
                }
            }),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn stop(mut self) {
        self.child.kill().await.unwrap();
    }
}

async fn initialized_catalog(root: &Path) -> sqlx::SqlitePool {
    // Let the executable under test apply its own migration checksums. This
    // also lets these assertions run against the executable from before a fix.
    Host::start(root, "", true).await.stop().await;
    sqlx::sqlite::SqlitePoolOptions::new()
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(root.join("aionui-backend.db")))
        .await
        .unwrap()
}

fn catalog_aelix(rows: &Value) -> &Value {
    rows.as_array().unwrap().iter().find(|row| row["id"] == ENTRY).unwrap()
}

#[tokio::test]
async fn aelix_failed_check_does_not_follow_a_runtime_move() {
    let dir = tempfile::tempdir().unwrap();
    let (broken, first) = runtime(dir.path(), "failing runtime", true);
    let host = Host::start(dir.path(), &first, true).await;
    let check = host.ok(Method::POST, HEALTH, Value::Null).await;
    assert!(broken.join("spawned").is_file());
    assert_eq!(check["status"], "offline");
    host.stop().await;
    let (_, second) = runtime(dir.path(), "working runtime elsewhere", false);
    let host = Host::start(dir.path(), &second, true).await;
    let management = host.ok(Method::GET, "/api/agents/management", Value::Null).await;
    assert_eq!(catalog_aelix(&management)["status"], "online");
    let assistants = host.ok(Method::GET, "/api/assistants", Value::Null).await;
    assert!(
        assistants
            .as_array()
            .unwrap()
            .iter()
            .any(|assistant| assistant["agent_id"] == ENTRY)
    );
    assert_eq!(host.ok(Method::POST, HEALTH, Value::Null).await["status"], "online");
    host.stop().await;
}

#[tokio::test]
async fn aelix_private_probe_omits_peer_path_from_http_database_and_diagnostics() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, descriptor) = runtime(dir.path(), "failing runtime", true);
    let host = Host::start(dir.path(), &descriptor, true).await;
    let check = host.ok(Method::POST, HEALTH, Value::Null).await;
    assert!(peer.join("spawned").is_file());
    assert_eq!(check["status"], "offline");
    assert_eq!(check["last_check_error_code"], "acp_init_failed");
    assert!(!check.to_string().contains(SENTINEL), "peer path in health response");
    assert_eq!(
        check["last_check_error_message"],
        "Builtin Aelix did not complete the ACP connection check"
    );
    let list = host.ok(Method::GET, "/api/agents/management", Value::Null).await;
    assert!(!list.to_string().contains(SENTINEL));
    assert!(!std::fs::read_to_string(&host.log).unwrap().contains(SENTINEL));
    host.stop().await;
    let db = aionui_db::init_database(&dir.path().join("aionui-backend.db"))
        .await
        .unwrap();
    let saved: Option<String> =
        sqlx::query_scalar("SELECT last_check_error_message FROM agent_metadata WHERE agent_id = ?")
            .bind(ENTRY)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(saved.is_none(), "a runtime health verdict was persisted");
}

#[tokio::test]
async fn acp_socket_path_error_is_structured_on_runtime_ensure_and_custom_registration() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, descriptor) = runtime(dir.path(), "socket path failure", false);
    std::fs::write(peer.join("socket-path-check"), "").unwrap();
    let host = Host::start(dir.path(), &descriptor, true).await;
    let conversation = host.conversation(ENTRY, dir.path()).await;
    let (runtime_status, runtime_body) = host
        .call(
            Method::POST,
            &format!("/api/conversations/{conversation}/runtime/ensure"),
            Value::Null,
            None,
        )
        .await;
    let (registration_status, registration_body) = host
        .call(
            Method::POST,
            "/api/agents/custom",
            json!({
                "name":"Socket path fixture", "command":"python3",
                "args":[peer.join("launch.py")]
            }),
            None,
        )
        .await;

    assert_eq!(runtime_status, StatusCode::BAD_GATEWAY, "{runtime_body}");
    assert_eq!(
        runtime_body,
        json!({
            "success":false, "code":"BAD_GATEWAY", "error":"ACP runtime initialization failed.",
            "details":{"kind":"socket_path_too_long","limitBytes":104}
        })
    );
    assert_eq!(registration_status, StatusCode::BAD_REQUEST, "{registration_body}");
    assert_eq!(
        registration_body,
        json!({
            "success":false, "code":"BAD_REQUEST", "error":"acp_init_failed: ACP initialization failed",
            "details":{"kind":"socket_path_too_long","limitBytes":104}
        })
    );
    host.stop().await;
}

#[tokio::test]
async fn aelix_disabled_row_cannot_spawn_for_existing_conversation_or_health_check() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, descriptor) = runtime(dir.path(), "available runtime", false);
    let host = Host::start(dir.path(), &descriptor, true).await;
    let conversation = host.conversation(ENTRY, dir.path()).await;
    host.ok(
        Method::PATCH,
        "/api/agents/builtin-aelix/enabled",
        json!({"enabled":false}),
    )
    .await;
    let (health_status, health) = host.call(Method::POST, HEALTH, Value::Null, None).await;
    let route = format!("/api/conversations/{conversation}/runtime/ensure");
    let (open_status, open) = host.call(Method::POST, &route, Value::Null, None).await;
    assert_eq!(
        (health_status, open_status),
        (StatusCode::BAD_REQUEST, StatusCode::BAD_REQUEST),
        "disabled health and conversation launch | {health} | {open}"
    );
    assert!(!peer.join("spawned").exists(), "disabled row spawned a peer");
    host.stop().await;
}

#[tokio::test]
async fn aelix_other_builtin_never_receives_relay_offer() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, descriptor) = runtime(dir.path(), "other builtin", false);
    let db = initialized_catalog(dir.path()).await;
    sqlx::query(
        "INSERT INTO agent_metadata
        (id,agent_id,name,backend,agent_type,agent_source,command,args,created_at,updated_at)
        VALUES ('old-fixture','old-fixture','Legacy builtin','aelix','acp','builtin',?1,?2,1,1)",
    )
    .bind(python().to_str().unwrap())
    .bind(json!([peer.join("launch.py")]).to_string())
    .execute(&db)
    .await
    .unwrap();
    db.close().await;
    let host = Host::start(dir.path(), &descriptor, true).await;
    let conversation = host.conversation("old-fixture", dir.path()).await;
    host.ok(
        Method::POST,
        &format!("/api/conversations/{conversation}/runtime/ensure"),
        Value::Null,
    )
    .await;
    assert_eq!(std::fs::read_to_string(peer.join("relay-offer")).unwrap(), "closed");
    host.stop().await;
}

#[tokio::test]
async fn aelix_descriptor_is_private_to_host_acp_mcp_and_terminal_children() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, descriptor) = runtime(dir.path(), "private runtime", false);
    std::fs::write(peer.join("terminal-check"), "").unwrap();
    let host = Host::start(dir.path(), &descriptor, true).await;
    assert_eq!(host.ok(Method::POST, HEALTH, Value::Null).await["status"], "online");
    assert_eq!(std::fs::read_to_string(peer.join("spawned")).unwrap(), "private");
    let marker = dir.path().join("mcp-descriptor-visibility");
    let script = "import os,pathlib,sys; pathlib.Path(sys.argv[1]).write_text('inherited' if 'AIONUI_STUDIO_AELIX_LAUNCH' in os.environ else 'private')";
    host.call(
        Method::POST,
        "/api/mcp/test-connection",
        json!({
            "name":"descriptor-visibility", "transport":{"type":"stdio","command":python(),"args":["-c",script,marker]}
        }),
        None,
    )
    .await;
    assert_eq!(
        (
            std::fs::read_to_string(marker).unwrap(),
            std::fs::read_to_string(peer.join("terminal-descriptor-visibility")).unwrap()
        ),
        ("private".to_owned(), "private".to_owned())
    );
    host.stop().await;
}

#[tokio::test]
async fn aelix_generic_multi_user_listener_cannot_use_or_mutate_the_operator_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let (peer, descriptor) = runtime(dir.path(), "operator-only runtime", false);
    let db = initialized_catalog(dir.path()).await;
    sqlx::query(
        "INSERT INTO users
        (id,user_type,username,password_hash,status,session_generation,created_at,updated_at)
        VALUES ('second-principal','local','second-principal','fixture','active',0,1,1)",
    )
    .execute(&db)
    .await
    .unwrap();
    let operator_generation: i64 = sqlx::query_scalar("SELECT session_generation FROM users WHERE id = ?")
        .bind(OPERATOR)
        .fetch_one(&db)
        .await
        .unwrap();
    db.close().await;
    let jwt = aionui_auth::JwtService::new(JWT_SECRET.into());
    let host = Host::start(dir.path(), &descriptor, false).await;
    for (principal, generation) in [(OPERATOR, operator_generation), ("second-principal", 0)] {
        let token = jwt
            .sign_with_session_generation(principal, principal, generation)
            .unwrap();
        let (status, catalog) = host
            .call(Method::GET, "/api/agents/management", Value::Null, Some(&token))
            .await;
        assert_eq!(status, StatusCode::OK, "{principal} should be authenticated");
        let (override_status, override_body) = host
            .call(
                Method::PUT,
                "/api/agents/builtin-aelix/overrides",
                json!({"env_override":[{"name":"HOST67_FIXTURE_OVERRIDE","value":"synthetic"}]}),
                Some(&token),
            )
            .await;
        let (health_status, health_body) = host.call(Method::POST, HEALTH, Value::Null, Some(&token)).await;
        assert_eq!(
            (
                catalog_aelix(&catalog["data"])["status"].as_str(),
                override_status,
                health_status
            ),
            (Some("missing"), StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
            "{principal}"
        );
        assert_eq!(override_body["code"], "FORBIDDEN");
        assert_eq!(health_body["code"], "FORBIDDEN");
    }
    assert!(!peer.join("spawned").exists());
    host.stop().await;

    let operator_host = Host::start(dir.path(), &descriptor, true).await;
    let row = operator_host
        .ok(
            Method::PUT,
            "/api/agents/builtin-aelix/overrides",
            json!({"env_override":[{"name":"HOST67_FIXTURE_OVERRIDE","value":"operator"}]}),
        )
        .await;
    assert_eq!(row["env_override_key_count"], 1);
    assert_eq!(row["status"], "online");
    assert!(peer.join("spawned").is_file());
    operator_host.stop().await;
}
