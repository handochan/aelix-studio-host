use super::*;

fn member_offsets(path: &Path, index: usize) -> (usize, usize, u64) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let entry = archive.by_index_raw(index).unwrap();
    (
        entry.header_start() as usize,
        entry.central_header_start() as usize,
        entry.data_start().unwrap(),
    )
}

fn set_compressed_size(path: &Path, index: usize, size: u32) {
    let (local, central, _) = member_offsets(path, index);
    let mut bytes = std::fs::read(path).unwrap();
    bytes[local + 18..local + 22].copy_from_slice(&size.to_le_bytes());
    bytes[central + 20..central + 24].copy_from_slice(&size.to_le_bytes());
    std::fs::write(path, bytes).unwrap();
    let archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    assert_eq!(archive.by_index_data(index).unwrap().compressed_size(), u64::from(size));
}

fn layout_fixture(path: &Path) {
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    writer.start_file("archive-skill/SKILL.md", payload_options()).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    writer
        .start_file("archive-skill/bomb", payload_options().compression_level(Some(9)))
        .unwrap();
    io::copy(&mut io::repeat(0).take(4 * MIB), &mut writer).unwrap();
    writer
        .start_file(
            "archive-skill/padding",
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored),
        )
        .unwrap();
    writer.write_all(&[7; 64 * 1024]).unwrap();
    writer.finish().unwrap();
}

#[tokio::test]
async fn inflated_compressed_size_cannot_overlap_the_next_header_and_data() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("inflated.zip");
    layout_fixture(&archive);
    let (_, _, bomb_start) = member_offsets(&archive, 1);
    let (next_header, _, next_data) = member_offsets(&archive, 2);
    assert!(bomb_start < next_header as u64);
    let inflated_size = (next_data + 32 * 1024 - bomb_start) as u32;
    // The false denominator would allow the 4 MiB bomb under 200:1.
    assert!(4 * MIB < u64::from(inflated_size) * 200);
    set_compressed_size(&archive, 1, inflated_size);
    assert!(std::fs::metadata(&archive).unwrap().len() < 80 * 1024);

    let error = import_skills(&paths, &archive).await.unwrap_err();

    assert!(matches!(error, ExtensionError::SkillImportInvalidZip(ref message)
        if message.contains("overlaps another local header or member")));
    assert_clean(&paths);
}

#[tokio::test]
async fn multiple_central_entries_cannot_share_the_same_local_header_and_data() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("shared-data.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for name in ["first/SKILL.md", "second/SKILL.md"] {
        writer.start_file(name, payload_options()).unwrap();
        writer.write_all(MANIFEST.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
    let (first_header, _, first_data) = member_offsets(&archive, 0);
    let (_, second_central, _) = member_offsets(&archive, 1);
    let mut bytes = std::fs::read(&archive).unwrap();
    bytes[second_central + 42..second_central + 46].copy_from_slice(&(first_header as u32).to_le_bytes());
    std::fs::write(&archive, bytes).unwrap();
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    assert_eq!(reader.len(), 2);
    assert_eq!(reader.by_index_raw(1).unwrap().data_start(), Some(first_data));

    let error = import_skills(&paths, &archive).await.unwrap_err();

    assert!(matches!(error, ExtensionError::SkillImportInvalidZip(ref message)
        if message.contains("overlaps another local header or member")));
    assert_clean(&paths);
}

#[tokio::test]
async fn compressed_ranges_must_end_before_the_central_directory_and_file_end() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    for beyond_file in [false, true] {
        let archive = tmp.path().join("out-of-bounds.zip");
        layout_fixture(&archive);
        let (_, _, data_start) = member_offsets(&archive, 2);
        let reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
        let end = if beyond_file {
            std::fs::metadata(&archive).unwrap().len() + 1
        } else {
            reader.central_directory_start() + 1
        };
        set_compressed_size(&archive, 2, (end - data_start) as u32);

        let error = import_skills(&paths, &archive).await.unwrap_err();

        assert!(matches!(error, ExtensionError::SkillImportInvalidZip(ref message)
            if message.contains("compressed member range exceeds the archive data region")));
        assert_clean(&paths);
    }
}

