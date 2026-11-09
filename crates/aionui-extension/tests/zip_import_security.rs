//! Exercise archive input through the public import service after the zip upgrade.

#[path = "zip_import_security/review_regressions.rs"]
mod review_regressions;

use std::io::{self, Read, Write};
use std::path::Path;

use aionui_db::{ISkillRepository, SqliteSkillRepository, init_database_memory};
use aionui_extension::ExtensionError;
use aionui_extension::skill_service::{SkillPaths, import_skills, import_skills_with_repo, skill_import_limits};
use tempfile::TempDir;
use zip::write::SimpleFileOptions;

const MIB: u64 = 1024 * 1024;
const MANIFEST: &str = "---\nname: archive-skill\ndescription: Archive regression\n---\nBody";

fn paths(base: &Path) -> SkillPaths {
    SkillPaths {
        data_dir: base.to_path_buf(),
        user_skills_dir: base.join("skills"),
        cron_skills_dir: base.join("cron-skills"),
        builtin_skills_dir: base.join("builtin-skills"),
        builtin_rules_dir: base.join("builtin-rules"),
        assistant_rules_dir: base.join("assistant-rules"),
        assistant_skills_dir: base.join("assistant-skills"),
    }
}

fn write_archive(path: &Path, payload_sizes: &[u64]) {
    let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(1));
    writer.start_file("archive-skill/SKILL.md", options).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    for (index, size) in payload_sizes.iter().enumerate() {
        write_payload(&mut writer, &format!("archive-skill/data-{index}.bin"), *size);
    }
    writer.finish().unwrap();
}

fn write_payload(writer: &mut zip::ZipWriter<std::fs::File>, name: &str, size: u64) {
    writer.start_file(name, payload_options()).unwrap();
    // Size-boundary fixtures must stay below the newly accepted 200:1 ratio
    // limit, so size rejection is exercised independently of ratio rejection.
    let mut block = [0_u8; 4096];
    let mut state = 1_u32;
    for byte in &mut block {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        *byte = state as u8;
    }
    let mut remaining = size;
    while remaining > 0 {
        let count = remaining.min(block.len() as u64) as usize;
        writer.write_all(&block[..count]).unwrap();
        remaining -= count as u64;
    }
}

fn payload_options() -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6))
}

fn assert_size_fixture(path: &Path) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    for index in 0..archive.len() {
        let entry = archive.by_index(index).unwrap();
        assert!(entry.size() <= entry.compressed_size() * 200);
    }
    let compressed_bytes = std::fs::metadata(path).unwrap().len();
    assert!(compressed_bytes < 4 * MIB, "fixture bytes: {compressed_bytes}");
}

fn assert_clean(paths: &SkillPaths) {
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
    let users = paths.user_skills_dir.join("users");
    if users.exists() {
        for user in std::fs::read_dir(users).unwrap() {
            assert_eq!(std::fs::read_dir(user.unwrap().path()).unwrap().count(), 0);
        }
    }
}

#[tokio::test]
async fn compressed_zip_preserves_file_limit_boundary() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("boundary.zip");
    assert_eq!(skill_import_limits().max_file_bytes, 50 * MIB);
    write_archive(&archive, &[50 * MIB]);
    assert_size_fixture(&archive);

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
    let user = std::fs::read_dir(paths.user_skills_dir.join("users"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let imported = user.path().join("archive-skill");
    assert_eq!(std::fs::read_to_string(imported.join("SKILL.md")).unwrap(), MANIFEST);
    assert_eq!(std::fs::metadata(imported.join("data-0.bin")).unwrap().len(), 50 * MIB);
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn compressed_zip_rejects_file_over_50_mib_without_catalog_or_staging_residue() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("oversized.zip");
    write_archive(&archive, &[50 * MIB + 1]);
    assert_size_fixture(&archive);
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());

    let outcome = import_skills_with_repo(&paths, &repo, &archive).await.unwrap();

    assert!(outcome.imported.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].code, "SKILL_IMPORT_FILE_TOO_LARGE");
    assert_eq!(outcome.failed[0].actual_bytes, Some((50 * MIB + 1) as i64));
    assert_eq!(outcome.failed[0].limit_bytes, Some((50 * MIB) as i64));
    assert_eq!(outcome.failed[0].error_path.as_deref(), Some("data-0.bin"));
    assert!(repo.find_by_name("archive-skill").await.unwrap().is_none());
    assert_clean(&paths);
}

