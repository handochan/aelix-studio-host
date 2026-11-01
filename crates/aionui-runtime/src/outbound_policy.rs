//! Explicit opt-in sources for host-managed downloads. No public fallback.
use std::ffi::OsString;

use crate::node_runtime::NodeRuntimeError;

pub const NODE_MIRROR_ENV: &str = "AELIX_STUDIO_HOST_NODE_MIRROR";
pub const NPM_REGISTRY_ENV: &str = "AELIX_STUDIO_HOST_NPM_REGISTRY";

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
        NodeRuntimeError::managed_invalid(format!(
            "OUTBOUND_DISABLED: {key} is not configured; provide a bundled runtime or configure an internal mirror"
        ))
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
    let registry = configured_url(NPM_REGISTRY_ENV)?;
    Ok([
        ("npm_config_registry", registry.as_str()),
        ("npm_config_offline", "false"),
        ("npm_config_replace_registry_host", "always"),
        ("npm_config_update_notifier", "false"),
        ("npm_config_audit", "false"),
        ("npm_config_fund", "false"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect())
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
}
