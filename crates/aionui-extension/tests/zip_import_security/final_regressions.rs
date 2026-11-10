use super::*;

#[tokio::test]
async fn small_repeated_text_is_not_rejected_by_compression_ratio() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("small-repeated.zip");
    let text = "repeat".repeat(5000);
    assert_eq!(text.len(), 30_000);
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer.start_file("archive-skill/SKILL.md", payload_options()).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    writer
        .start_file(
            "archive-skill/repeated.txt",
            payload_options().compression_level(Some(9)),
        )
        .unwrap();
    writer.write_all(text.as_bytes()).unwrap();
    writer.finish().unwrap();
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    let entry = reader.by_index_raw(1).unwrap();
    let ratio = entry.size() as f64 / entry.compressed_size() as f64;
    assert!(ratio > 400.0 && ratio < 500.0, "ratio: {ratio}");

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
    let user = std::fs::read_dir(paths.user_skills_dir.join("users"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(user.path().join("archive-skill/repeated.txt")).unwrap(),
        text
    );
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn ratio_eligibility_uses_actual_output_not_a_large_declared_size() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("declared-large.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer.start_file("archive-skill/SKILL.md", payload_options()).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    writer
        .start_file("archive-skill/small", payload_options().compression_level(Some(9)))
        .unwrap();
    writer.write_all(&[b'a'; 30_000]).unwrap();
    writer.finish().unwrap();
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    let entry = reader.by_index_raw(1).unwrap();
    let local = entry.header_start() as usize;
    let central = entry.central_header_start() as usize;
    let mut bytes = std::fs::read(&archive).unwrap();
    bytes[local + 22..local + 26].copy_from_slice(&(2 * MIB as u32).to_le_bytes());
    bytes[central + 24..central + 28].copy_from_slice(&(2 * MIB as u32).to_le_bytes());
    std::fs::write(&archive, bytes).unwrap();

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn exact_directory_file_sizes_are_not_marked_as_lower_bounds() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let parent = tmp.path().join("source");
    let skill = parent.join("archive-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), MANIFEST).unwrap();
    std::fs::File::create(skill.join("large"))
        .unwrap()
        .set_len(50 * MIB + 9)
        .unwrap();
    let repo = SqliteSkillRepository::new(init_database_memory().await.unwrap().pool().clone());

    let outcome = import_skills_with_repo(&paths, &repo, &parent).await.unwrap();

    assert!(outcome.imported.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].actual_bytes, Some((50 * MIB + 9) as i64));
    assert_eq!(outcome.failed[0].limit_bytes, Some((50 * MIB) as i64));
    assert!(outcome.failed[0].details.is_none());
    assert_clean(&paths);
}

// Build the ordinary ZIP record forms directly, using the library-generated
// compressed payload/CRC as a reference. Local UT metadata has mtime+atime;
// central UT metadata has mtime only. The optional bit-3 descriptor is signed
// (Python streaming shape) or unsigned, while seekable metadata stays inline.
fn ordinary_zip_bytes(descriptor: Option<bool>) -> Vec<u8> {
    let name = b"archive-skill/SKILL.md";
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file(std::str::from_utf8(name).unwrap(), payload_options())
        .unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    let original = writer.finish().unwrap().into_inner();
    let mut reader = zip::ZipArchive::new(std::io::Cursor::new(original.clone())).unwrap();
    let entry = reader.by_index_raw(0).unwrap();
    let central_start = entry.central_header_start() as usize;
    let data_start = entry.data_start().unwrap() as usize;
    let compressed = entry.compressed_size() as usize;
    let crc = entry.crc32();
    let uncompressed = entry.size() as u32;
    let payload = &original[data_start..data_start + compressed];
    let local_extra = [0x55, 0x54, 9, 0, 3, 1, 0, 0, 0, 2, 0, 0, 0];
    let central_extra = [0x55, 0x54, 5, 0, 1, 1, 0, 0, 0];
    let mut local = original[..30].to_vec();
    let mut central = original[central_start..central_start + 46].to_vec();
    local[28..30].copy_from_slice(&(local_extra.len() as u16).to_le_bytes());
    central[30..32].copy_from_slice(&(central_extra.len() as u16).to_le_bytes());
    if descriptor.is_some() {
        let flags = u16::from_le_bytes(local[6..8].try_into().unwrap()) | 8;
        local[6..8].copy_from_slice(&flags.to_le_bytes());
        central[8..10].copy_from_slice(&flags.to_le_bytes());
        local[14..26].fill(0);
    }
    let mut bytes = local;
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&local_extra);
    bytes.extend_from_slice(payload);
    if let Some(signed) = descriptor {
        if signed {
            bytes.extend_from_slice(b"PK\x07\x08");
        }
        bytes.extend_from_slice(&crc.to_le_bytes());
        bytes.extend_from_slice(&(compressed as u32).to_le_bytes());
        bytes.extend_from_slice(&uncompressed.to_le_bytes());
    }
    let new_central_start = bytes.len();
    bytes.extend_from_slice(&central);
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(&central_extra);
    let central_size = bytes.len() - new_central_start;
    let mut end = original[original.len() - 22..].to_vec();
    assert_eq!(&end[..4], b"PK\x05\x06");
    end[12..16].copy_from_slice(&(central_size as u32).to_le_bytes());
    end[16..20].copy_from_slice(&(new_central_start as u32).to_le_bytes());
    bytes.extend_from_slice(&end);
    bytes
}

