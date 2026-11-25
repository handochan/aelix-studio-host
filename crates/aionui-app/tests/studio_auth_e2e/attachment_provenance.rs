use super::*;
use base64::Engine as _;

#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + use<> {
        let writer = self.clone();
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish()
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[tokio::test]
async fn attachment_provenance_matches_normal_send_and_running_metadata_without_diagnostic_payloads() {
    use std::os::unix::fs::symlink;

    let trace = Capture::default();
    tracing::subscriber::set_global_default(trace.subscriber()).expect("capture Host and blocking worker events");
    let workspace = tempfile::tempdir().unwrap();
    let base = workspace.path();
    std::fs::write(base.join("relay-calls.attachment-fixture"), "").unwrap();
    let host = start_relay_host(base, fixture(base).await).await;
    let id = create(&host, base, "builtin-aelix").await;
    let messages = format!("{}/api/conversations/{id}/messages", host.url);
    let metadata = format!("{}/api/fs/content/metadata", host.url);
    let post = |url| host.client.post(url).header(PRIVATE_HEADER, FIXTURE_TOKEN);

    let external = tempfile::tempdir().unwrap();
    let name = r#"h2-path-"quoted"\첨부.png"#;
    let local = external.path().join(name);
    let bytes = b"private-h2-content-marker-for-wire";
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let content = r#"h2-prompt-"quoted"\원문"#;
    std::fs::write(&local, bytes).unwrap();

    let general = std::env::temp_dir().join("aionui").join("general");
    std::fs::create_dir_all(&general).unwrap();
    let uploads = tempfile::tempdir_in(&general).unwrap();
    let uploaded = uploads.path().join("valid.txt");
    std::fs::write(&uploaded, b"general upload").unwrap();
    let escaped = uploads.path().join(name);
    symlink(&local, &escaped).unwrap();
    let missing = external.path().join(format!("missing-{name}"));
    let unrepresentable = external.path().join(format!("one\n{name}"));
    std::fs::write(&unrepresentable, bytes).unwrap();

    for (kind, path) in [
        ("local", missing.as_path()),
        ("local", Path::new(name)),
        ("local", external.path()),
        ("local", unrepresentable.as_path()),
        ("upload", local.as_path()),
        ("upload", escaped.as_path()),
        ("upload", uploads.path()),
    ] {
        let file = json!({"kind":kind,"path":path});
        for (request, status, error) in [
            (
                post(&metadata).json(&json!({"file":file})),
                StatusCode::NOT_FOUND,
                "FILE_NOT_FOUND",
            ),
            (
                post(&messages).json(&json!({"content":content,"files":[file]})),
                StatusCode::BAD_REQUEST,
                "BAD_REQUEST",
            ),
        ] {
            let body = read(request.send().await.unwrap(), status).await;
            assert_eq!(body["code"], error);
            assert!(!body.to_string().contains("h2-path-"));
            assert!(!body.to_string().contains("h2-prompt-"));
        }
    }

    let too_many = vec![json!({"kind":"local","path":local}); 101];
    let body = read(
        post(&messages)
            .json(&json!({"content":content,"files":too_many}))
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(body["code"], "BAD_REQUEST");
    assert_eq!(body["error"], "Could not resolve a message attachment.");

    let local_ref = json!({"kind":"local","path":local});
    let upload_ref = json!({"kind":"upload","path":uploaded});
    for (reference, path, length) in [(&local_ref, &local, bytes.len()), (&upload_ref, &uploaded, 14)] {
        let body = read(
            post(&metadata).json(&json!({"file":reference})).send().await.unwrap(),
            StatusCode::OK,
        )
        .await;
        assert_eq!(body["data"]["path"], json!(std::fs::canonicalize(path).unwrap()));
        assert_eq!(body["data"]["size"], length);
    }

    let prompt_path = base.join("relay-calls.attachment-prompt");
    assert!(!prompt_path.exists(), "no invalid request may reach the agent");
    read(
        post(&messages)
            .json(&json!({"content":content,"files":[local_ref,upload_ref]}))
            .send()
            .await
            .unwrap(),
        StatusCode::ACCEPTED,
    )
    .await;
    let prompt = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(prompt) = std::fs::read(&prompt_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                break prompt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let text = prompt[0]["text"].as_str().unwrap();
    assert!(text.contains(content));
    assert!(text.contains(std::fs::canonicalize(local).unwrap().to_str().unwrap()));
    assert!(text.contains(std::fs::canonicalize(uploaded).unwrap().to_str().unwrap()));
    assert_eq!(prompt[1]["type"], "image");
    assert_eq!(prompt[1]["data"], encoded);
    host.tasks.kill_and_wait(&id, None).await;

    // Trigger the actual file-service warning after admission, on spawn_blocking.
    use aionui_file::IFileService;
    let file_service =
        aionui_file::FileService::new(std::sync::Arc::new(aionui_realtime::BroadcastEventBus::new(1)), vec![]);
    let leaf = external.path().join(name);
    let stale = aionui_common::chat_attachment::ChatAttachment::open(&leaf, None).unwrap();
    std::fs::rename(leaf, external.path().join("removed")).unwrap();
    assert!(matches!(
        file_service.attachment_metadata(stale).await,
        Err(aionui_file::FileError::TargetNotFound)
    ));

    let distrusted = external.path().join("h2-path-insecure-root");
    std::fs::create_dir(&distrusted).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&distrusted, std::fs::Permissions::from_mode(0o777)).unwrap();
    tokio::task::spawn_blocking(move || {
        assert!(matches!(
            aionui_common::chat_attachment::ChatAttachment::open(&distrusted.join(name), Some(&distrusted)),
            Err(aionui_common::chat_attachment::AttachmentError::UntrustedRoot)
        ));
    })
    .await
    .unwrap();
    let logs = trace.text();
    for signal in [
        "ATTACHMENT_OPENED",
        "UPLOAD_ROOT_UNTRUSTED",
        "resolved attachment changed or is unavailable",
        "could not resolve message attachments",
        "could not resolve chat file reference",
        "ACP prompt carries native media content blocks",
    ] {
        assert!(logs.contains(signal), "missing production signal: {signal}");
    }
    assert!(!logs.contains("h2-path-insecure-root"));
    let control_bytes = encoded.clone();
    let test_thread = std::thread::current().id();
    tokio::task::spawn_blocking(move || {
        assert_ne!(std::thread::current().id(), test_thread);
        tracing::trace!(path = %name, content = %content, bytes = %std::str::from_utf8(bytes).unwrap(), encoded = %control_bytes);
        tracing::trace!(path = ?name, content = ?content);
    }).await.unwrap();
    let positive = trace.text();
    for marker in [
        name.to_owned(),
        format!("{name:?}").trim_matches('"').to_owned(),
        content.to_owned(),
        format!("{content:?}").trim_matches('"').to_owned(),
        std::str::from_utf8(bytes).unwrap().to_owned(),
        encoded,
    ] {
        assert!(positive.contains(&marker), "positive control did not emit the marker");
        assert!(
            !logs.contains(&marker),
            "production diagnostic leaked an attachment marker"
        );
    }
}