#[tokio::test]
async fn valid_zip64_members_with_a_prepended_stub_remain_supported() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("zip64.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer
        .start_file("archive-skill/SKILL.md", payload_options().large_file(true))
        .unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    writer.finish().unwrap();
    let bytes = std::fs::read(&archive).unwrap();
    let mut prefixed = b"zip stub".to_vec();
    prefixed.extend_from_slice(&bytes);
    std::fs::write(&archive, prefixed).unwrap();
    let reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    assert_eq!(reader.offset(), 8);

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn skipped_oversized_files_never_refund_the_zip_expansion_budget() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("skipped-budget.zip");
    write_archive(&archive, &[50 * MIB + 1; 4]);
    assert_size_fixture(&archive);

    let error = import_skills(&paths, &archive).await.unwrap_err();

    assert!(
        matches!(error, ExtensionError::SkillImportZipTooLarge { total_bytes, limit_bytes }
        if total_bytes == 200 * MIB + 1 && limit_bytes == 200 * MIB)
    );
    assert_clean(&paths);
}

#[tokio::test]
async fn oversized_manifest_is_reported_as_a_skill_failure_after_partial_file_removal() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("large-manifest.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer.start_file("archive-skill/SKILL.md", payload_options()).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    let mut state = 1_u32;
    let mut block = [0_u8; 4096];
    for byte in &mut block {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = 32 + (state % 95) as u8;
    }
    let mut remaining = 50 * MIB + 1 - MANIFEST.len() as u64;
    while remaining > 0 {
        let count = remaining.min(block.len() as u64) as usize;
        writer.write_all(&block[..count]).unwrap();
        remaining -= count as u64;
    }
    writer.finish().unwrap();
    assert_size_fixture(&archive);

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert!(outcome.imported.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].source_name, "archive-skill");
    assert_eq!(outcome.failed[0].code, "SKILL_IMPORT_FILE_TOO_LARGE");
    assert_eq!(outcome.failed[0].actual_bytes, Some((50 * MIB + 1) as i64));
    assert_eq!(outcome.failed[0].limit_bytes, Some((50 * MIB) as i64));
    assert_eq!(outcome.failed[0].error_path.as_deref(), Some("SKILL.md"));
    assert_clean(&paths);
}

