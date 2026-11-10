use std::path::Path;

#[tokio::test]
async fn ended_owner_child() {
    if std::env::var_os("AELIX_ENDED_OWNER_FIXTURE").is_some() {
        std::process::exit(0);
    }
}

pub async fn ended_pid() -> u32 {
    let mut builder = aionui_runtime::Builder::new(std::env::current_exe().unwrap());
    builder
        .args(["--exact", "owner_fixture::ended_owner_child", "--nocapture"])
        .env("AELIX_ENDED_OWNER_FIXTURE", "1")
        .stdout(std::process::Stdio::null());
    let mut child = builder.spawn().unwrap();
    let pid = child.id().unwrap();
    assert!(child.wait().await.unwrap().success());
    drop(child);
    assert!(!aionui_runtime::process_may_exist(pid));
    pid
}

// Complete crash-record fixture; the PID is from a real exited process.
pub fn write_owner(path: &Path, kind: &str, pid: u32) {
    let marker = path.with_file_name(format!(
        "{}.aelix-staging-owner",
        path.file_name().unwrap().to_str().unwrap()
    ));
    std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
    std::fs::write(
        marker,
        serde_json::to_vec(&serde_json::json!({
            "schema": "aelix.skill-staging/1", "stage_name": path.file_name().unwrap().to_str().unwrap(),
            "kind": kind, "pid": pid, "token": format!("fixture-{pid}")
        }))
        .unwrap(),
    )
    .unwrap();
}
