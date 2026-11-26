use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use aionui_api_types::{AcpBuildExtra, AgentManagementStatus};
use aionui_db::{SqliteAgentMetadataRepository, SqliteProviderRepository, init_database_memory};
use aionui_realtime::BroadcastEventBus;
use serde_json::json;

use super::*;
use crate::protocol::acp_relay::{RelayError, is_builtin_aelix};
use crate::services::availability::AgentAvailabilityService;
use crate::studio_aelix::{AGENT_ID, RUNTIME_UNAVAILABLE};

const TEST_CASE: &str = "HOST67_ACP_TEST_CASE";
const TEST_DIR: &str = "HOST67_ACP_TEST_DIR";
const TEST_DESCRIPTOR: &str = "HOST67_ACP_TEST_DESCRIPTOR";
const USER: &str = "system_default_user";

const PEER: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

case = os.environ['HOST67_ACP_TEST_CASE']
root = os.environ['HOST67_ACP_TEST_DIR']
studio_launch = case not in ('command-override', 'custom', 'legacy-builtin')
assert 'AIONUI_STUDIO_AELIX_LAUNCH' not in os.environ
assert sys.argv[1:] == (['acp', '--profile', 'coding'] if studio_launch else [])
if studio_launch:
    assert os.environ['AELIX_CODING_AGENT_DIR'] == os.path.join(root, 'agent')
    assert os.environ['AELIX_ACP_STUDIO_MANAGED'] == '1'
    assert os.environ['AELIX_STUDIO_EXTENSION_STATE'] == os.path.join(root, 'extensions')
assert os.environ.get('HOST67_OVERRIDE') == ('set' if case == 'env-override' else None)

for line in sys.stdin:
    req = json.loads(line)
    if req['method'] == 'initialize':
        meta = req['params'].get('clientCapabilities', {}).get('_meta', {})
        managed = case in ('original', 'moved', 'wrong-name')
        assert bool(meta.get('aelix/sessionRelay')) == managed
        assert meta.get('aelix/sessionLockHolder') == (1 if managed else None)
        result = {
            'protocolVersion': 1,
            'agentInfo': {'name': 'other-acp' if case == 'wrong-name' else 'aelix-acp', 'version': '0.0.0'},
            'agentCapabilities': {'_meta': {'aelix/sessionRelay': {
                'version': 1, 'requests': ['_aelix/session/state'], 'notifications': []}}},
        }
    elif req['method'] == 'session/new':
        result = {'sessionId': 'host67-session'}
        binding = req['params'].get('_meta', {}).get('aelix/sessionRelay')
        if binding is not None:
            result['_meta'] = {'aelix/sessionRelay': binding}
    else:
        continue
    print(json.dumps({'jsonrpc': '2.0', 'id': req['id'], 'result': result}), flush=True)
"#;

#[tokio::test]
async fn studio_launch_and_relay_use_the_seeded_catalog_entry() {
    let dir = tempfile::tempdir().unwrap();
    let test_binary = std::env::current_exe().unwrap();
    for case in [
        "missing",
        "original",
        "moved",
        "manual",
        "command-override",
        "env-override",
        "custom",
        "wrong-name",
        "legacy-builtin",
        "unproven",
        "relative-launcher",
        "missing-launcher",
    ] {
        let workspace = dir.path().join(case);
        std::fs::create_dir(&workspace).unwrap();
        let command = workspace.join("Fixture Runtime").join("acp");
        std::fs::create_dir(command.parent().unwrap()).unwrap();
        std::fs::write(&command, PEER).unwrap();
        std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
        let launcher = command.with_file_name("launch.py");
        if case != "missing-launcher" {
            std::fs::write(&launcher, PEER).unwrap();
        }
        let launcher = if case == "relative-launcher" {
            std::path::PathBuf::from("launch.py")
        } else {
            launcher
        };
        let python = aionui_runtime::resolve_command_path("python3").unwrap();
        let mut child = aionui_runtime::Builder::new(&test_binary);
        child
            .args([
                "--exact",
                concat!(module_path!(), "::studio_launch_child")
                    .split_once("::")
                    .unwrap()
                    .1,
            ])
            .env(TEST_CASE, case)
            .env(TEST_DIR, &workspace)
            .env_remove(TEST_DESCRIPTOR);
        if case != "missing" {
            child.env(
                TEST_DESCRIPTOR,
                json!({
                    "command": python,
                    "args": ["-I", "-S", "-B", "-X", "utf8", launcher, "acp", "--profile", "coding"],
                    "env": {
                        "AELIX_CODING_AGENT_DIR": workspace.join("agent"),
                        "AELIX_ACP_STUDIO_MANAGED": "1",
                        "AELIX_STUDIO_EXTENSION_STATE": workspace.join("extensions")
                    }
                })
                .to_string(),
            );
        }
        let output = tokio::time::timeout(Duration::from_secs(30), child.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child test did not run for {case}"
        );
    }
}