#[tokio::test]
async fn compressed_zip_preserves_200_mib_total_limit() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("total.zip");
    assert_eq!(skill_import_limits().max_total_bytes, 200 * MIB);
    // Four individually allowed files plus the manifest exceed the skill budget.
    write_archive(&archive, &[50 * MIB; 4]);
    assert_size_fixture(&archive);

    let error = import_skills(&paths, &archive).await.unwrap_err();

    assert!(matches!(error, ExtensionError::SkillImportZipTooLarge {
        total_bytes, limit_bytes
    } if total_bytes == 200 * MIB + 1 && limit_bytes == 200 * MIB));
    assert_clean(&paths);
}

#[tokio::test]
async fn compressed_zip_accepts_exactly_200_mib_total() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("total-boundary.zip");
    write_archive(
        &archive,
        &[50 * MIB, 50 * MIB, 50 * MIB, 50 * MIB - MANIFEST.len() as u64],
    );
    assert_size_fixture(&archive);

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn zip_rejects_combined_skills_over_200_mib_before_installing_any() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("multiple-skills.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    // Each skill is only 100 MiB plus a manifest. The ZIP-wide sum must fail.
    for name in ["first", "second"] {
        writer
            .start_file(format!("{name}/SKILL.md"), payload_options())
            .unwrap();
        writer
            .write_all(MANIFEST.replace("archive-skill", name).as_bytes())
            .unwrap();
        for index in 0..2 {
            write_payload(&mut writer, &format!("{name}/data-{index}.bin"), 50 * MIB);
        }
    }
    writer.finish().unwrap();
    assert_size_fixture(&archive);
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());

    let error = import_skills_with_repo(&paths, &repo, &archive).await.unwrap_err();

    assert!(matches!(error, ExtensionError::SkillImportZipTooLarge {
        total_bytes, limit_bytes
    } if total_bytes == 200 * MIB + 1 && limit_bytes == 200 * MIB));
    for name in ["first", "second"] {
        assert!(repo.find_by_name(name).await.unwrap().is_none());
    }
    assert_clean(&paths);
}

fn forge_uncompressed_size(path: &Path, index: usize, size: u32) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let entry = archive.by_index(index).unwrap();
    let local = entry.header_start() as usize;
    let central = entry.central_header_start() as usize;
    let mut bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[local..local + 4], b"PK\x03\x04");
    assert_eq!(&bytes[central..central + 4], b"PK\x01\x02");
    bytes[local + 22..local + 26].copy_from_slice(&size.to_le_bytes());
    bytes[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn zip_cannot_bypass_actual_size_budget_with_forged_header() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("forged.zip");
    write_archive(&archive, &[50 * MIB + 1]);
    // The ZIP decoder accepts reading up to this false high declaration; our
    // independent budget must stop at the actual 50 MiB + 1 byte instead.
    forge_uncompressed_size(&archive, 1, u32::MAX - 1);
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    assert_eq!(reader.by_index(1).unwrap().size(), u64::from(u32::MAX - 1));

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert!(outcome.imported.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].code, "SKILL_IMPORT_FILE_TOO_LARGE");
    assert_eq!(outcome.failed[0].actual_bytes, Some((50 * MIB + 1) as i64));
    assert_eq!(outcome.failed[0].limit_bytes, Some((50 * MIB) as i64));
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_rejects_undersized_header_and_cleans_partial_extraction() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("undersized-header.zip");
    write_archive(&archive, &[50 * MIB + 1]);
    forge_uncompressed_size(&archive, 1, 16);
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    assert_eq!(reader.by_index(1).unwrap().size(), 16);

    let error = import_skills(&paths, &archive).await.unwrap_err();

    // zip 9's decoder independently rejects data larger than the declaration.
    assert!(matches!(error, ExtensionError::SkillImportInvalidZip(ref message)
        if message.contains("larger than its declared uncompressed size")));
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_rejects_more_than_4096_entries() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("entries.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for index in 0..4097 {
        writer.add_directory(format!("dir-{index}"), payload_options()).unwrap();
    }
    writer.finish().unwrap();
    assert!(std::fs::metadata(&archive).unwrap().len() < MIB);

    let error = import_skills(&paths, &archive).await.unwrap_err();

    assert!(matches!(
        error,
        ExtensionError::SkillImportZipTooManyEntries {
            entry_count: 4097,
            limit_entries: 4096
        }
    ));
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_counts_duplicate_names_before_extraction() {
    for directory_count in [2, 4096] {
        let tmp = TempDir::new().unwrap();
        let paths = paths(tmp.path());
        let archive_path = tmp.path().join("duplicates.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive_path).unwrap());
        writer.start_file("SKILL.md", payload_options()).unwrap();
        writer.write_all(MANIFEST.as_bytes()).unwrap();
        for index in 0..directory_count {
            writer
                .add_directory(format!("dir-{index:04}"), payload_options())
                .unwrap();
        }
        writer.finish().unwrap();
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&archive_path).unwrap()).unwrap();
        let mut bytes = std::fs::read(&archive_path).unwrap();
        // The writer prohibits duplicate names. Alias same-length local and
        // central names in the generated fixture to exercise the reader's dedup.
        for index in 1..archive.len() {
            let entry = archive.by_index(index).unwrap();
            assert_eq!(entry.name().unwrap().len(), 9);
            let local = entry.header_start() as usize + 30;
            let central = entry.central_header_start() as usize + 46;
            bytes[local..local + 9].copy_from_slice(b"same-dir/");
            bytes[central..central + 9].copy_from_slice(b"same-dir/");
        }
        std::fs::write(&archive_path, bytes).unwrap();
        let archive = zip::ZipArchive::new(std::fs::File::open(&archive_path).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        assert!(archive.has_duplicate_names());

        let result = import_skills(&paths, &archive_path).await;

        if directory_count == 2 {
            let outcome = result.unwrap();
            assert_eq!(outcome.imported, ["archive-skill"]);
            assert!(outcome.failed.is_empty());
            assert!(!paths.user_skills_dir.join(".import-tmp").exists());
        } else {
            assert!(matches!(
                result,
                Err(ExtensionError::SkillImportZipTooManyEntries {
                    entry_count: 4097,
                    limit_entries: 4096
                })
            ));
            assert_clean(&paths);
        }
    }
}

