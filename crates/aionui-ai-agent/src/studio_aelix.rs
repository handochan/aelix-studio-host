//! Launch configuration supplied by the local Studio host parent (host #67).

use std::path::{Path, PathBuf};

use aionui_api_types::{AgentMetadata, AgentSource};
use aionui_common::{CommandSpec, EnvVar};
use serde::Deserialize;

pub(crate) const AGENT_ID: &str = "builtin-aelix";
pub(crate) const RUNTIME_UNAVAILABLE: &str = "Studio did not provide a usable Aelix runtime";
pub(crate) const AGENT_DISABLED: &str = "Builtin Aelix is disabled";

pub(crate) fn probe_failure_message(code: &str) -> &'static str {
    match code {
        "auth_required" => "Builtin Aelix requires authentication",
        "command_not_found" => "Builtin Aelix launcher is unavailable",
        _ => "Builtin Aelix did not complete the ACP connection check",
    }
}

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

pub(crate) fn is_catalog_entry(meta: &AgentMetadata) -> bool {
    meta.id == AGENT_ID && meta.agent_source == AgentSource::Builtin && meta.backend.as_deref() == Some("aelix")
}

impl StudioAelixLaunch {
    /// Parses the descriptor Studio supplied at startup and logs, without any
    /// path, why it was not accepted. Otherwise Aelix would only show as missing.
    pub(crate) fn parse_and_report(raw: &str) -> Option<Self> {
        let Some(launch) = Self::parse(raw) else {
            tracing::warn!(
                reason = "invalid_descriptor",
                "Studio Aelix launch descriptor was rejected; builtin Aelix is unavailable"
            );
            return None;
        };
        if launch.resolve_command().is_none() {
            tracing::warn!(
                reason = "unsupported_launch",
                "Studio Aelix launch form, interpreter or launcher file was not accepted; builtin Aelix is unavailable"
            );
        }
        Some(launch)
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
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
        let command = Path::new(&self.command);
        let interpreter = command.file_name()?.to_str()?;
        #[cfg(windows)]
        let interpreter = interpreter.to_ascii_lowercase();
        #[cfg(windows)]
        let interpreter = interpreter.strip_suffix(".exe")?;

        let launcher = match self.args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            ["-I", "-S", "-B", "-X", "utf8", script, "acp", "--profile", "coding"] if is_python(interpreter) => {
                existing_launcher(script, "launch.py")?
            }
            [script, "--profile", "coding"] if interpreter == "node" => existing_launcher(script, "launch.cjs")?,
            _ => return None,
        };
        if !launcher.is_file() {
            return None;
        }
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

    pub(crate) fn command_spec(&self, meta: &AgentMetadata, cwd: String) -> Option<CommandSpec> {
        let command = self.resolve_command()?;
        let mut env = vec![
            EnvVar {
                name: "AELIX_CODING_AGENT_DIR".into(),
                value: self.env.agent_dir.clone(),
            },
            EnvVar {
                name: "AELIX_ACP_STUDIO_MANAGED".into(),
                value: self.env.managed.clone(),
            },
        ];
        if let Some(value) = &self.env.extension_state {
            env.push(EnvVar {
                name: "AELIX_STUDIO_EXTENSION_STATE".into(),
                value: value.clone(),
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
            args: self.args.clone(),
            env,
            cwd: Some(cwd),
        })
    }
}

fn is_python(name: &str) -> bool {
    matches!(name, "python" | "python3")
        || name
            .strip_prefix("python3.")
            .is_some_and(|minor| !minor.is_empty() && minor.bytes().all(|byte| byte.is_ascii_digit()))
}

fn existing_launcher<'a>(value: &'a str, name: &str) -> Option<&'a Path> {
    let path = Path::new(value);
    (is_absolute_path(value) && path.file_name().and_then(|file| file.to_str()) == Some(name)).then_some(path)
}

fn is_absolute_path(value: &str) -> bool {
    !value.contains('\0') && Path::new(value).is_absolute()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn launch_json(root: &Path, name: &str) -> Value {
        let executable = root.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        });
        std::fs::write(&executable, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let launcher = root.join(if name == "node" { "launch.cjs" } else { "launch.py" });
        std::fs::write(&launcher, "").unwrap();
        let args = if name == "node" {
            json!([launcher, "--profile", "coding"])
        } else {
            json!(["-I", "-S", "-B", "-X", "utf8", launcher, "acp", "--profile", "coding"])
        };
        json!({
            "command": executable,
            "args": args,
            "env": {
                "AELIX_CODING_AGENT_DIR": root,
                "AELIX_ACP_STUDIO_MANAGED": "1"
            }
        })
    }

    #[test]
    fn rejects_relative_and_missing_launchers_for_both_runtime_forms() {
        let root = tempfile::tempdir().unwrap();
        for (interpreter, launcher, index) in [("python3", "launch.py", 5), ("node", "launch.cjs", 0)] {
            let mut input = launch_json(root.path(), interpreter);
            assert!(
                StudioAelixLaunch::parse(&input.to_string())
                    .unwrap()
                    .resolve_command()
                    .is_some()
            );
            let launcher_path = root.path().join(launcher);
            std::fs::remove_file(&launcher_path).unwrap();
            for launcher in [PathBuf::from(launcher), launcher_path] {
                input["args"][index] = json!(launcher);
                assert!(
                    StudioAelixLaunch::parse(&input.to_string())
                        .and_then(|launch| launch.resolve_command())
                        .is_none(),
                    "{launcher:?} must not resolve"
                );
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_node_executable_and_launcher_with_spaces_are_passed_verbatim() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Studio Runtime With Spaces");
        std::fs::create_dir(&root).unwrap();
        let input = launch_json(&root, "node");
        let node = aionui_runtime::resolve_command_path("node.exe").expect("Windows runner must provide native Node");
        std::fs::copy(node, root.join("node.exe")).unwrap();
        std::fs::write(
            root.join("launch.cjs"),
            "console.log(JSON.stringify(process.argv.slice(2)));",
        )
        .unwrap();
        let launch = StudioAelixLaunch::parse(&input.to_string()).unwrap();
        let mut builder = aionui_runtime::Builder::new(launch.resolve_command().unwrap());
        builder.args(&launch.args);
        let output = builder.output().await.unwrap();
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!(["--profile", "coding"])
        );
    }

    #[test]
    fn launch_requires_a_valid_local_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let valid = launch_json(root.path(), "python3");
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
