//! Host resume/reconcile/persistence regression with a synthetic ACP peer.
//! This exercises real stdio and fresh child processes, not a real agent CLI
//! or Studio installer. The peer deliberately rejects automatic policy writes.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use aionui_ai_agent::manager::acp::AcpAgentManager;
use aionui_ai_agent::registry::AgentRegistry;
use aionui_ai_agent::{AcpBuildExtra, AcpSessionSyncService, AcpSkillManager, AgentInstance};
use aionui_common::CommandSpec;
use aionui_db::{
    CreateAcpSessionParams, IAcpSessionRepository, SaveRuntimeStateParams, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, init_database_memory,
};

const PEER: &str = r#"
import json
import sys

mode, surface, trace = sys.argv[1:]
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if 'id' not in request or method is None:
        continue
    with open(trace, 'a') as output:
        output.write(method + '\n')
    response = {'jsonrpc': '2.0', 'id': request['id']}
    if method == 'initialize':
        response['result'] = {
            'protocolVersion': 1,
            'agentCapabilities': {'loadSession': True},
            'authMethods': [],
        }
    elif method == 'session/load':
        modes = {
            'currentModeId': mode,
            'availableModes': [{'id': 'default', 'name': 'Ask'}, {'id': 'yolo', 'name': 'Yolo'}],
        }
        response['result'] = {'modes': modes}
        if surface == 'generic':
            # An explicit generic permission takes precedence over stale legacy modes.
            modes['currentModeId'] = 'yolo' if mode == 'default' else 'default'
            response['result']['configOptions'] = [{
                'id': 'permissionLevel', 'name': 'Permission', 'category': 'mode',
                'type': 'select', 'currentValue': mode,
                'options': [{'value': 'default', 'name': 'Ask'}, {'value': 'yolo', 'name': 'Yolo'}],
            }]
    else:
        response['error'] = {'code': -32601, 'message': 'Unexpected policy write or fresh session'}
    print(json.dumps(response), flush=True)
"#;

async fn check_restore(surface: &str, saved: &str, restored: &str) {
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("peer.py");
    let trace = temp.path().join("methods.txt");
    std::fs::write(&script, PEER).unwrap();
    let python = which::which("python3").expect("synthetic ACP integration test requires python3");
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
         VALUES ('restore-user', 'local', 'restore-user', 'hash', 'active', 0, 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
         VALUES ('restore-conv', 'restore-user', 'Restore', 'acp', '{}', 'pending', 0, 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let registry = AgentRegistry::new(Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())));
    registry.hydrate().await.unwrap();
    let mut metadata = registry.find_builtin_by_backend("gemini").await.unwrap();
    metadata.backend = None; // Exercise standard ACP load without vendor-specific resume behavior.
    metadata.handshake = Default::default();
    let repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
    repo.create(&CreateAcpSessionParams {
        user_id: "restore-user",
        conversation_id: "restore-conv",
        agent_source: "builtin",
        agent_id: &metadata.id,
    })
    .await
    .unwrap();
    repo.update_session_id_for_user("restore-user", "restore-conv", "restored-session")
        .await
        .unwrap();
    let config_key = if surface == "generic" {
        "permissionLevel"
    } else {
        "mode"
    };
    let saved_config = serde_json::json!({config_key: saved}).to_string();
    repo.save_runtime_state_for_user(
        "restore-user",
        "restore-conv",
        &SaveRuntimeStateParams {
            current_mode_id: Some(Some(saved)),
            config_selections_json: Some(Some(&saved_config)),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let sync = AcpSessionSyncService::new(repo);

    // Rebuild twice from the persisted DB state, with a fresh OS child each time.
    for _ in 0..2 {
        let snapshot = sync.load_snapshot_state("restore-user", "restore-conv").await;
        let params = assemble_acp_params(
            "restore-conv".into(),
            "restore-user".into(),
            WorkspaceInfo {
                path: temp.path().to_string_lossy().into_owned(),
                is_custom: true,
            },
            metadata.clone(),
            CommandSpec {
                command: python.clone(),
                args: vec![
                    script.to_string_lossy().into_owned(),
                    restored.into(),
                    surface.into(),
                    trace.to_string_lossy().into_owned(),
                ],
                env: vec![],
                cwd: None,
            },
            AcpBuildExtra {
                session_mode: Some(saved.into()),
                ..Default::default()
            },
            vec![],
            snapshot,
            temp.path().to_path_buf(),
            false,
        )
        .await;
        let skill_paths = Arc::new(aionui_extension::resolve_skill_paths(temp.path(), temp.path()));
        let (manager, domain_rx, _notifications) = AcpAgentManager::build(
            Arc::new(params),
            AcpSkillManager::new(skill_paths),
            &registry.catalog_sender(),
        )
        .await
        .unwrap();
        manager
            .set_session_id(sync.load_session_id("restore-user", "restore-conv").await.unwrap())
            .await;
        sync.attach("restore-user".into(), "restore-conv".into(), domain_rx)
            .await;
        manager.warmup_session().await.unwrap();
        let instance = AgentInstance::Acp(Arc::new(manager));
        assert_eq!(instance.get_mode().await.unwrap().mode, restored);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = sync.load_snapshot_state("restore-user", "restore-conv").await.unwrap();
                let expected_config = if surface == "generic" {
                    serde_json::json!({config_key: restored})
                } else {
                    serde_json::json!({})
                };
                let config = serde_json::to_value(
                    snapshot
                        .config_selections
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str()))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )
                .unwrap();
                if snapshot.current_mode_id.as_ref().map(|mode| mode.as_str()) == Some(restored)
                    && config == expected_config
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("restored mode and cleared/replaced stale config must reach SQLite");
        instance.kill_and_wait(None).await;
    }
    assert_eq!(
        std::fs::read_to_string(trace).unwrap(),
        "initialize\nsession/load\ninitialize\nsession/load\n",
        "resume must neither create a fresh session nor replay the saved permission"
    );
}

#[tokio::test]
async fn legacy_permissions_survive_restore_and_process_rebuild() {
    for (saved, restored) in [("yolo", "default"), ("default", "yolo")] {
        tokio::time::timeout(Duration::from_secs(30), check_restore("legacy", saved, restored))
            .await
            .expect("legacy restore timed out");
    }
}

#[tokio::test]
async fn generic_permissions_survive_restore_and_process_rebuild() {
    for (saved, restored) in [("yolo", "default"), ("default", "yolo")] {
        tokio::time::timeout(Duration::from_secs(30), check_restore("generic", saved, restored))
            .await
            .expect("generic restore timed out");
    }
}
