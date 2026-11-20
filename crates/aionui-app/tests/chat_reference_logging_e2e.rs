//! Exercise reference failures through the authenticated router and access log.

mod common;

use aionui_file::IFileService;
use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

#[tokio::test]
async fn reference_failures_keep_their_http_errors_without_logging_reference_data() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": "Send reference fixture", "extra": {} }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let conversation_id = body_json(response).await["data"]["id"].as_str().unwrap().to_owned();
    let messages_uri = format!("/api/conversations/{conversation_id}/messages");
    let root = tempfile::tempdir().unwrap();
    let absolute = root.path().join("private-absolute-dir/private-absolute-file.txt");
    let absolute = absolute.to_str().unwrap();
    let relative = "../private-relative-dir/private-relative-file.txt";
    let project = services
        .project_service
        .create_standard(
            "system_default_user",
            aionui_project::canonical::to_file_uri(root.path()).unwrap(),
        )
        .await
        .unwrap();

    let logs = tempfile::NamedTempFile::new().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.reopen().unwrap())
        .finish();
    let _capture = tracing::subscriber::set_default(subscriber);
    let content = "private-user-message";
    let files = [
        (
            json!({ "kind": "local", "path": absolute }),
            "local_path_not_readable",
            format!("local file path is not a readable file: {absolute}"),
        ),
        (
            json!({ "kind": "upload", "path": absolute }),
            "chat_file_missing",
            format!("attached file does not exist: {absolute}"),
        ),
        (
            json!({ "kind": "project", "pe_id": project.project_explorer.pe_id, "relative_path": relative }),
            "invalid_relative_path",
            format!("invalid relative path: {relative}"),
        ),
    ];
    for (file, code, message) in files {
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/fs/content/metadata",
                json!({ "file": file }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "success": false, "code": "FILE_NOT_FOUND", "error": "The requested file no longer exists." })
        );
        let response = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                &messages_uri,
                json!({ "content": content, "files": [file] }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "success": false, "code": "BAD_REQUEST", "error": message })
        );
        let output = std::fs::read_to_string(logs.path()).unwrap();
        assert!(
            output
                .lines()
                .any(|line| line.contains("could not resolve chat file reference") && line.contains(code))
        );
        assert!(
            output
                .lines()
                .any(|line| line.contains("could not resolve message attachments") && line.contains(code))
        );
    }

    // Exercise the later metadata failure after a path has already resolved.
    let file_service = aionui_file::FileService::new(services.event_bus.clone(), vec![]);
    let err = file_service.resolved_metadata(absolute.as_ref()).await.unwrap_err();
    assert!(matches!(err, aionui_file::FileError::TargetNotFound));

    let mention_id = "private-mention-id";
    let response = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &messages_uri,
            json!({ "content": content, "sessions": [{ "id": mention_id }] }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await,
        json!({
            "success": false, "code": "NOT_FOUND", "error": format!("Conversation {mention_id} not found")
        })
    );

    let missing_sender = "private-mention-sender";
    let response = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/session-messages/mentionable?current_conversation_id={missing_sender}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(response).await,
        json!({
            "success": false,
            "code": "CONFLICT",
            "error": format!("delivery transport unavailable: sender conversation missing: {missing_sender}")
        })
    );

    let response = app.oneshot(get_with_token(&messages_uri, &token)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["data"]["items"], json!([]));

    let output = std::fs::read_to_string(logs.path()).unwrap();
    assert!(output.contains("resolved metadata target is unreadable"));
    assert!(output.contains("could not resolve session mentions"));
    for code in ["FILE_NOT_FOUND", "BAD_REQUEST", "NOT_FOUND", "CONFLICT"] {
        assert!(
            output
                .lines()
                .any(|line| line.contains("http response") && line.contains(&format!("error_code=\"{code}\"")))
        );
    }
    for value in [
        absolute,
        relative,
        "private-absolute-dir",
        "private-absolute-file.txt",
        "private-relative-dir",
        "private-relative-file.txt",
        content,
        mention_id,
        missing_sender,
    ] {
        assert!(
            !output.contains(value),
            "reference data found in logs: {value}\n{output}"
        );
    }
}
