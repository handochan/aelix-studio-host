//! Smoke test: starting the app twice with the same binary version
//! should be a no-op on the second run (version gate skips rewrite).

use tempfile::TempDir;

#[cfg(debug_assertions)]
#[test]
fn standalone_entry_captures_environment_before_cli_parsing() {
    use aionui_common::host_env::HostEnv;
    use std::process::Command;

    // --version exits during parsing, before any launcher could scrub a child.
    // The debug binary checks its process environment on the way to parsing.
    let key = HostEnv::JwtSecret.name();
    let key = if cfg!(windows) {
        key.to_ascii_lowercase()
    } else {
        key.to_owned()
    };
    let output = Command::new(env!("CARGO_BIN_EXE_aelix-studio-host"))
        .arg("--version")
        .env(key, "startup-signing-key-fixture")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("aelix-studio-host "));
}

#[tokio::test]
async fn second_start_with_same_version_is_noop() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    let first =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-1.0.0")
            .await
            .unwrap();
    assert!(first, "first call should materialize");

    let second =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-1.0.0")
            .await
            .unwrap();
    assert!(!second, "second call with same version should skip");
}

#[tokio::test]
async fn version_bump_triggers_rewrite() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path();

    let first =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-1.0.0")
            .await
            .unwrap();
    assert!(first);

    let second =
        aionui_extension::materialize_if_needed(data_dir, aionui_extension::builtin_skills_corpus(), "test-2.0.0")
            .await
            .unwrap();
    assert!(second, "version change should trigger a fresh materialize");

    let version = std::fs::read_to_string(data_dir.join("builtin-skills").join(".version")).unwrap();
    assert_eq!(version, "test-2.0.0");
}
