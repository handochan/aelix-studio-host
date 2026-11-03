//! Exercise archive input through the public import service after the zip upgrade.

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
        writer
            .start_file(format!("archive-skill/data-{index}.bin"), options)
            .unwrap();
        assert_eq!(io::copy(&mut io::repeat(0).take(*size), &mut writer).unwrap(), *size);
    }
    writer.finish().unwrap();
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
    let db = init_database_memory().await.unwrap();
    let repo = SqliteSkillRepository::new(db.pool().clone());

    let outcome = import_skills_with_repo(&paths, &repo, &archive).await.unwrap();

    assert!(outcome.imported.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].code, "SKILL_IMPORT_FILE_TOO_LARGE");
    assert_eq!(outcome.failed[0].actual_bytes, Some((50 * MIB + 1) as i64));
    assert_eq!(outcome.failed[0].limit_bytes, Some((50 * MIB) as i64));
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

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert!(outcome.imported.is_empty());
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].code, "SKILL_IMPORT_TOTAL_TOO_LARGE");
    assert_eq!(
        outcome.failed[0].actual_bytes,
        Some((200 * MIB + MANIFEST.len() as u64) as i64)
    );
    assert_eq!(outcome.failed[0].limit_bytes, Some((200 * MIB) as i64));
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

    let outcome = import_skills(&paths, &archive).await.unwrap();

    assert_eq!(outcome.imported, ["archive-skill"]);
    assert!(outcome.failed.is_empty());
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
