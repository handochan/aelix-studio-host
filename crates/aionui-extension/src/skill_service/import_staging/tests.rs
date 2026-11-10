use super::*;
use std::time::Duration;

#[tokio::test]
async fn cancelling_handler_keeps_worker_owned_then_removes_staging() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let handler = tokio::spawn(extract_with(root.clone(), move |path| {
        fs::write(path.join("SKILL.md"), "partial skill").unwrap();
        entered_tx.send(path.to_path_buf()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(Vec::new())
    }));
    let staging = entered_rx.await.unwrap();
    handler.abort();
    assert!(matches!(handler.await, Err(error) if error.is_cancelled()));
    // Cancellation cannot delete files while the blocking writer still owns them.
    assert!(staging.join("SKILL.md").exists());
    assert!(!cleanup_abandoned(&root).unwrap());
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while root.join(TEMP_DIR_NAME).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(!staging.exists());
}

#[tokio::test]
async fn dropping_extraction_result_removes_staging_after_success() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let extracted = extract_with(root.clone(), |path| {
        fs::write(path.join("SKILL.md"), "complete skill")?;
        Ok(Vec::new())
    })
    .await
    .unwrap();
    let staging = extracted.staging.path().to_path_buf();
    assert!(staging.exists());
    assert!(!cleanup_abandoned(&root).unwrap());
    drop(extracted);
    assert!(!staging.exists());
    assert!(!root.join(TEMP_DIR_NAME).exists());
}

#[tokio::test]
async fn child_hold_import_lease() {
    let Some(root) = std::env::var_os("AELIX_STAGING_TEST_CHILD_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    fs::create_dir_all(&root).unwrap();
    if std::env::var_os("AELIX_STAGING_TEST_PARTIAL_OWNER").is_some() {
        let lease = open_lease(&root).unwrap();
        FileExt::lock_shared(&lease).unwrap();
        let _publish = publication_lock(&root).unwrap();
        let stage = root.join("users/fixture-user/.import-staging-publishing.tmp");
        fs::create_dir_all(&stage).unwrap();
        let _owner = OwnerLease::publish_with(&stage, Kind::Install, || {
            // Fault injection: even payload left by an interrupted publisher
            // must not be inferred to be owned from its directory name.
            fs::write(stage.join("SKILL.md"), "unpublished owner")?;
            fs::write(root.join("owned-path"), stage.to_string_lossy().as_bytes())?;
            fs::write(root.join("child-ready"), "ready")?;
            std::thread::sleep(Duration::from_secs(60));
            Ok(())
        })
        .unwrap();
        return;
    }
    let installation = std::env::var_os("AELIX_STAGING_TEST_CHILD_INSTALL").is_some();
    let leased = std::env::var_os("AELIX_STAGING_TEST_CHILD_LEASED").is_some();
    let user_root = root.join("users/fixture-user");
    let _install_guard = if installation && leased {
        let guard = InstallStaging::create(&root, &user_root).unwrap();
        fs::write(guard.path.join("SKILL.md"), "active install").unwrap();
        Some(guard)
    } else {
        None
    };
    let _guard = if !installation && leased {
        let guard = ZipStaging::create(&root).unwrap();
        fs::write(guard.path().join("SKILL.md"), "active leased import").unwrap();
        Some(guard)
    } else {
        None
    };
    if let Some(path) = _install_guard
        .as_ref()
        .map(|guard| &guard.path)
        .or_else(|| _guard.as_ref().map(|guard| &guard.path))
    {
        fs::write(root.join("owned-path"), path.to_string_lossy().as_bytes()).unwrap();
    }
    // Simulate a still-running old host that predates the shared lease.
    let legacy = if installation {
        user_root.join(format!(".import-staging-leased-legacy-skill-{}-1", std::process::id()))
    } else {
        root.join(TEMP_DIR_NAME)
            .join(format!("skills-{}-1", std::process::id()))
    };
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("SKILL.md"), "active legacy import").unwrap();
    fs::write(root.join("child-ready"), "ready").unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
}