#[tokio::test]
async fn ordinary_descriptors_and_different_local_central_extras_are_accepted() {
    for descriptor in [None, Some(true), Some(false)] {
        let tmp = TempDir::new().unwrap();
        let paths = paths(tmp.path());
        let archive = tmp.path().join("ordinary.zip");
        let bytes = ordinary_zip_bytes(descriptor);
        assert_eq!(
            u16::from_le_bytes(bytes[6..8].try_into().unwrap()) & 8 != 0,
            descriptor.is_some()
        );
        let central = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone()))
            .unwrap()
            .central_directory_start() as usize;
        assert_eq!(u16::from_le_bytes(bytes[28..30].try_into().unwrap()), 13);
        assert_eq!(
            u16::from_le_bytes(bytes[central + 30..central + 32].try_into().unwrap()),
            9
        );
        std::fs::write(&archive, bytes).unwrap();
        let mut reference = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
        let mut decoded = String::new();
        reference.by_index(0).unwrap().read_to_string(&mut decoded).unwrap();
        assert_eq!(decoded, MANIFEST);

        let outcome = import_skills(&paths, &archive).await.unwrap();

        assert_eq!(outcome.imported, ["archive-skill"], "{descriptor:?}");
        assert!(outcome.failed.is_empty());
        assert!(!paths.user_skills_dir.join(".import-tmp").exists());
    }
}

#[tokio::test]
async fn install_staging_is_hidden_from_scans_disk_lists_and_existing_catalog_rows() {
    use aionui_db::UpsertSkillParams;
    use aionui_extension::skill_service::{
        list_available_skills, list_available_skills_with_repo, scan_for_skills_with_paths,
        sync_skill_catalog_into_repo,
    };
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let staged = paths
        .user_skills_dir
        .join("users/owner/.import-staging-leased-pending.tmp");
    let legacy = paths.user_skills_dir.join(".import-staging-old-skill-123-1");
    let visible = paths.user_skills_dir.join("visible");
    for (directory, name) in [
        (&staged, "hidden-install"),
        (&legacy, "hidden-legacy"),
        (&visible, "visible"),
    ] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), MANIFEST.replace("archive-skill", name)).unwrap();
    }
    let ended = owner_fixture::ended_pid().await;
    owner_fixture::write_owner(&staged, "install", ended);
    owner_fixture::write_owner(&legacy, "install", ended);
    assert_eq!(
        scan_for_skills_with_paths(&paths, &paths.user_skills_dir)
            .await
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["visible"]
    );
    assert!(scan_for_skills_with_paths(&paths, &staged).await.unwrap().is_empty());
    assert!(scan_for_skills_with_paths(&paths, &legacy).await.unwrap().is_empty());
    assert_eq!(
        list_available_skills(&paths)
            .await
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["visible"]
    );
    let repo = SqliteSkillRepository::new(init_database_memory().await.unwrap().pool().clone());
    sync_skill_catalog_into_repo(&paths, &repo).await.unwrap();
    repo.upsert_for_user(
        "system_default_user",
        UpsertSkillParams {
            name: "old-install-row",
            description: None,
            path: staged.to_str().unwrap(),
            source: "user",
            enabled: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        list_available_skills_with_repo(&paths, &repo)
            .await
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["visible"]
    );
    assert!(repo.find_by_name("old-install-row").await.unwrap().is_some());
    assert!(staged.join("SKILL.md").exists());
}

