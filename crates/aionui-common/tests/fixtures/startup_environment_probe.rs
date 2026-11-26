// Compiled and run as a standalone process by startup_environment.rs.
#[allow(dead_code)]
#[path = "../../src/host_env.rs"]
mod host_env;

use std::ffi::OsString;

use host_env::{HostEnv, SharedEnv, capture_at_startup};

fn main() {
    // SAFETY: this entry point has no test harness or worker threads.
    unsafe { capture_at_startup() };

    for &setting in HostEnv::ALL {
        let name = setting.name();
        assert!(
            std::env::var_os(name).is_none(),
            "ambient {name} survived startup capture"
        );
        assert_eq!(setting.read_os(), Some(OsString::from(format!("fixture:{name}"))));
    }
    for setting in [SharedEnv::RuntimeToken, SharedEnv::TeamMcpToken, SharedEnv::HookToken] {
        let name = setting.name();
        assert!(
            std::env::var_os(name).is_none(),
            "ambient {name} survived startup capture"
        );
        assert_eq!(setting.read_os(), Some(OsString::from(format!("fixture:{name}"))));
    }
    assert_eq!(
        std::env::var_os(SharedEnv::LogDir.name()),
        Some("shared-fixture".into())
    );
    assert_eq!(std::env::var_os("AWS_ACCESS_KEY_ID"), Some("shell-fixture".into()));
}
