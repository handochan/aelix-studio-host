//! Launch configuration supplied by the local Studio host parent (host #67).

use std::path::{Path, PathBuf};

use aionui_api_types::{AgentMetadata, AgentSource};
use aionui_common::{CommandSpec, EnvVar};
use serde::Deserialize;

pub(crate) const AGENT_ID: &str = "builtin-aelix";
pub(crate) const RUNTIME_UNAVAILABLE: &str = "Studio did not provide a usable Aelix runtime";
const LAUNCH_ENV: &str = "AIONUI_STUDIO_AELIX_LAUNCH";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StudioAelixLaunch {
    command: String,
    args: Vec<String>,
    env: LaunchEnv,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchEnv {
    #[serde(rename = "AELIX_CODING_AGENT_DIR")]
    agent_dir: String,
    #[serde(rename = "AELIX_ACP_STUDIO_MANAGED")]
    managed: String,
    #[serde(rename = "AELIX_STUDIO_EXTENSION_STATE")]
    extension_state: Option<String>,
}

pub(crate) fn uses_studio_launch(meta: &AgentMetadata) -> bool {
    meta.id == AGENT_ID
        && meta.agent_source == AgentSource::Builtin
        && meta.backend.as_deref() == Some("aelix")
        && !meta.has_command_override
}

impl StudioAelixLaunch {
    pub(crate) fn load() -> Option<Self> {
        Self::parse(&std::env::var(LAUNCH_ENV).ok()?)
    }

    fn parse(raw: &str) -> Option<Self> {
        let launch: Self = serde_json::from_str(raw).ok()?;
        (is_absolute_path(&launch.command)
            && is_absolute_path(&launch.env.agent_dir)
            && launch.env.managed == "1"
            && launch.env.extension_state.as_deref().is_none_or(is_absolute_path)
            && launch
                .args
                .iter()
                .all(|arg| !arg.contains('\0') && !matches!(arg.split('=').next(), Some("--provider" | "--model")))
            && aionui_runtime::outbound_policy::ensure_implicit_package_command_allowed(&launch.command, &launch.args)
                .is_ok())
        .then_some(launch)
    }

    pub(crate) fn resolve_command(&self) -> Option<PathBuf> {
        let path = aionui_runtime::resolve_command_path(&self.command)?;
        // The runtime builder wraps Windows script shims in a shell. Studio
        // supplies a native executable whose argv can be passed directly.
        #[cfg(windows)]
        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("exe"))
        {
            return None;
        }
        path.is_file().then_some(path)
    }

    pub(crate) fn command_spec(self, meta: &AgentMetadata, cwd: String) -> Option<CommandSpec> {
        let command = self.resolve_command()?;
        let mut env = vec![
            EnvVar {
                name: "AELIX_CODING_AGENT_DIR".into(),
                value: self.env.agent_dir,
            },
            EnvVar {
                name: "AELIX_ACP_STUDIO_MANAGED".into(),
                value: self.env.managed,
            },
        ];
        if let Some(value) = self.env.extension_state {
            env.push(EnvVar {
                name: "AELIX_STUDIO_EXTENSION_STATE".into(),
                value,
            });
        }
        // The registry already filtered overrides. Apply them last, as it does
        // for other builtins, and leave their provenance in metadata for relay.
        env.extend(meta.env.iter().map(|entry| EnvVar {
            name: entry.name.clone(),
            value: entry.value.clone(),
        }));
        Some(CommandSpec {
            command,
            args: self.args,
            env,
            cwd: Some(cwd),
        })
    }
}

fn is_absolute_path(value: &str) -> bool {
    !value.contains('\0') && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn launch_json() -> Value {
        json!({
            "command": std::env::current_exe().unwrap(),
            "args": ["--profile", "coding"],
            "env": {
                "AELIX_CODING_AGENT_DIR": std::env::temp_dir(),
                "AELIX_ACP_STUDIO_MANAGED": "1"
            }
        })
    }

    #[test]
    fn launch_requires_a_valid_local_descriptor() {
        let valid = launch_json();
        let launch = StudioAelixLaunch::parse(&valid.to_string()).unwrap();
        assert!(launch.resolve_command().is_some());

        let mut missing = valid.clone();
        missing["command"] = json!(std::env::temp_dir().join("aelix-missing-runtime").join("fixture.exe"));
        assert!(
            StudioAelixLaunch::parse(&missing.to_string())
                .unwrap()
                .resolve_command()
                .is_none()
        );
        for (field, invalid) in [
            ("command", json!("node")),
            ("args", json!(["--model", "fixture"])),
            ("args", json!(["--provider=fixture"])),
            ("env", json!({ "AELIX_ACP_STUDIO_MANAGED": "1" })),
        ] {
            let mut input = valid.clone();
            input[field] = invalid;
            assert!(StudioAelixLaunch::parse(&input.to_string()).is_none(), "{field}");
        }
        for (key, value) in [
            ("OPENAI_API_KEY", "synthetic-secret"),
            ("AELIX_CODING_AGENT_DIR", "relative/agent"),
            ("AELIX_ACP_STUDIO_MANAGED", "0"),
        ] {
            let mut input = valid.clone();
            input["env"][key] = json!(value);
            assert!(StudioAelixLaunch::parse(&input.to_string()).is_none(), "{key}");
        }
        assert!(StudioAelixLaunch::parse("not JSON").is_none());
    }
}