#[tokio::test]
async fn successfully_imported_staging_shaped_skill_is_not_reaped() {
    use aionui_db::UpsertSkillParams;
    use aionui_extension::skill_service::{
        cleanup_skill_import_staging, list_available_skills, list_available_skills_with_repo,
    };
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());
    // 26a4554 accepted these names. Seed the already-installed state; new
    // imports are now intentionally rejected by the user's reservation policy.
    for name in [".import-staging-leased-demo.tmp", ".import-tmp", ".import-tmp-existing"] {
        let installed_dir = paths.user_skills_dir.join("users/old-user").join(name);
        std::fs::create_dir_all(&installed_dir).unwrap();
        let original = MANIFEST.replace("archive-skill", name);
        std::fs::write(installed_dir.join("SKILL.md"), &original).unwrap();
        repo.upsert_for_user(
            "system_default_user",
            UpsertSkillParams {
                name,
                description: Some("existing"),
                path: installed_dir.to_str().unwrap(),
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();
        assert!(
            list_available_skills(&paths)
                .await
                .unwrap()
                .iter()
                .any(|skill| skill.name == name)
        );
        assert!(
            list_available_skills_with_repo(&paths, &repo)
                .await
                .unwrap()
                .iter()
                .any(|skill| skill.name == name)
        );
        assert!(!cleanup_skill_import_staging(&paths).await.unwrap());
        assert_eq!(
            std::fs::read_to_string(installed_dir.join("SKILL.md")).unwrap(),
            original
        );
        assert!(
            list_available_skills(&paths)
                .await
                .unwrap()
                .iter()
                .any(|skill| skill.name == name)
        );
        assert!(
            list_available_skills_with_repo(&paths, &repo)
                .await
                .unwrap()
                .iter()
                .any(|skill| skill.name == name)
        );
    }
}

#[tokio::test]
async fn reserved_names_use_coded_single_errors_and_zip_per_skill_failures() {
    use aionui_auth::CurrentUser;
    use aionui_db::{UserStatus, UserType};
    use aionui_extension::external_paths::ExternalPathsManager;
    use aionui_extension::skill_routes::{SkillRouterState, skill_routes};
    use axum::{
        Extension,
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let db = init_database_memory().await.unwrap();
    let repo = std::sync::Arc::new(SqliteSkillRepository::new(db.pool().clone()));
    let router = skill_routes(SkillRouterState {
        skill_paths: paths.clone(),
        skill_repo: repo,
        external_paths_manager: Arc::new(ExternalPathsManager::with_file(tmp.path().join("paths.json")).await),
        assistant_dispatcher: None,
    })
    .layer(Extension(CurrentUser {
        id: "system_default_user".into(),
        username: "local".into(),
        user_type: UserType::Local,
        status: UserStatus::Active,
    }));
    for (index, (name, prefix)) in [
        (".import-staging-demo", ".import-staging-"),
        (".import-tmp", ".import-tmp"),
        (".import-tmp-custom", ".import-tmp"),
    ]
    .into_iter()
    .enumerate()
    {
        let source = tmp.path().join(format!("source-{index}"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), MANIFEST.replace("archive-skill", name)).unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::post("/api/skills/import")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({"skill_path": source}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let json: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(json["code"], "SKILL_IMPORT_RESERVED_NAME");
        assert_eq!(json["details"]["reason"], "reserved_skill_name");
        assert_eq!(json["details"]["reserved_prefix"], prefix);
        assert_eq!(json["details"]["skill_name"], name);
        assert!(json["error"].as_str().unwrap().contains("reserved"));
        assert!(!paths.user_skills_dir.exists());
    }
    for (index, (content, code)) in [
        ("not valid frontmatter".to_string(), "SKILL_INVALID_FRONTMATTER"),
        (
            MANIFEST.replace("archive-skill", "../escape"),
            "SKILL_IMPORT_INVALID_NAME",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let source = tmp.path().join(format!("existing-error-{index}"));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), content).unwrap();
        let response = router
            .clone()
            .oneshot(
                Request::post("/api/skills/import")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({"skill_path": source}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(json["data"]["failed"][0]["code"], code);
    }
    let archive = tmp.path().join("reserved.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for (directory, name) in [
        ("bad0", ".import-staging-any"),
        ("bad1", ".import-tmp-hidden"),
        ("good", "good"),
    ] {
        writer
            .start_file(format!("{directory}/SKILL.md"), payload_options())
            .unwrap();
        writer
            .write_all(MANIFEST.replace("archive-skill", name).as_bytes())
            .unwrap();
    }
    writer.finish().unwrap();
    let response = router
        .oneshot(
            Request::post("/api/skills/import")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::json!({"skill_path": archive}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(json["data"]["skill_names"], serde_json::json!(["good"]));
    let failed = json["data"]["failed"].as_array().unwrap();
    assert_eq!(failed.len(), 2);
    for (failure, prefix) in failed.iter().zip([".import-staging-", ".import-tmp"]) {
        assert_eq!(failure["code"], "SKILL_IMPORT_RESERVED_NAME");
        assert_eq!(failure["details"]["reason"], "reserved_skill_name");
        assert_eq!(failure["details"]["reserved_prefix"], prefix);
    }
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn source_directory_prefix_is_not_the_imported_skill_name() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let source = tmp.path().join(".import-staging-source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), MANIFEST).unwrap();
    let outcome = import_skills(&paths, &source).await.unwrap();
    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
}

#[tokio::test]
async fn a_live_marker_owner_is_neither_hidden_nor_reclaimed() {
    use aionui_extension::skill_service::{
        cleanup_skill_import_staging, list_available_skills, scan_for_skills_with_paths,
    };
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let stage = paths.user_skills_dir.join("users/owner/.import-staging-active.tmp");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(stage.join("SKILL.md"), MANIFEST.replace("archive-skill", "active")).unwrap();
    owner_fixture::write_owner(&stage, "install", std::process::id());
    assert_eq!(scan_for_skills_with_paths(&paths, &stage).await.unwrap().len(), 1);
    assert!(
        list_available_skills(&paths)
            .await
            .unwrap()
            .iter()
            .any(|skill| skill.name == "active")
    );
    assert!(!cleanup_skill_import_staging(&paths).await.unwrap());
    assert!(stage.join("SKILL.md").exists());
}

#[tokio::test]
async fn payload_metadata_cannot_claim_host_staging_ownership() {
    use aionui_extension::skill_service::{cleanup_skill_import_staging, scan_for_skills_with_paths};
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let fake = paths
        .user_skills_dir
        .join("users/owner/ordinary/.import-staging-payload.tmp");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("SKILL.md"), MANIFEST).unwrap();
    owner_fixture::write_owner(&fake, "install", owner_fixture::ended_pid().await);
    // A copied payload is below a normal installed skill, not a host staging
    // location in the trusted storage root, even with a well-formed sidecar.
    assert_eq!(scan_for_skills_with_paths(&paths, &fake).await.unwrap().len(), 1);
    assert!(!cleanup_skill_import_staging(&paths).await.unwrap());
    assert!(fake.join("SKILL.md").exists());
}
