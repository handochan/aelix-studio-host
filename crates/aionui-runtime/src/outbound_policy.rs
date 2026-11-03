//! Explicit opt-in sources for host-managed downloads. No public fallback.
use std::ffi::OsString;

use crate::node_runtime::NodeRuntimeError;

pub const NODE_MIRROR_ENV: &str = "AELIX_STUDIO_HOST_NODE_MIRROR";
pub const NPM_REGISTRY_ENV: &str = "AELIX_STUDIO_HOST_NPM_REGISTRY";

/// Recognize registry tools even when a caller supplies an explicit path or a
/// Windows launcher. Arbitrary custom scripts remain outside this policy.
pub fn is_registry_tool_command(command: &str) -> bool {
    matches!(
        command
            .trim()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "pnpx"
            | "bunx"
            | "uvx"
            | "npm"
            | "npx"
            | "npm.bat"
            | "npx.bat"
            | "npm.cmd"
            | "npx.cmd"
            | "npm.exe"
            | "npx.exe"
            | "npm.ps1"
            | "npx.ps1"
            | "npm-cli.js"
            | "npx-cli.js"
    )
}

pub(crate) fn configured_url(key: &str) -> Result<reqwest::Url, NodeRuntimeError> {
    parse_url(key, std::env::var(key).ok().as_deref())
}

fn parse_url(key: &str, value: Option<&str>) -> Result<reqwest::Url, NodeRuntimeError> {
    let value = value.map(str::trim).filter(|v| !v.is_empty()).ok_or_else(|| {
        tracing::info!(
            setting = key,
            reason = "not_configured",
            "host-managed outbound access blocked"
        );
        let remedy = if key == NODE_MIRROR_ENV {
            "provide a bundled Node runtime or configure an internal Node mirror"
        } else {
            "configure an internal npm registry to enable npm/npx"
        };
        NodeRuntimeError::managed_invalid(format!("OUTBOUND_DISABLED: {key} is not configured; {remedy}"))
    })?;
    let url = reqwest::Url::parse(value).map_err(|_| invalid_url(key))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_url(key));
    }
    Ok(url)
}

fn invalid_url(key: &str) -> NodeRuntimeError {
    NodeRuntimeError::managed_invalid(format!(
        "OUTBOUND_CONFIG_INVALID: {key} must be an HTTP(S) URL without credentials, query or fragment"
    ))
}

pub(crate) fn npm_env() -> Result<Vec<(OsString, OsString)>, NodeRuntimeError> {
    npm_env_for(false)
}

pub(crate) fn npm_env_for(user_configured: bool) -> Result<Vec<(OsString, OsString)>, NodeRuntimeError> {
    let mirror_set = std::env::var(NPM_REGISTRY_ENV)
        .ok()
        .is_some_and(|v| !v.trim().is_empty());
    let mut env: Vec<(OsString, OsString)> = [
        ("npm_config_offline", "false"),
        ("npm_config_update_notifier", "false"),
        ("npm_config_audit", "false"),
        ("npm_config_fund", "false"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()))
    .collect();
    if !user_configured || mirror_set {
        let registry = configured_url(NPM_REGISTRY_ENV)?;
        env.push(("npm_config_registry".into(), registry.as_str().into()));
        env.push(("npm_config_replace_registry_host".into(), "always".into()));
    }
    // With no host mirror, explicit users retain normal npm configuration:
    // npm's default is the public registry; user .npmrc/env choices remain valid.
    Ok(env)
}

pub fn is_node_package_wrapper(command: &str, args: &[String]) -> bool {
    matches!(
        command
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "node" | "node.exe"
    ) && args.first().is_some_and(|arg| {
        matches!(
            arg.rsplit(['/', '\\']).next().unwrap_or(""),
            "npx-cli.js" | "npx-cli.cjs" | "npm-cli.js"
        )
    })
}

