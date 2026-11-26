use std::env::consts::EXE_SUFFIX;
use std::process::Command;

use aionui_common::host_env::{HostEnv, SharedEnv};

#[test]
fn startup_capture_consumes_and_scrubs_the_environment() {
    // Run the production capture function in a separate, single-threaded
    // executable. Inspect its own environment before any launcher can filter it.
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join(format!("startup_environment_probe{EXE_SUFFIX}"));
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/startup_environment_probe.rs"
    );
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let built = Command::new(rustc)
        .args(["--edition=2024", fixture, "-o"])
        .arg(&probe)
        .output()
        .unwrap();
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));

    let names = HostEnv::ALL.iter().map(|setting| setting.name()).chain(
        [SharedEnv::RuntimeToken, SharedEnv::TeamMcpToken, SharedEnv::HookToken]
            .into_iter()
            .map(SharedEnv::name),
    );
    let output = Command::new(probe)
        .envs(names.map(|name| {
            let key = if cfg!(windows) {
                name.to_ascii_lowercase()
            } else {
                name.to_owned()
            };
            (key, format!("fixture:{name}"))
        }))
        .env(SharedEnv::LogDir.name(), "shared-fixture")
        .env("AWS_ACCESS_KEY_ID", "shell-fixture")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
