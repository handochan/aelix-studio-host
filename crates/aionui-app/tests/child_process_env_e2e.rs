#![cfg(any(unix, windows))]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::terminal::{CreateTerminalParams, TerminalRegistry};
use aionui_common::host_env::HostEnv;
use aionui_extension::{
    lifecycle::{HookKind, execute_hook},
    template::resolve_env_templates,
};
use aionui_mcp::connection_test::McpConnectionTestService;
use aionui_mcp::types::McpServerTransport;
use aionui_realtime::BroadcastEventBus;
use aionui_runtime::{Builder, ResolvedCommand};

const HOST_FIXTURE: &str = "HOST78_ENV_FIXTURE";
const USER_VAR: &str = "HOST78_USER_VAR";

fn run_in_host_env(test: &str) -> bool {
    if std::env::var_os(HOST_FIXTURE).is_some() {
        assert!(
            HostEnv::ALL
                .iter()
                .all(|setting| std::env::var(setting.name()).as_deref() == Ok("host sentinel"))
        );
        return true;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--nocapture"]).env(HOST_FIXTURE, "1");
    command.envs(HostEnv::ALL.iter().map(|setting| {
        let name = setting.name();
        // Windows environment keys are case-insensitive.
        let name = if cfg!(windows) {
            name.to_ascii_lowercase()
        } else {
            name.into()
        };
        (name, "host sentinel")
    }));
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{test}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn python() -> PathBuf {
    aionui_runtime::resolve_command_path(if cfg!(windows) { "python" } else { "python3" })
        .expect("Python for the subprocess fixture")
}

fn env_check() -> String {
    let names: Vec<_> = HostEnv::ALL.iter().map(|setting| setting.name()).collect();
    format!("import os\nleaked = [name for name in {names:?} if name in os.environ]\nassert not leaked, leaked\n")
}

fn overrides() -> Vec<(String, String)> {
    vec![
        (HostEnv::JwtSecret.name().into(), "explicit sentinel".into()),
        (USER_VAR.into(), "from config".into()),
    ]
}

#[tokio::test]
async fn terminal_filters_host_env() {
    if !run_in_host_env("terminal_filters_host_env") {
        return;
    }
    let registry = TerminalRegistry::new("env-test", None);
    let id = registry
        .create(CreateTerminalParams {
            command: python().to_str().unwrap().into(),
            args: vec![
                "-c".into(),
                format!("{}assert os.environ[{USER_VAR:?}] == 'from config'", env_check()),
            ],
            env: overrides(),
            cwd: None,
            output_byte_limit: None,
        })
        .await
        .unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(10), registry.wait_for_exit(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(exit.exit_code, Some(0), "{:?}", registry.output(&id).await);
}

#[tokio::test]
async fn mcp_stdio_probe_filters_host_env() {
    if !run_in_host_env("mcp_stdio_probe_filters_host_env") {
        return;
    }
    let script = format!(
        "{}assert os.environ[{USER_VAR:?}] == 'from config'\n{}",
        env_check(),
        r#"import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    result = {'tools': []} if req['method'] == 'tools/list' else {
        'protocolVersion': '2024-11-05', 'capabilities': {},
        'serverInfo': {'name': 'env-check', 'version': '1'}
    }
    print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'result': result}), flush=True)"#
    );
    let transport = McpServerTransport::Stdio {
        command: python().to_str().unwrap().into(),
        args: vec!["-c".into(), script],
        env: HashMap::from_iter(overrides()),
    };
    let service = McpConnectionTestService::new(reqwest::Client::new(), Arc::new(BroadcastEventBus::new(16)));
    let result = service.test_connection("env-check", &transport).await;
    assert!(result.success, "{result:?}");
}

#[tokio::test]
async fn vendored_mcp_stdio_filters_host_env() {
    use aion_mcp::{
        protocol::JsonRpcRequest,
        transport::{McpTransport, stdio::StdioTransport},
    };

    if !run_in_host_env("vendored_mcp_stdio_filters_host_env") {
        return;
    }

    let script = format!(
        "{}assert os.environ[{USER_VAR:?}] == 'from config'\n{}",
        env_check(),
        r#"import sys, json
request = json.loads(sys.stdin.readline())
print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{}}), flush=True)"#
    );
    let transport = StdioTransport::spawn(
        python().to_str().unwrap(),
        &["-c".into(), script],
        &HashMap::from_iter(overrides()),
    )
    .await
    .unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(10),
        transport.request(&JsonRpcRequest::new(1, "initialize", None)),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(reply.result.is_some());
}

#[tokio::test]
async fn utility_and_extension_commands_filter_host_env() {
    if !run_in_host_env("utility_and_extension_commands_filter_host_env") {
        return;
    }
    let script = env_check();
    let mut probe = Builder::clean_cli(python());
    probe.args(["-c", &script]);
    let output = probe.output().await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let resolved = ResolvedCommand {
        program: python(),
        args_prefix: vec!["-c".into(), script.clone().into()],
        env: overrides()
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect(),
    };
    let output = Builder::from_resolved(&resolved).output().await.unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let dir = tempfile::tempdir().unwrap();
    assert_eq!(resolve_env_templates("${JWT_SECRET}", false).unwrap(), "");
    std::fs::write(dir.path().join("check.py"), script).unwrap();
    #[cfg(unix)]
    let (hook, body) = (
        "check.sh",
        format!("#!/bin/sh\nexec \"{}\" check.py\n", python().display()),
    );
    #[cfg(windows)]
    let (hook, body) = ("check.cmd", format!("@\"{}\" check.py\r\n", python().display()));
    let hook_path = dir.path().join(hook);
    std::fs::write(&hook_path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    execute_hook(dir.path(), hook, HookKind::OnInstall, "env-check")
        .await
        .unwrap();
}