#[tokio::test]
async fn startup_cleanup_preserves_live_processes_and_reaps_hard_exit_leftovers() {
    for leased in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let mut builder = aionui_runtime::Builder::new(std::env::current_exe().unwrap());
        builder
            .args([
                "--exact",
                "skill_service::import_staging::tests::child_hold_import_lease",
                "--nocapture",
            ])
            .env("AELIX_STAGING_TEST_CHILD_ROOT", &root)
            .env_remove("AELIX_STAGING_TEST_CHILD_LEASED")
            .env_remove("AELIX_STAGING_TEST_CHILD_INSTALL")
            .env_remove("AELIX_STAGING_TEST_PARTIAL_OWNER")
            .stdout(std::process::Stdio::null());
        if leased {
            builder.env("AELIX_STAGING_TEST_CHILD_LEASED", "1");
        }
        let mut child = builder.spawn().unwrap();
        let pid = child.id().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.join("child-ready").exists() {
                assert!(child.try_wait().unwrap().is_none(), "child exited before readiness");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let legacy = root.join(TEMP_DIR_NAME).join(format!("skills-{pid}-1"));
        let stale = root.join(TEMP_DIR_NAME).join("leased-abandoned");
        fs::create_dir_all(&stale).unwrap();

        assert!(!cleanup_abandoned(&root).unwrap());
        assert!(legacy.join("SKILL.md").exists());
        assert!(stale.exists());

        child.kill().await.unwrap();
        child.wait().await.unwrap();
        drop(child);
        if leased {
            let owned = PathBuf::from(fs::read_to_string(root.join("owned-path")).unwrap());
            assert!(is_abandoned(&root, &owned));
            assert!(cleanup_abandoned(&root).unwrap());
            assert!(!owned.exists());
            assert!(!ownership::marker_path(&owned).unwrap().exists());
        } else {
            assert!(!cleanup_abandoned(&root).unwrap());
        }
        // A dead PID/name is not proof: unmarked old data stays untouched.
        assert!(legacy.join("SKILL.md").exists());
        assert!(stale.exists());
    }
}

#[test]
fn startup_cleanup_retains_unknown_owners() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let unknown = root.join(TEMP_DIR_NAME).join("unknown-owner");
    fs::create_dir_all(&unknown).unwrap();
    fs::write(unknown.join("SKILL.md"), "unknown ownership").unwrap();
    assert!(!cleanup_abandoned(&root).unwrap());
    assert!(unknown.join("SKILL.md").exists());
}

#[cfg(unix)]
#[test]
fn startup_cleanup_preserves_unmarked_staging_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), "unchanged").unwrap();
    fs::create_dir_all(root.join(TEMP_DIR_NAME)).unwrap();
    std::os::unix::fs::symlink(&outside, root.join(TEMP_DIR_NAME).join("leased-link")).unwrap();
    assert!(!cleanup_abandoned(&root).unwrap());
    assert!(
        root.join(TEMP_DIR_NAME)
            .join("leased-link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "unchanged");
}

#[tokio::test]
async fn cancelled_install_copy_cleans_only_after_the_writer_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let user_root = root.join("users/fixture-user");
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let task_root = root.clone();
    let task_user = user_root.clone();
    let handler = tokio::spawn(async move {
        prepare_install(&task_root, &task_user, move |path| {
            fs::write(path.join("SKILL.md"), "partial install")?;
            entered_tx.send(path.to_path_buf()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            Ok(15)
        })
        .await
    });
    let staging = entered_rx.await.unwrap();
    handler.abort();
    assert!(matches!(handler.await, Err(error) if error.is_cancelled()));
    assert!(staging.exists());
    assert!(!cleanup_abandoned(&root).unwrap());
    release_tx.send(()).unwrap();
    wait_for_removal(&staging).await;
    assert!(user_root.exists());
}

#[tokio::test]
async fn dropping_prepared_install_cleans_staging_without_deleting_the_user_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let user_root = root.join("users/fixture-user");
    let prepared = prepare_install(&root, &user_root, |path| {
        fs::write(path.join("SKILL.md"), "prepared install")?;
        Ok(16)
    })
    .await
    .unwrap();
    let staging = prepared.staging.path.clone();
    assert!(!cleanup_abandoned(&root).unwrap());
    drop(prepared);
    assert!(!staging.exists());
    assert!(user_root.exists());
}

