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
    let _guard = if std::env::var_os("AELIX_STAGING_TEST_CHILD_LEASED").is_some() {
        let guard = ZipStaging::create(&root).unwrap();
        fs::write(guard.path().join("SKILL.md"), "active leased import").unwrap();
        Some(guard)
    } else {
        None
    };
    // Simulate a still-running old host that predates the shared lease.
    let legacy = root
        .join(TEMP_DIR_NAME)
        .join(format!("skills-{}-1", std::process::id()));
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

        assert_eq!(cleanup_abandoned(&root).unwrap(), !leased);
        assert!(legacy.join("SKILL.md").exists());
        assert_eq!(stale.exists(), leased);

        child.kill().await.unwrap();
        child.wait().await.unwrap();
        drop(child);
        assert!(cleanup_abandoned(&root).unwrap());
        assert!(!root.join(TEMP_DIR_NAME).exists());
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
fn startup_cleanup_does_not_follow_staging_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("skills");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep"), "unchanged").unwrap();
    fs::create_dir_all(root.join(TEMP_DIR_NAME)).unwrap();
    std::os::unix::fs::symlink(&outside, root.join(TEMP_DIR_NAME).join("leased-link")).unwrap();
    assert!(cleanup_abandoned(&root).unwrap());
    assert_eq!(fs::read_to_string(outside.join("keep")).unwrap(), "unchanged");
}
