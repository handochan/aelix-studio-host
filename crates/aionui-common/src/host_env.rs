//! Process settings reserved for the host. Declare new host-only variables
//! here so their typed readers and the child environment policy share a list.
//!
//! Raw environment access lives here for the typed inventories, the agent
//! environment snapshot, and extension templates that expand user variables.
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::OnceLock;

macro_rules! environment_settings {
    ($type:ident { $($setting:ident => $name:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $type { $($setting),+ }

        impl $type {
            pub const ALL: &'static [Self] = &[$(Self::$setting),+];

            pub const fn name(self) -> &'static str {
                match self { $(Self::$setting => $name),+ }
            }
        }
    };
}

environment_settings! { HostEnv {
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
}

// Platform and helper inputs that belong to a child-facing contract. LogDir
// is shared with the agent's diagnose command. Shell AWS credentials retain
// their existing inheritance.
environment_settings! { SharedEnv {
    Path => "PATH",
    Home => "HOME",
    Shell => "SHELL",
    AppData => "APPDATA",
    LocalAppData => "LOCALAPPDATA",
    ProgramFiles => "ProgramFiles",
    ProgramFilesX86 => "ProgramFiles(x86)",
    Scoop => "SCOOP",
    LogDir => "AIONUI_LOG_DIR",
    BaseUrl => "AIONUI_BASE_URL",
    UserId => "AIONUI_USER_ID",
    ConversationId => "AIONUI_CONVERSATION_ID",
    RuntimeToken => "AIONUI_RUNTIME_TOKEN",
    TeamMcpPort => "TEAM_MCP_PORT",
    TeamMcpToken => "TEAM_MCP_TOKEN",
    TeamMcpSlot => "TEAM_AGENT_SLOT_ID",
    HookBaseUrl => "AIONUI_ANTIGRAVITY_HOOK_BASE_URL",
    HookToken => "AIONUI_ANTIGRAVITY_HOOK_TOKEN",
    HookConversationId => "AIONUI_ANTIGRAVITY_HOOK_CONVERSATION_ID",
} }

const SCOPED_CREDENTIALS: &[SharedEnv] = &[SharedEnv::RuntimeToken, SharedEnv::TeamMcpToken, SharedEnv::HookToken];
static STARTUP_VALUES: OnceLock<HashMap<&'static str, OsString>> = OnceLock::new();

impl HostEnv {
    pub fn read_os(self) -> Option<OsString> {
        startup_value(self.name())
    }

    pub fn read(self) -> Option<String> {
        self.read_os()?.into_string().ok()
    }
}

impl SharedEnv {
    pub fn read_os(self) -> Option<OsString> {
        if SCOPED_CREDENTIALS.contains(&self) {
            startup_value(self.name())
        } else {
            std::env::var_os(self.name())
        }
    }

    pub fn read(self) -> Option<String> {
        self.read_os()?.into_string().ok()
    }
}

/// The shared input fields for the conversation, session, team and other
/// scoped CLI helpers. Arbitrary names are outside that contract.
pub fn read_session_env(name: &str) -> Option<String> {
    [
        SharedEnv::BaseUrl,
        SharedEnv::UserId,
        SharedEnv::ConversationId,
        SharedEnv::RuntimeToken,
    ]
    .into_iter()
    .find(|setting| setting.name() == name)?
    .read()
}

fn startup_value(name: &'static str) -> Option<OsString> {
    match STARTUP_VALUES.get() {
        Some(values) => values.get(name).cloned(),
        // Library callers can read the setting without process-wide setup.
        None => std::env::var_os(name),
    }
}

/// Capture host settings and scoped credentials before removing them from
/// the ambient environment, including for subprocesses inside dependencies.
///
/// # Safety
/// Call at single-threaded binary startup, before any thread can read the
/// process environment and before launching any subprocesses.
pub unsafe fn capture_at_startup() {
    STARTUP_VALUES.get_or_init(|| {
        HostEnv::ALL
            .iter()
            .map(|setting| setting.name())
            .chain(SCOPED_CREDENTIALS.iter().map(|setting| setting.name()))
            .filter_map(|name| {
                let value = std::env::var_os(name)?;
                // SAFETY: the caller guarantees exclusive environment access.
                unsafe { std::env::remove_var(name) };
                Some((name, value))
            })
            .collect()
    });
}

fn matches_name(key: &OsStr, name: &str) -> bool {
    if cfg!(windows) {
        key.eq_ignore_ascii_case(OsStr::new(name))
    } else {
        key == OsStr::new(name)
    }
}

pub fn is_host_env(key: &OsStr) -> bool {
    HostEnv::ALL.iter().any(|setting| matches_name(key, setting.name()))
}

/// A login-shell snapshot imports ambient variables too. Filter it before
/// applying the caller's per-session overrides.
pub fn is_inheritable(key: &OsStr) -> bool {
    !is_host_env(key)
        && !SCOPED_CREDENTIALS
            .iter()
            .any(|setting| matches_name(key, setting.name()))
}

pub fn ambient_environment() -> Vec<(OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| is_inheritable(key)).collect()
}

/// Extension templates can expand arbitrary user variables, excluding host
/// settings and scoped credentials that could otherwise be copied under aliases.
pub fn read_template_env(name: &str) -> Option<String> {
    if !is_inheritable(name.as_ref()) {
        return None;
    }
    std::env::var(name).ok()
}

/// Remove reserved names and suppress inherited scoped credentials while
/// preserving explicit credential injection on the command.
pub fn strip_from_command(command: &mut Command) {
    for setting in HostEnv::ALL {
        command.env_remove(setting.name());
    }
    for setting in SCOPED_CREDENTIALS {
        let name = setting.name();
        let explicitly_set = command
            .get_envs()
            .any(|(key, value)| matches_name(key, name) && value.is_some());
        if !explicitly_set {
            command.env_remove(name);
        }
    }
}