#[tokio::test]
async fn cancelled_commit_worker_retains_the_guard_until_commit_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let user_root = root.join("users/fixture-user");
    let prepared = prepare_install(&root, &user_root, |path| {
        fs::write(path.join("SKILL.md"), "prepared install")?;
        Ok(16)
    })
    .await
    .unwrap();
    let staging = prepared.staging.path.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let handler = tokio::spawn(finish_install(prepared, move |_| {
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Err(ExtensionError::Internal("test commit stopped".into()))
    }));
    entered_rx.await.unwrap();
    handler.abort();
    assert!(matches!(handler.await, Err(error) if error.is_cancelled()));
    assert!(staging.exists());
    assert!(!cleanup_abandoned(&root).unwrap());
    release_tx.send(()).unwrap();
    wait_for_removal(&staging).await;
}

async fn wait_for_removal(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn owner_fixture(root: &Path, partial: bool) -> tokio::process::Child {
    let mut builder = aionui_runtime::Builder::new(std::env::current_exe().unwrap());
    builder
        .args([
            "--exact",
            "skill_service::import_staging::tests::child_hold_import_lease",
            "--nocapture",
        ])
        .env("AELIX_STAGING_TEST_CHILD_ROOT", root)
        .env("AELIX_STAGING_TEST_CHILD_INSTALL", "1")
        .env("AELIX_STAGING_TEST_CHILD_LEASED", "1")
        .env_remove("AELIX_STAGING_TEST_PARTIAL_OWNER")
        .stdout(std::process::Stdio::null());
    if partial {
        builder.env("AELIX_STAGING_TEST_PARTIAL_OWNER", "1");
    }
    let mut child = builder.spawn().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join("child-ready").exists() {
            assert!(child.try_wait().unwrap().is_none(), "owner fixture exited early");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    child
}

#[tokio::test]
async fn interrupted_owner_publication_is_neither_hidden_nor_reclaimed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let mut child = owner_fixture(&root, true).await;
    let stage = PathBuf::from(fs::read_to_string(root.join("owned-path")).unwrap());
    assert!(!ownership::marker_path(&stage).unwrap().exists());
    assert!(!cleanup_abandoned(&root).unwrap());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    drop(child);
    assert!(
        fs::read_dir(stage.parent().unwrap()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".pending-"))
    );
    assert!(!is_abandoned(&root, &stage));
    assert!(!cleanup_abandoned(&root).unwrap());
    assert_eq!(fs::read_to_string(stage.join("SKILL.md")).unwrap(), "unpublished owner");
    let mut discovered = Vec::new();
    super::super::collect_skill_dirs_recursive(&root, &mut discovered, Some(&root))
        .await
        .unwrap();
    assert!(discovered.contains(&stage));
}

#[tokio::test]
async fn an_owner_record_with_an_active_marker_lease_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let mut child = owner_fixture(&root, false).await;
    let stage = PathBuf::from(fs::read_to_string(root.join("owned-path")).unwrap());
    assert!(!is_abandoned(&root, &stage));
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    drop(child);
    let marker = OpenOptions::new()
        .read(true)
        .write(true)
        .open(ownership::marker_path(&stage).unwrap())
        .unwrap();
    FileExt::lock_shared(&marker).unwrap();
    assert!(!is_abandoned(&root, &stage));
    assert!(!cleanup_abandoned(&root).unwrap());
    assert!(stage.join("SKILL.md").exists());
    drop(marker);
    assert!(is_abandoned(&root, &stage));
    assert!(cleanup_abandoned(&root).unwrap());
    assert!(!stage.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn owned_abandoned_symlink_is_unlinked_without_following_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let mut child = owner_fixture(&root, false).await;
    let stage = PathBuf::from(fs::read_to_string(root.join("owned-path")).unwrap());
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    drop(child);
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), "unchanged").unwrap();
    fs::remove_dir_all(&stage).unwrap();
    std::os::unix::fs::symlink(&outside, &stage).unwrap();
    assert!(cleanup_abandoned(&root).unwrap());
    assert!(stage.symlink_metadata().is_err());
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "unchanged");
}

