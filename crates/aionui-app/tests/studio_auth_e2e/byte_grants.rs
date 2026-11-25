use super::*;

#[derive(Clone, Default)]
struct ByteTrace(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for ByteTrace {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn byte_grant_sdk_trace_and_protected_wire() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    let trace = ByteTrace::default();
    let capture = trace.clone();
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || capture.clone())
            .finish(),
    );
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    std::fs::write(dir.join("relay-calls.host-bytes-enabled"), "").unwrap();
    std::fs::write(dir.join("relay-calls.diagnostic-fixture"), "").unwrap();
    let database = fixture(dir).await;
    let pool = database.pool().clone();
    let host = start_relay_host(dir, database).await;
    let id = create(&host, dir, "builtin-aelix").await;
    let root = format!("{}/api/conversations/{id}", host.url);
    let endpoint = format!("{root}/byte-grants");
    let file_name = "private-h1-file-文-quote.dat";
    let file = dir.join(file_name);
    let body_marker = "private-h1-body-文-\"-\\";
    let pixel_marker = b"private-h1-pixel-marker";
    let standard_raw = "private-h1-standard-raw";
    let standard_escaped = r#"private-h1-standard-"quoted"\끝"#;
    let standard_base64 = base64::engine::general_purpose::STANDARD.encode(standard_raw);

    let mut pixels = vec![0x7b; 200_000];
    pixels[..pixel_marker.len()].copy_from_slice(pixel_marker);
    let sha = format!("{:x}", Sha256::digest(&pixels));
    std::fs::write(&file, &pixels).unwrap();

    let post = |binding: &str, prepare: &str, path: &Path| {
        host.client.post(&endpoint).json(&json!({
            "prepareId":prepare, "bindingId":binding, "refs":[{"kind":"local","path":path}]
        }))
    };
    let desktop =
        |binding: &str, prepare: &str, path: &Path| post(binding, prepare, path).header(PRIVATE_HEADER, FIXTURE_TOKEN);
    code(
        post("b", "p", &file)
            .header(header::ORIGIN, &host.url)
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "AUTH_REQUIRED",
    )
    .await;
    code(
        desktop("b", "p", &file).send().await.unwrap(),
        StatusCode::CONFLICT,
        "EXT_NO_LIVE_TASK",
    )
    .await;

    let binding = ensure_and_discover(&host, &root).await;
    let discovery_url = format!("{root}/acp-extensions");
    let browser = host.login().await;
    let response = host
        .client
        .get(&discovery_url)
        .header(header::COOKIE, &browser)
        .send()
        .await
        .unwrap();
    let csrf_cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok()?.split(';').next())
        .find(|value| value.starts_with("aionui-csrf-token="))
        .unwrap()
        .to_owned();
    let csrf = csrf_cookie.split_once('=').unwrap().1;
    let cookies = format!("{browser}; {csrf_cookie}");
    assert_eq!(read(response, StatusCode::OK).await["data"]["hostBytes"], true);
    assert_eq!(
        std::fs::read_to_string(dir.join("relay-calls.provider-debug")).unwrap(),
        "False"
    );
    let browser_post = || post(&binding, "browser", &file).header(header::COOKIE, &cookies);
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
    let from_browser = read(
        browser_post()
            .header(header::ORIGIN, &host.url)
            .header("x-csrf-token", csrf)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(from_browser["data"]["grants"][0]["sha256"], sha);

    let other = host
        .client
        .post(format!("{}/api/conversations/other-conversation/byte-grants", host.url))
        .header(PRIVATE_HEADER, FIXTURE_TOKEN)
        .json(&json!({
            "prepareId":"p", "bindingId":binding, "refs":[{"kind":"local","path":file}]
        }))
        .send()
        .await
        .unwrap();
    code(other, StatusCode::NOT_FOUND, "CONVERSATION_NOT_FOUND").await;
    code(
        desktop("stale", "p", &file).send().await.unwrap(),
        StatusCode::CONFLICT,
        "EXT_STALE_BINDING",
    )
    .await;
    code(
        desktop(&binding, "bad", &file.with_extension("missing"))
            .send()
            .await
            .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "BYTE_GRANT_FILE",
    )
    .await;

    let (first, duplicate) = tokio::join!(
        desktop(&binding, "p", &file).send(),
        desktop(&binding, "p", &file).send()
    );
    assert_eq!(first.as_ref().unwrap().headers()[header::CACHE_CONTROL], "no-store");
    let first = read(first.unwrap(), StatusCode::OK).await["data"].clone();
    let duplicate = read(duplicate.unwrap(), StatusCode::OK).await["data"].clone();
    assert_eq!(first["grants"], duplicate["grants"]);
    assert_eq!(first["expiresAtMs"], duplicate["expiresAtMs"]);
    let grant = first["grants"][0]["id"].as_str().unwrap();
    assert_eq!(first["grants"][0]["byteLength"], pixels.len());
    assert_eq!(first["grants"][0]["sha256"], sha);
    assert!(first["expiresInMs"].as_u64().unwrap() <= 60_000);
    code(
        desktop(&binding, "p", dir).send().await.unwrap(),
        StatusCode::CONFLICT,
        "BYTE_GRANT_CONFLICT",
    )
    .await;

    // Replace the file after authorization. Retries return the same immutable snapshot.
    std::fs::remove_file(&file).unwrap();
    std::fs::write(&file, b"new content").unwrap();
    let replay = read(desktop(&binding, "p", &file).send().await.unwrap(), StatusCode::OK).await;
    assert_eq!(replay["data"]["grants"], first["grants"]);
    assert_eq!(replay["data"]["expiresAtMs"], first["expiresAtMs"]);

    let call = |name, payload| invoke(&host, &root, &binding, name, payload).header(PRIVATE_HEADER, FIXTURE_TOKEN);
    let denied = read(
        call(
            "byte-denied",
            json!({"grant":grant,"sessionOverride":"another-session"}),
        )
        .send()
        .await
        .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(denied["data"]["result"]["code"], -33110);

    // Hold the authoritative owner fence after the outer SDK request begins.
    let begin = call("byte-denied", json!({"grant":grant,"holdForDatabase":true}));
    let waiting = tokio::spawn(async move { begin.send().await.unwrap() });
    wait_until(|| dir.join("relay-calls.reverse-paused").exists()).await;
    let transaction = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let started = tokio::time::Instant::now();
    std::fs::write(dir.join("relay-calls.reverse-go"), "").unwrap();
    let denied = read(
        tokio::time::timeout(Duration::from_secs(12), waiting)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert!(started.elapsed() >= Duration::from_secs(5));
    assert_eq!(denied["data"]["result"]["code"], -33110);
    transaction.rollback().await.unwrap();

    let mut upgrade = host.upgrade();
    upgrade.headers_mut().insert(header::COOKIE, browser.parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    let turn = read(
        host.client
            .post(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"content":"standard prompt"}))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await["data"]["turn_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_until(|| dir.join("relay-calls.prompt-started").exists()).await;
    // Positive wire control: the standard update really traversed the SDK
    // and reached the existing WS projection with its unmodified raw output.
    let update = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = websocket.next().await.unwrap().unwrap();
            if let Ok(text) = frame.to_text()
                && text.contains(&standard_base64)
            {
                break serde_json::from_str::<Value>(text).unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert!(
        update
            .to_string()
            .contains(&serde_json::to_string(standard_escaped).unwrap())
    );

    let send = call(
        "bytes",
        json!({"grant":grant,"length":pixels.len(),"sha256":sha,"marker":body_marker}),
    );
    let outer = tokio::spawn(async move { send.send().await.unwrap() });
    let item = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let confirmations = read(
                host.client
                    .get(format!("{root}/confirmations"))
                    .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                    .send()
                    .await
                    .unwrap(),
                StatusCode::OK,
            )
            .await;
            if let Some(item) = confirmations["data"].as_array().unwrap().first() {
                break item.clone();
            }
            assert!(!outer.is_finished(), "outer H1 request settled before permission");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(item["call_id"], "byte-permission");
    read(
        host.client
            .post(format!("{root}/confirmations/byte-permission/confirm"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"msg_id":item["id"],"data":{"option_id":"allow"}}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    wait_until(|| dir.join("relay-calls.byte-permission-answered").exists()).await;
    assert!(!outer.is_finished(), "outer H1 request settled before Stop");
    read(
        host.client
            .post(format!("{root}/cancel"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"turn_id":turn}))
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let result = read(
        tokio::time::timeout(Duration::from_secs(10), outer)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    let result = &result["data"]["result"];
    assert!((196_605..=196_608).contains(&result["lineBytes"].as_u64().unwrap()));
    for key in ["replayed", "released", "permission", "stop"] {
        assert_eq!(result[key], true);
    }

    drop(websocket);
    let unsafe_error = call(
        "byte-error",
        json!({"marker":body_marker,"grant":grant,"sha256":sha,"path":file}),
    )
    .send()
    .await
    .unwrap();
    let sanitized = read(unsafe_error, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(sanitized["details"], json!({"rpcCode": -32603}));
    assert!(!sanitized.to_string().contains("private-h1"));
    let revoked = from_browser["data"]["grants"][0]["id"].as_str().unwrap();
    host.users
        .increment_session_generation("system_default_user")
        .await
        .unwrap();
    let denied = read(
        call("byte-denied", json!({"grant":revoked})).send().await.unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(denied["data"]["result"]["code"], -33110);
    let still_live = read(
        desktop(&binding, "replace-connection", &file).send().await.unwrap(),
        StatusCode::OK,
    )
    .await;
    let old = still_live["data"]["grants"][0]["id"].as_str().unwrap();
    code(
        call("byte-disconnect", json!({"marker":body_marker,"grant":old}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_GATEWAY,
        "EXT_OUTCOME_UNKNOWN",
    )
    .await;
    host.tasks.kill_and_wait(&id, None).await;
    let fresh = ensure_and_discover(&host, &root).await;
    assert_ne!(fresh, binding);
    let denied = read(
        invoke(&host, &root, &fresh, "byte-denied", json!({"grant":old}))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(denied["data"]["result"]["code"], -33110);
    let mut upgrade = host.upgrade();
    upgrade
        .headers_mut()
        .insert(PRIVATE_HEADER, FIXTURE_TOKEN.parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    let error_turn = read(
        host.client
            .post(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"content":"emit standard error"}))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    let error_turn = error_turn["data"]["turn_id"].as_str().unwrap();
    let public_error = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = websocket.next().await.unwrap().unwrap();
            if let Ok(text) = frame.to_text()
                && text.contains(error_turn)
                && text.contains("USER_AGENT_TURN_FAILED_AFTER_DISPATCH")
            {
                break text.to_owned();
            }
        }
    })
    .await
    .unwrap();
    assert!(!public_error.contains("private-h1-standard"));

    drop(websocket);
    host.tasks.kill_and_wait(&id, None).await;

    let log = String::from_utf8(trace.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("http response") && log.contains("/byte-grants"));
    assert!(
        !log.contains("Received JSON-RPC message"),
        "protected SDK wire trace must be suppressed"
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(&pixels[..48]);
    for marker in [
        file.to_str().unwrap(),
        file_name,
        body_marker,
        "private-h1-",
        standard_raw,
        standard_escaped,
        &standard_base64,
        std::str::from_utf8(pixel_marker).unwrap(),
        &encoded,
        &sha,
        grant,
        old,
        revoked,
        "data:application/octet-stream;base64,",
    ] {
        let escaped = serde_json::to_string(marker).unwrap();
        assert!(
            !log.contains(marker) && !log.contains(&escaped[1..escaped.len() - 1]),
            "a private byte-grant marker reached host diagnostics"
        );
    }
}

#[tokio::test]
async fn byte_grant_resolves_project_upload_and_local_as_one_ordered_batch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("relay-calls.host-bytes-enabled"), "").unwrap();
    let host = start_relay_host(dir.path(), fixture(dir.path()).await).await;
    let root = format!(
        "{}/api/conversations/{}",
        host.url,
        create(&host, dir.path(), "builtin-aelix").await
    );
    let binding = ensure_and_discover(&host, &root).await;
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().join("file.dat");
    std::fs::write(&path, b"project-or-local").unwrap();
    let project = host
        .projects
        .create_standard(
            "system_default_user",
            aionui_project::canonical::to_file_uri(workspace.path()).unwrap(),
        )
        .await
        .unwrap();
    let upload_root = std::env::temp_dir().join("aionui");
    std::fs::create_dir_all(&upload_root).unwrap();
    let managed = tempfile::tempdir_in(upload_root).unwrap();
    let upload = managed.path().join("blob.dat");
    std::fs::write(&upload, b"upload").unwrap();

    let prepare = |refs: Value| {
        host.client
            .post(format!("{root}/byte-grants"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"prepareId":"ordered","bindingId":binding,"refs":refs}))
    };
    let local = json!({"kind":"local","path":path});
    let uploaded = json!({"kind":"upload","path":upload});
    let project_id = &project.project_explorer.pe_id;
    code(
        prepare(json!([
            local, {"kind":"project","pe_id":project_id,"relative_path":"../file.dat"}, uploaded
        ]))
        .send()
        .await
        .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "BYTE_GRANT_FILE",
    )
    .await;
    code(
        prepare(json!([
            local, {"kind":"upload","path":path}
        ]))
        .send()
        .await
        .unwrap(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "BYTE_GRANT_FILE",
    )
    .await;
    let success = read(
        prepare(json!([
            uploaded, {"kind":"project","pe_id":project_id,"relative_path":"file.dat"}, local
        ]))
        .send()
        .await
        .unwrap(),
        StatusCode::OK,
    )
    .await;
    let grants = success["data"]["grants"].as_array().unwrap();
    assert_eq!(
        grants
            .iter()
            .map(|grant| grant["byteLength"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [6, 16, 16]
    );
    assert_eq!(grants[1]["sha256"], grants[2]["sha256"]);
    assert_ne!(grants[0]["sha256"], grants[1]["sha256"]);
    let id = root.rsplit('/').next().unwrap();
    host.tasks.kill_and_wait(id, None).await;
}

#[tokio::test]
async fn byte_grant_rpc_refusals_preserve_safe_studio_recovery_context() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    std::fs::write(dir.join("relay-calls.host-bytes-enabled"), "").unwrap();
    let host = start_relay_host(dir, fixture(dir).await).await;
    let sensitive = r#"private-h1-"/operator/private-extension.py""#;

    for (mut input, expected) in [
        (
            json!({"kind":"restrictions_unenforceable","methods":[
                "agents/use", sensitive, "tools/set_active", "permissions/set", "permissions/set"
            ]}),
            json!({"kind":"restrictions_unenforceable","methods":["agents/use","tools/set_active","permissions/set"]}),
        ),
        (
            json!({"kind":"profile_unavailable","profile":sensitive}),
            json!({"kind":"profile_unavailable","profile":"saved session profile"}),
        ),
        (
            json!({"kind":"extension_owned_by_launch","source":sensitive}),
            json!({"kind":"extension_owned_by_launch","source":"launch-managed extension"}),
        ),
    ] {
        input["privateExtra"] = json!(sensitive);
        std::fs::write(dir.join("relay-calls.recovery-fixture"), input.to_string()).unwrap();
        let id = create(&host, dir, "builtin-aelix").await;
        let rejection = read(
            host.client
                .post(format!("{}/api/conversations/{id}/runtime/ensure", host.url))
                .header(PRIVATE_HEADER, FIXTURE_TOKEN)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_GATEWAY,
        )
        .await;
        assert_eq!(rejection["code"], "USER_AGENT_OPERATION_REFUSED");
        assert_eq!(rejection["details"]["protocol_code"], -32004);
        assert_eq!(rejection["details"]["data"], expected);
        assert!(!rejection.to_string().contains("private-h1-"));
        assert!(!rejection.to_string().contains("private-extension.py"));
        host.tasks.kill_and_wait(&id, None).await;
    }
}

#[tokio::test]
async fn protected_prompt_model_selection_refusal_reaches_studio_live_and_history() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    std::fs::write(dir.join("relay-calls.host-bytes-enabled"), "").unwrap();
    let refusal = model_selection_refusal();
    std::fs::write(dir.join("relay-calls.prompt-refusal"), refusal.to_string()).unwrap();
    let host = start_relay_host(dir, fixture(dir).await).await;
    let id = create(&host, dir, "builtin-aelix").await;
    let root = format!("{}/api/conversations/{id}", host.url);
    ensure_and_discover(&host, &root).await;
    let discovery = read(
        host.client
            .get(format!("{root}/acp-extensions"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .send()
            .await
            .unwrap(),
        StatusCode::OK,
    )
    .await;
    assert_eq!(discovery["data"]["hostBytes"], true);
    assert_model_selection_refusal(&host, &id, refusal).await;
    // The fixture appends before each reply, including in a replacement worker.
    let requests = std::fs::read_to_string(dir.join("relay-calls.prompt-requests")).unwrap();
    assert_eq!(
        requests.lines().count(),
        1,
        "prompt requests across workers: {requests}"
    );
    host.tasks.kill_and_wait(&id, None).await;
}

#[tokio::test]
async fn legacy_prompt_model_selection_refusal_hides_peer_text() {
    let trace = ByteTrace::default();
    let capture = trace.clone();
    let _subscriber = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || capture.clone())
            .finish(),
    );
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    let refusal = model_selection_refusal();
    std::fs::write(dir.join("relay-calls.prompt-refusal"), refusal.to_string()).unwrap();
    let host = start_relay_host(dir, fixture(dir).await).await;
    let id = create(&host, dir, "custom-aelix").await;
    assert_model_selection_refusal(&host, &id, refusal).await;
    host.tasks.kill_and_wait(&id, None).await;

    assert_eq!(
        std::fs::read_to_string(dir.join("relay-calls.initialize")).unwrap(),
        "standard"
    );
    let log = String::from_utf8(trace.0.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("Received JSON-RPC message"),
        "legacy SDK tracing must be exercised"
    );
    assert!(
        !log.contains("private-h1-standard"),
        "peer refusal text reached host diagnostics"
    );
}

fn model_selection_refusal() -> Value {
    json!({"kind": "model_selection_required", "configId": "model", "availableModels": 2, "retryable": false})
}

async fn assert_model_selection_refusal(host: &StudioHost, id: &str, refusal: Value) {
    let root = format!("{}/api/conversations/{id}", host.url);
    let mut upgrade = host.upgrade();
    upgrade
        .headers_mut()
        .insert(PRIVATE_HEADER, FIXTURE_TOKEN.parse().unwrap());
    let (mut websocket, _) = tokio_tungstenite::connect_async(upgrade).await.unwrap();
    let accepted = read(
        host.client
            .post(format!("{root}/messages"))
            .header(PRIVATE_HEADER, FIXTURE_TOKEN)
            .json(&json!({"content": "Choose a model first"}))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    let turn_id = accepted["data"]["turn_id"].as_str().unwrap();
    let expected = json!({
        "message": "The Agent cannot perform this operation",
        "protocol_code": -32004,
        "data": refusal,
        "code": "USER_AGENT_OPERATION_REFUSED",
        "ownership": "user_agent",
        "detail": "ACP request failed",
        "retryable": false,
        "feedback_recommended": false,
    });
    let live_error = tokio::time::timeout(Duration::from_secs(30), async {
        let mut refusal = None;
        loop {
            let frame = websocket.next().await.unwrap().unwrap();
            if let Ok(text) = frame.to_text() {
                assert!(!text.contains("private-h1-standard"), "peer text reached the stream");
                let event: Value = serde_json::from_str(text).unwrap();
                if event["data"]["conversation_id"] != id || event["data"]["turn_id"] != turn_id {
                    continue;
                }
                match event["name"].as_str() {
                    Some("message.stream") if event["data"]["type"] == "error" => {
                        refusal = Some(event["data"]["data"].clone());
                    }
                    Some("turn.completed") => break refusal.expect("turn completed without an error frame"),
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap();

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
    assert!(
        !history.to_string().contains("private-h1-standard"),
        "peer text reached history"
    );
    let tip = history["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["type"] == "tips")
        .expect("the completed refusal must be in history");
    assert_eq!(live_error, expected);
    assert_eq!(
        tip["content"],
        json!({"type": "error", "content": expected["message"], "error": expected}),
    );
}
