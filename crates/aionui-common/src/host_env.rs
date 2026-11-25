//! Process settings reserved for the host. Declare new host-only variables
//! here so their typed readers and the child environment policy share a list.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::OnceLock;

macro_rules! host_settings {
    ($($setting:ident => $name:literal),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub enum HostEnv { $($setting),+ }

        impl HostEnv {
            pub const ALL: &'static [Self] = &[$(Self::$setting),+];

            pub const fn name(self) -> &'static str {
                match self { $(Self::$setting => $name),+ }
            }
        }
    };
}

host_settings! {
    JwtSecret => "JWT_SECRET",
    EncryptionSecret => "AIONUI_ENCRYPTION_SECRET",
    BootstrapSecret => "AIONCORE_BOOTSTRAP_SECRET",
    StudioAelixLaunch => "AIONUI_STUDIO_AELIX_LAUNCH",
    Https => "AIONUI_HTTPS",
    WorkDir => "AIONUI_WORK_DIR",
    CacheDir => "AIONUI_CACHE_DIR",
    BuiltinAssistants => "AIONUI_BUILTIN_ASSISTANTS_PATH",
    BuiltinSkills => "AIONUI_BUILTIN_SKILLS_PATH",
    BundledResources => "AIONUI_BUNDLED_MANAGED_RESOURCES",
    ExtensionsPath => "AIONUI_EXTENSIONS_PATH",
    ExtensionStatesFile => "AIONUI_EXTENSION_STATES_FILE",
    UpdateCheck => "AELIX_STUDIO_HOST_UPDATE_CHECK",
    GithubRepo => "AIONUI_GITHUB_REPO",
    RemoteImages => "AELIX_STUDIO_HOST_REMOTE_IMAGES",
    NodeMirror => "AELIX_STUDIO_HOST_NODE_MIRROR",
    NpmRegistry => "AELIX_STUDIO_HOST_NPM_REGISTRY",
    HandshakeTimeout => "AIONUI_HANDSHAKE_TIMEOUT_SECS",
    AcpInitTimeout => "AIONUI_ACP_INIT_TIMEOUT_SECS",
    IdleTimeout => "AIONUI_IDLE_TIMEOUT_SECS",
    TeamIdleTimeout => "AIONUI_TEAM_IDLE_TIMEOUT_SECS",
    IdleScanInterval => "AIONUI_IDLE_SCAN_INTERVAL_SECS",
    ClaudeWireDump => "AIONUI_CLAUDE_WIRE_DUMP",
    BypassProbe => "AIONUI_BYPASS_PROBE",
    E2eTest => "AIONUI_E2E_TEST",
}

// AIONUI_LOG_DIR is also read by the agent's diagnose helper. Runtime, team
// MCP and hook credentials are issued for children and carried by their
// existing scoped launch contracts.
static STARTUP_VALUES: OnceLock<HashMap<HostEnv, OsString>> = OnceLock::new();

impl HostEnv {
    pub fn read_os(self) -> Option<OsString> {
        match STARTUP_VALUES.get() {
            Some(values) => values.get(&self).cloned(),
            // Library callers can read the setting without process-wide setup.
            None => std::env::var_os(self.name()),
        }
    }

    pub fn read(self) -> Option<String> {
        self.read_os()?.into_string().ok()
    }
}

/// Capture host settings before removing them from the ambient environment.
/// This also protects subprocesses launched internally by dependencies.
///
/// # Safety
/// Call at single-threaded binary startup, before any thread can read the
/// process environment and before launching any subprocesses.
pub unsafe fn capture_at_startup() {
    STARTUP_VALUES.get_or_init(|| {
        HostEnv::ALL
            .iter()
            .filter_map(|&setting| {
                let value = std::env::var_os(setting.name())?;
                // SAFETY: the caller guarantees exclusive environment access.
                unsafe { std::env::remove_var(setting.name()) };
                Some((setting, value))
            })
            .collect()
    });
}

pub fn is_host_env(key: &OsStr) -> bool {
    HostEnv::ALL.iter().any(|setting| {
        if cfg!(windows) {
            key.eq_ignore_ascii_case(OsStr::new(setting.name()))
        } else {
            key == OsStr::new(setting.name())
        }
    })
}

/// Remove both inherited and explicitly supplied host-only variables.
pub fn strip_from_command(command: &mut Command) {
    for setting in HostEnv::ALL {
        command.env_remove(setting.name());
    }
}