#[tokio::test]
async fn zip_accepts_exactly_4096_entries_and_output_paths() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("entry-boundary.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer.start_file("SKILL.md", payload_options()).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    for index in 0..4095 {
        writer.start_file(format!("empty-{index}"), payload_options()).unwrap();
    }
    writer.finish().unwrap();

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn zip_counts_implicit_directories_against_output_path_limit() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("implicit-directories.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for index in 0..2048 {
        writer
            .start_file(format!("root/dir-{index}/empty"), payload_options())
            .unwrap();
    }
    writer.finish().unwrap();

    let error = import_skills(&paths, &archive).await.unwrap_err();

    assert!(matches!(
        error,
        ExtensionError::SkillImportZipTooManyEntries {
            entry_count: 4097,
            limit_entries: 4096
        }
    ));
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_rejects_high_compression_ratio_and_cleans_partial_extraction() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("ratio.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer.start_file("archive-skill/SKILL.md", payload_options()).unwrap();
    writer.write_all(MANIFEST.as_bytes()).unwrap();
    writer
        .start_file(
            "archive-skill/bomb",
            SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .compression_level(Some(9)),
        )
        .unwrap();
    io::copy(&mut io::repeat(0).take(4 * MIB), &mut writer).unwrap();
    writer.finish().unwrap();
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    let entry = reader.by_index(1).unwrap();
    let compressed_bytes = entry.compressed_size();
    assert!(entry.size() > compressed_bytes * 200);
    assert!(std::fs::metadata(&archive).unwrap().len() < 16 * 1024);
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());

    let error = import_skills_with_repo(&paths, &repo, &archive).await.unwrap_err();

    assert!(matches!(error, ExtensionError::SkillImportZipCompressionRatio {
        expanded_bytes, compressed_bytes: actual_compressed, limit_ratio: 200, ..
    } if actual_compressed == compressed_bytes && expanded_bytes == compressed_bytes * 200 + 1));
    assert!(repo.find_by_name("archive-skill").await.unwrap().is_none());
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_limit_errors_keep_specific_http_codes_and_details() {
    use aionui_common::ApiError;
    use axum::http::StatusCode;
    let cases = [
        (
            ExtensionError::SkillImportZipTooLarge {
                total_bytes: 200 * MIB + 1,
                limit_bytes: 200 * MIB,
            },
            "SKILL_IMPORT_ZIP_TOO_LARGE",
            "limit_bytes",
            200 * MIB,
        ),
        (
            ExtensionError::SkillImportZipTooManyEntries {
                entry_count: 4097,
                limit_entries: 4096,
            },
            "SKILL_IMPORT_ZIP_TOO_MANY_ENTRIES",
            "limit_entries",
            4096,
        ),
        (
            ExtensionError::SkillImportZipCompressionRatio {
                file_path: "data".into(),
                expanded_bytes: 201,
                compressed_bytes: 1,
                limit_ratio: 200,
            },
            "SKILL_IMPORT_ZIP_COMPRESSION_RATIO",
            "limit_ratio",
            200,
        ),
    ];
    for (error, code, field, value) in cases {
        let api = ApiError::from(error);
        assert_eq!(api.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(api.error_code(), code);
        assert_eq!(api.error_details().unwrap()[field], value);
    }
}

#[tokio::test]
async fn bundled_skill_zip_preserves_every_file() {
    fn collect_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_files(&path, files);
            } else {
                files.push(path);
            }
        }
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../aionui-app/assets/builtin-skills/morph-ppt");
    let mut files = Vec::new();
    collect_files(&source, &mut files);
    files.sort();
    assert_eq!(files.len(), 111);
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("morph-ppt.zip");
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    for file in &files {
        let name = file
            .strip_prefix(source.parent().unwrap())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        writer.start_file(name, payload_options()).unwrap();
        writer.write_all(&std::fs::read(file).unwrap()).unwrap();
    }
    writer.finish().unwrap();
    assert_size_fixture(&archive);

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["morph-ppt"]);
    assert!(outcome.failed.is_empty());
    let user = std::fs::read_dir(paths.user_skills_dir.join("users"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let imported = user.path().join("morph-ppt");
    for file in files {
        assert_eq!(
            std::fs::read(imported.join(file.strip_prefix(&source).unwrap())).unwrap(),
            std::fs::read(file).unwrap()
        );
    }
    assert!(!paths.user_skills_dir.join(".import-tmp").exists());
}

#[tokio::test]
async fn zip_compression_backends_preserve_skill_contents() {
    for method in [zip::CompressionMethod::Bzip2, zip::CompressionMethod::Zstd] {
        let tmp = TempDir::new().unwrap();
        let paths = paths(tmp.path());
        let archive = tmp.path().join("compressed.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        writer
            .start_file(
                "archive-skill/SKILL.md",
                SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
        writer.write_all(MANIFEST.as_bytes()).unwrap();
        writer.finish().unwrap();
        let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
        assert_eq!(reader.by_index(0).unwrap().compression(), method);

        let outcome = import_skills(&paths, &archive).await.unwrap();

        assert_eq!(outcome.imported, ["archive-skill"], "{method:?}");
        assert!(outcome.failed.is_empty(), "{method:?}");
        let user = std::fs::read_dir(paths.user_skills_dir.join("users"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(user.path().join("archive-skill/SKILL.md")).unwrap(),
            MANIFEST
        );
    }
}

#[tokio::test]
async fn zip_rejects_symlinks_and_cleans_extraction() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("symlink.zip");
    let outside = tmp.path().join("outside.txt");
    std::fs::write(&outside, "unchanged").unwrap();
    let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
    writer
        .add_symlink("archive-skill/link", "../../outside.txt", SimpleFileOptions::default())
        .unwrap();
    writer.finish().unwrap();

    let result = import_skills(&paths, &archive).await;

    assert!(matches!(result, Err(ExtensionError::SkillImportSymlinkEntry(ref name)) if name == "archive-skill/link"));
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "unchanged");
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_rejects_symlink_with_huge_declared_size_without_allocation() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    let archive = tmp.path().join("huge-symlink.zip");
    std::fs::write(&archive, include_bytes!("fixtures/huge-symlink.zip")).unwrap();
    // Confirm that this fixture exercises the malicious metadata, not a normal link.
    let mut reader = zip::ZipArchive::new(std::fs::File::open(&archive).unwrap()).unwrap();
    let entry = reader.by_index(0).unwrap();
    assert_eq!(entry.size(), 1 << 62);
    assert!(entry.is_symlink());

    let result = import_skills(&paths, &archive).await;

    assert!(matches!(result, Err(ExtensionError::SkillImportSymlinkEntry(ref name)) if name == "l"));
    assert_clean(&paths);
}

#[tokio::test]
async fn zip_rejects_parent_absolute_and_backslash_paths() {
    let tmp = TempDir::new().unwrap();
    let paths = paths(tmp.path());
    for name in [
        "../escape.txt",
        "nested/../../escape.txt",
        "/escape.txt",
        "..\\escape.txt",
    ] {
        let archive = tmp.path().join("traversal.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        writer.start_file(name, SimpleFileOptions::default()).unwrap();
        writer.write_all(b"outside").unwrap();
        writer.finish().unwrap();

        let result = import_skills(&paths, &archive).await;

        assert!(
            matches!(result, Err(ExtensionError::PathTraversal(ref entry)) if entry == name),
            "{name}: {result:?}"
        );
        assert!(!tmp.path().join("escape.txt").exists());
        assert_clean(&paths);
    }
}