#[tokio::test]
async fn http_200_preserves_per_skill_failure_fields_and_imports_the_other_skill() {
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
    let archive = tmp.path().join("mixed.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer.start_file("bad-skill/SKILL.md", payload_options()).unwrap();
    writer
        .write_all(MANIFEST.replace("archive-skill", "bad-skill").as_bytes())
        .unwrap();
    write_payload(&mut writer, "bad-skill/data.bin", 51 * MIB);
    // This member follows the rejected file, proving extraction continues.
    writer.start_file("good-skill/SKILL.md", payload_options()).unwrap();
    writer
        .write_all(MANIFEST.replace("archive-skill", "good-skill").as_bytes())
        .unwrap();
    writer.finish().unwrap();
    assert_size_fixture(&archive);
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteSkillRepository::new(db.pool().clone()));
    let router = skill_routes(SkillRouterState {
        skill_paths: paths.clone(),
        skill_repo: repo.clone(),
        external_paths_manager: Arc::new(ExternalPathsManager::with_file(tmp.path().join("paths.json")).await),
        assistant_dispatcher: None,
    })
    .layer(Extension(CurrentUser {
        id: "system_default_user".into(),
        username: "system_default_user".into(),
        user_type: UserType::Local,
        status: UserStatus::Active,
    }));
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
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["skill_names"], serde_json::json!(["good-skill"]));
    assert_eq!(json["data"]["failed"].as_array().unwrap().len(), 1);
    let failure = &json["data"]["failed"][0];
    assert_eq!(failure["source_name"], "bad-skill");
    assert_eq!(failure["code"], "SKILL_IMPORT_FILE_TOO_LARGE");
    assert_eq!(failure["actual_bytes"], 50 * MIB + 1);
    assert_eq!(failure["limit_bytes"], 50 * MIB);
    assert_eq!(failure["error_path"], "data.bin");
    assert_eq!(failure["details"]["actual_bytes_is_lower_bound"], true);
    assert!(
        repo.find_by_name_for_user("system_default_user", "good-skill")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        repo.find_by_name_for_user("system_default_user", "bad-skill")
            .await
            .unwrap()
            .is_none()
    );
    let records = repo
        .list_import_records_for_user("system_default_user", 10)
        .await
        .unwrap();
    assert_eq!(records.len(), 2);
    let failed = records.iter().find(|record| record.status == "failed").unwrap();
    assert_eq!(failed.error_code.as_deref(), Some("SKILL_IMPORT_FILE_TOO_LARGE"));
    assert_eq!(failed.actual_bytes, Some((50 * MIB + 1) as i64));
    assert_eq!(failed.limit_bytes, Some((50 * MIB) as i64));
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn recursive_scans_disk_lists_and_backfill_skip_import_temporary_trees() {
    use aionui_extension::skill_service::{
        list_available_skills, scan_for_skills_with_paths, sync_skill_catalog_into_repo,
    };
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let hidden = paths.user_skills_dir.join(".import-tmp/leased-abandoned/private");
    let visible = paths.user_skills_dir.join("visible");
    for (directory, name) in [(&hidden, "hidden"), (&visible, "visible")] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(directory.join("SKILL.md"), MANIFEST.replace("archive-skill", name)).unwrap();
    }
    let ended = owner_fixture::ended_pid().await;
    owner_fixture::write_owner(&paths.user_skills_dir.join(".import-tmp"), "container", ended);
    owner_fixture::write_owner(
        &paths.user_skills_dir.join(".import-tmp/leased-abandoned"),
        "zip",
        ended,
    );
    assert_eq!(
        scan_for_skills_with_paths(&paths, &paths.user_skills_dir)
            .await
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["visible"]
    );
    assert!(scan_for_skills_with_paths(&paths, &hidden).await.unwrap().is_empty());
    assert_eq!(
        list_available_skills(&paths)
            .await
            .unwrap()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["visible"]
    );
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());
    sync_skill_catalog_into_repo(&paths, &repo).await.unwrap();
    assert!(repo.find_by_name("hidden").await.unwrap().is_none());
    assert!(repo.find_by_name("visible").await.unwrap().is_some());
    assert!(hidden.join("SKILL.md").exists());
}

#[tokio::test]
async fn database_lists_hide_old_staging_rows_before_builtin_precedence() {
    use aionui_db::UpsertSkillParams;
    use aionui_extension::skill_service::{
        SkillSource, list_available_skills_with_repo, list_available_skills_with_repo_for_user,
    };
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let builtin = paths.builtin_skills_dir.join("shared");
    std::fs::create_dir_all(&builtin).unwrap();
    std::fs::write(builtin.join("SKILL.md"), MANIFEST.replace("archive-skill", "shared")).unwrap();
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());
    repo.upsert_global(UpsertSkillParams {
        name: "shared",
        description: Some("valid builtin"),
        path: builtin.to_str().unwrap(),
        source: "builtin",
        enabled: true,
    })
    .await
    .unwrap();
    let ended = owner_fixture::ended_pid().await;
    owner_fixture::write_owner(&paths.user_skills_dir.join(".import-tmp"), "container", ended);
    owner_fixture::write_owner(&paths.user_skills_dir.join(".import-tmp/skills-123-0"), "zip", ended);
    for name in ["shared", "hidden-only"] {
        let abandoned = paths.user_skills_dir.join(".import-tmp/skills-123-0").join(name);
        // The file has already been removed by startup cleanup; the old row remains.
        assert!(!abandoned.exists());
        repo.upsert_for_user(
            "system_default_user",
            UpsertSkillParams {
                name,
                description: Some("stale staging"),
                path: abandoned.to_str().unwrap(),
                source: "user",
                enabled: true,
            },
        )
        .await
        .unwrap();
    }
    for listed in [
        list_available_skills_with_repo(&paths, &repo).await.unwrap(),
        list_available_skills_with_repo_for_user(&paths, &repo, "system_default_user")
            .await
            .unwrap(),
    ] {
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "shared");
        assert_eq!(listed[0].source, SkillSource::Builtin);
        assert_eq!(listed[0].location, builtin.join("SKILL.md").to_string_lossy());
    }
    assert!(repo.find_by_name("hidden-only").await.unwrap().is_some());
}