/// Host-owned package launchers require an explicit mirror even when wrapped by Node.
pub fn ensure_implicit_package_command_allowed(command: &str, args: &[String]) -> Result<(), NodeRuntimeError> {
    let basename = command.rsplit(['/', '\\']).next().unwrap_or("").to_ascii_lowercase();
    if matches!(basename.as_str(), "pnpx" | "bunx" | "uvx") {
        return Err(NodeRuntimeError::managed_invalid(
            "OUTBOUND_DISABLED: implicit package launcher has no host mirror contract; configure an explicit custom agent instead",
        ));
    }
    let node_wrapper = is_node_package_wrapper(command, args);
    if is_registry_tool_command(command) || node_wrapper {
        npm_env()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_and_invalid_sources_fail_without_default() {
        for value in [None, Some(""), Some(" ")] {
            assert!(
                parse_url(NODE_MIRROR_ENV, value)
                    .unwrap_err()
                    .to_string()
                    .contains("OUTBOUND_DISABLED")
            );
        }
        for value in [
            "file:///tmp/node",
            "https://u:secret@mirror/",
            "https://mirror/?token=secret",
            "https://mirror/#x",
            "oops",
        ] {
            let error = parse_url(NODE_MIRROR_ENV, Some(value)).unwrap_err().to_string();
            assert!(error.contains("OUTBOUND_CONFIG_INVALID"));
            assert!(!error.contains("secret"));
        }
        assert_eq!(
            parse_url(NODE_MIRROR_ENV, Some("https://mirror.example/dist/"))
                .unwrap()
                .as_str(),
            "https://mirror.example/dist/"
        );
    }
    #[tokio::test]
    async fn registry_tools_fail_before_runtime_preparation() {
        if !crate::test_support::run_in_env_child(
            "outbound_policy::tests::registry_tools_fail_before_runtime_preparation",
            |command| {
                command.env_remove(NPM_REGISTRY_ENV);
                command.env_remove(NODE_MIRROR_ENV);
            },
        ) {
            return;
        }
        for tool in ["npm", "npx"] {
            let error = crate::ensure_runtime_command(tool).await.unwrap_err();
            assert!(error.to_string().contains("OUTBOUND_DISABLED"));
            assert!(error.to_string().contains(NPM_REGISTRY_ENV));
        }
    }

    #[test]
    fn configured_registry_environment_disables_background_requests() {
        if !crate::test_support::run_in_env_child(
            "outbound_policy::tests::configured_registry_environment_disables_background_requests",
            |command| {
                command.env(NPM_REGISTRY_ENV, "http://127.0.0.1:8123/repository/npm/");
            },
        ) {
            return;
        }
        let env = npm_env().unwrap();
        for (key, value) in [
            ("npm_config_registry", "http://127.0.0.1:8123/repository/npm/"),
            ("npm_config_update_notifier", "false"),
            ("npm_config_replace_registry_host", "always"),
        ] {
            assert!(env.contains(&(key.into(), value.into())));
        }
    }
    #[test]
    fn registry_tool_paths_and_wrappers_are_recognized() {
        for command in [
            "npx",
            "/usr/local/bin/npm",
            "C:\\Node\\npx.cmd",
            "NPX.PS1",
            "NPX.BAT",
            "/node/npm-cli.js",
        ] {
            assert!(is_registry_tool_command(command), "{command}");
        }
        for command in ["node", "/bin/python", "/custom/npx-agent", "npm-helper"] {
            assert!(!is_registry_tool_command(command), "{command}");
        }
    }

    #[tokio::test]
    async fn explicit_registry_tool_paths_require_mirror() {
        if !crate::test_support::run_in_env_child(
            "outbound_policy::tests::explicit_registry_tool_paths_require_mirror",
            |command| {
                command.env_remove(NPM_REGISTRY_ENV);
            },
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        for name in ["npm", "npx", "npx.cmd", "npm.ps1"] {
            let path = temp.path().join(name);
            std::fs::write(&path, "not executed").unwrap();
            let error = crate::ensure_runtime_command(path.to_str().unwrap())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("OUTBOUND_DISABLED"));
            assert!(error.contains(NPM_REGISTRY_ENV));
            assert!(!error.contains("provide a bundled"));
        }
    }

    #[tokio::test]
    async fn explicit_registry_tool_keeps_path_and_receives_mirror() {
        if !crate::test_support::run_in_env_child(
            "outbound_policy::tests::explicit_registry_tool_keeps_path_and_receives_mirror",
            |command| {
                command.env(NPM_REGISTRY_ENV, "http://127.0.0.1:9/internal/npm/");
            },
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("npx.cmd");
        std::fs::write(&path, "not executed").unwrap();
        let resolved = crate::ensure_runtime_command(path.to_str().unwrap()).await.unwrap();
        assert_eq!(resolved.program, path);
        assert!(
            resolved
                .env
                .contains(&("npm_config_registry".into(), "http://127.0.0.1:9/internal/npm/".into()))
        );
    }
    #[tokio::test]
    async fn user_commands_allow_public_npm_but_implicit_launchers_stay_blocked() {
        if !crate::test_support::run_in_env_child(
            "outbound_policy::tests::user_commands_allow_public_npm_but_implicit_launchers_stay_blocked",
            |command| {
                command.env_remove(NPM_REGISTRY_ENV);
            },
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("npx.cmd");
        std::fs::write(&path, "fixture").unwrap();
        let resolved = crate::ensure_user_runtime_command(path.to_str().unwrap())
            .await
            .unwrap();
        assert!(!resolved.env.iter().any(|(key, _)| key == "npm_config_registry"));
        assert!(resolved.env.contains(&("npm_config_offline".into(), "false".into())));
        for command in ["npm", "npx", "pnpx", "bunx", "uvx"] {
            assert!(
                ensure_implicit_package_command_allowed(command, &[])
                    .unwrap_err()
                    .to_string()
                    .contains("OUTBOUND_DISABLED")
            );
        }
        assert!(
            ensure_implicit_package_command_allowed("node", &["/runtime/npm/bin/npx-cli.js".into()])
                .unwrap_err()
                .to_string()
                .contains("OUTBOUND_DISABLED")
        );
    }
}