#[tokio::test]
async fn studio_launch_child() {
    let Ok(case) = std::env::var(TEST_CASE) else { return };
    let workspace = std::path::PathBuf::from(std::env::var_os(TEST_DIR).unwrap());
    let cwd = workspace.to_str().unwrap();
    let command = workspace.join("Fixture Runtime").join("acp");
    let db = init_database_memory().await.unwrap();
    let id = match case.as_str() {
        "custom" => {
            sqlx::query("INSERT INTO agent_metadata (id, agent_id, user_id, name, backend, agent_type, agent_source, command, created_at, updated_at) VALUES ('e4cedc22', 'e4cedc22', ?1, 'Custom Aelix', 'aelix', 'acp', 'custom', ?2, 1, 1)")
                .bind(USER).bind(command.to_str().unwrap()).execute(db.pool()).await.unwrap();
            "e4cedc22"
        }
        "legacy-builtin" => {
            sqlx::query("INSERT INTO agent_metadata (id, agent_id, name, backend, agent_type, agent_source, command, created_at, updated_at) VALUES ('legacy-fixture', 'legacy-fixture', 'Old fixture', 'aelix', 'acp', 'builtin', ?1, 1, 1)")
                .bind(command.to_str().unwrap()).execute(db.pool()).await.unwrap();
            "legacy-fixture"
        }
        "command-override" => {
            sqlx::query("UPDATE agent_metadata SET command_override = ? WHERE agent_id = 'builtin-aelix'")
                .bind(command.to_str().unwrap())
                .execute(db.pool())
                .await
                .unwrap();
            AGENT_ID
        }
        "env-override" => {
            sqlx::query("UPDATE agent_metadata SET env_override = '[{\"name\":\"HOST67_OVERRIDE\",\"value\":\"set\"}]' WHERE agent_id = 'builtin-aelix'")
                .execute(db.pool()).await.unwrap();
            AGENT_ID
        }
        "missing" => {
            sqlx::query("UPDATE agent_metadata SET last_check_status = 'online', last_check_kind = 'session' WHERE agent_id = 'builtin-aelix'")
                .execute(db.pool()).await.unwrap();
            AGENT_ID
        }
        _ => AGENT_ID,
    };
    let descriptor = std::env::var(TEST_DESCRIPTOR).ok();
    let registry = AgentRegistry::new_for_studio(
        Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())),
        USER.to_owned(),
        descriptor.as_deref(),
    );
    registry.hydrate().await.unwrap();
    let meta = registry.get_for_user(USER, id).await.unwrap().unwrap();
    let spec = resolve_agent_command_spec(
        &registry,
        &meta,
        USER,
        cwd,
        "studio-acp",
        Arc::new(BroadcastEventBus::new(16)),
    )
    .await;

    if matches!(case.as_str(), "missing" | "relative-launcher" | "missing-launcher") {
        assert!(!meta.available);
        assert!(matches!(spec, Err(AgentError::BadRequest(message)) if message == RUNTIME_UNAVAILABLE));
        let service =
            AgentAvailabilityService::new(registry, Arc::new(SqliteProviderRepository::new(db.pool().clone())));
        let list = service.list_management_rows(USER).await.unwrap();
        let row = list.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.status, AgentManagementStatus::Missing);
        assert!(!row.installed);
        assert_eq!(
            row.last_check_error_code.as_deref(),
            Some("managed_runtime_unavailable")
        );
        let checked = service.run_manual_health_check(USER, id).await.unwrap();
        assert_eq!(checked.status, AgentManagementStatus::Missing);
        assert_eq!(checked.last_check_kind, row.last_check_kind);
        return;
    }

    if case == "manual" {
        let service =
            AgentAvailabilityService::new(registry, Arc::new(SqliteProviderRepository::new(db.pool().clone())));
        let checked = service.run_manual_health_check(USER, id).await.unwrap();
        assert_eq!(
            checked.status,
            AgentManagementStatus::Online,
            "{:?}",
            checked.last_check_error_message
        );
        assert_eq!(
            checked.last_check_kind,
            Some(aionui_api_types::AgentSnapshotCheckKind::Manual)
        );
        return;
    }

    let spec = spec.unwrap();
    let descriptor_path = !matches!(case.as_str(), "command-override" | "custom" | "legacy-builtin");
    let expected = if descriptor_path {
        aionui_runtime::resolve_command_path("python3").unwrap()
    } else {
        command
    };
    assert_eq!(spec.command, expected);
    let provenance = descriptor_path && case != "unproven";
    let candidate = matches!(case.as_str(), "original" | "moved" | "wrong-name");
    assert_eq!(is_builtin_aelix(&meta, provenance), candidate);
    if candidate {
        assert_eq!(meta.id, AGENT_ID);
        assert!(meta.command.is_none());
        assert!(meta.args.is_empty());
        assert!(meta.env.is_empty());
        assert_eq!(
            registry
                .management_row_by_id_for_user(USER, id)
                .await
                .unwrap()
                .unwrap()
                .status,
            AgentManagementStatus::Online
        );
    }
    let mut params = assemble_acp_params(
        format!("studio-acp-{case}"),
        USER.into(),
        WorkspaceInfo {
            path: cwd.into(),
            is_custom: true,
        },
        meta,
        spec,
        AcpBuildExtra::default(),
        vec![],
        None,
        workspace.clone(),
        false,
    )
    .await;
    params.studio_launch = provenance;
    let paths = aionui_extension::resolve_skill_paths(&workspace, &workspace);
    let (manager, _events, _notifications) = AcpAgentManager::build(
        Arc::new(params),
        crate::AcpSkillManager::new(Arc::new(paths)),
        &registry.catalog_sender(),
    )
    .await
    .unwrap();
    manager.warmup_session().await.unwrap();
    assert_eq!(manager.session_id().await.as_deref(), Some("host67-session"));
    let discovery = manager.relay_discovery();
    manager.kill_and_wait(None).await;
    if matches!(case.as_str(), "original" | "moved") {
        assert_eq!(discovery.unwrap().requests, ["_aelix/session/state"]);
    } else {
        assert_eq!(discovery.err(), Some(RelayError::Unavailable));
    }
}