#[tokio::test]
async fn an_unmarked_existing_temp_container_is_not_modified_by_new_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let existing = root.join(TEMP_DIR_NAME);
    fs::create_dir_all(&existing).unwrap();
    fs::write(existing.join("SKILL.md"), "existing user data").unwrap();
    let extracted = extract_with(root.clone(), |path| {
        fs::write(path.join("SKILL.md"), "new extraction")?;
        Ok(Vec::new())
    })
    .await
    .unwrap();
    assert_ne!(extracted.staging.temp_root, existing);
    drop(extracted);
    assert_eq!(
        fs::read_to_string(existing.join("SKILL.md")).unwrap(),
        "existing user data"
    );
    assert_eq!(fs::read_dir(&existing).unwrap().count(), 1);
    assert!(!is_abandoned(&root, &existing));
    assert!(!cleanup_abandoned(&root).unwrap());
}

#[tokio::test]
async fn an_orphan_record_is_preserved_and_does_not_block_new_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let original = root.join(TEMP_DIR_NAME);
    fs::create_dir_all(&original).unwrap();
    let owner = OwnerLease::publish(&original, Kind::Container).unwrap();
    fs::remove_dir(&original).unwrap();
    drop(owner); // owner host is still alive; its record must be preserved
    let marker = ownership::marker_path(&original).unwrap();
    let before = fs::read(&marker).unwrap();
    let extracted = extract_with(root.clone(), |_| Ok(Vec::new())).await.unwrap();
    assert_ne!(extracted.staging.temp_root, original);
    drop(extracted);
    assert_eq!(fs::read(marker).unwrap(), before);
    assert!(!original.exists());
}

#[tokio::test]
async fn startup_install_sweep_preserves_leased_and_legacy_live_hosts() {
    for leased in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let user_root = root.join("users/fixture-user");
        let mut builder = aionui_runtime::Builder::new(std::env::current_exe().unwrap());
        builder
            .args([
                "--exact",
                "skill_service::import_staging::tests::child_hold_import_lease",
                "--nocapture",
            ])
            .env("AELIX_STAGING_TEST_CHILD_ROOT", &root)
            .env("AELIX_STAGING_TEST_CHILD_INSTALL", "1")
            .env_remove("AELIX_STAGING_TEST_CHILD_LEASED")
            .env_remove("AELIX_STAGING_TEST_PARTIAL_OWNER")
            .stdout(std::process::Stdio::null());
        if leased {
            builder.env("AELIX_STAGING_TEST_CHILD_LEASED", "1");
        }
        let mut child = builder.spawn().unwrap();
        let pid = child.id().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !root.join("child-ready").exists() {
                assert!(child.try_wait().unwrap().is_none(), "child exited before readiness");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let legacy = user_root.join(format!(".import-staging-leased-legacy-skill-{pid}-1"));
        let abandoned = user_root.join(".import-staging-leased-abandoned.tmp");
        fs::create_dir_all(&abandoned).unwrap();
        assert!(!cleanup_abandoned(&root).unwrap());
        assert!(legacy.join("SKILL.md").exists());
        assert!(abandoned.exists());
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        drop(child);
        if leased {
            let owned = PathBuf::from(fs::read_to_string(root.join("owned-path")).unwrap());
            assert!(is_abandoned(&root, &owned));
            assert!(cleanup_abandoned(&root).unwrap());
            assert!(!owned.exists());
        } else {
            assert!(!cleanup_abandoned(&root).unwrap());
        }
        assert!(legacy.join("SKILL.md").exists());
        assert!(abandoned.exists());
        assert_eq!(fs::read_dir(&user_root).unwrap().count(), 2);
        assert!(user_root.exists());
    }
}

#[test]
fn startup_install_sweep_preserves_unknown_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let unknown = root.join(".import-staging-unknown");
    fs::create_dir_all(&unknown).unwrap();
    fs::write(unknown.join("SKILL.md"), "unknown ownership").unwrap();
    assert!(!cleanup_abandoned(&root).unwrap());
    assert!(unknown.join("SKILL.md").exists());
}

#[cfg(unix)]
#[test]
fn startup_install_sweep_does_not_follow_symlinked_user_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let outside = tmp.path().join("outside");
    let preserved = outside.join(".import-staging-leased-preserved.tmp/SKILL.md");
    fs::create_dir_all(preserved.parent().unwrap()).unwrap();
    fs::write(&preserved, "not owned by this storage root").unwrap();
    fs::create_dir_all(root.join("users")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("users/linked-owner")).unwrap();
    assert!(!cleanup_abandoned(&root).unwrap());
    assert_eq!(fs::read_to_string(preserved).unwrap(), "not owned by this storage root");
}
