//! Exercise Host selector reconciliation over stdio with a synthetic ACP peer and SQLite.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use aionui_ai_agent::manager::acp::AcpAgentManager;
use aionui_ai_agent::registry::AgentRegistry;
use aionui_ai_agent::{AcpBuildExtra, AcpSessionSyncService, AcpSkillManager, AgentInstance};
use aionui_api_types::ConfigOptionConfirmation;
use aionui_common::CommandSpec;
use aionui_db::{
    CreateAcpSessionParams, IAcpSessionRepository, SqliteAcpSessionRepository, SqliteAgentMetadataRepository,
    init_database_memory,
};

const PEER: &str = r#"
import json
import sys

agent_name, trace, load_surface = sys.argv[1:]
selections = {'model': 'fixture/two', 'thinking_level': 'high'}

def options():
    return [
        {'id': key, 'name': key, 'category': category, 'type': 'select',
         'currentValue': selections[key],
         'options': [{'value': value, 'name': value} for value in choices]}
        for key, category, choices in (
            ('model', 'model', ('fixture/one', 'fixture/two')),
            ('thinking_level', 'thought_level', ('low', 'high')),
        )
    ]

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    params = request.get('params', {})
    entry = method
    response = {'jsonrpc': '2.0', 'id': request['id']}
    if method == 'initialize':
        response['result'] = {
            'protocolVersion': 1, 'agentInfo': {'name': agent_name, 'version': '0.0.0'},
            'agentCapabilities': {'loadSession': True}, 'authMethods': [],
        }
    elif method == 'session/new':
        response['result'] = {'sessionId': 'selector-session', 'configOptions': options()}
    elif method == 'session/load' and params['sessionId'] == 'selector-session':
        response['result'] = {'configOptions': options()}
        if load_surface == 'legacy':
            model, *remaining = response['result']['configOptions']
            response['result']['configOptions'] = remaining
            response['result']['models'] = {
                'currentModelId': model['currentValue'],
                'availableModels': [{'modelId': item['value'], 'name': item['name']} for item in model['options']],
            }
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        entry += f' {key}={value}'
        selections[key] = value
        response['result'] = {'configOptions': options()}
    else:
        response['error'] = {'code': -32601, 'message': f'Unexpected request: {method}'}
    with open(trace, 'a') as output:
        output.write(entry + '\n')
    print(json.dumps(response), flush=True)
"#;

const USER: &str = "selector-user";
const CONVERSATION: &str = "selector-conv";
const SESSION: &str = "selector-session";
const SAVED_WRITES: [&str; 2] = [
    "session/set_config_option model=fixture/one",
    "session/set_config_option thinking_level=low",
];

async fn open_session(
    directory: &Path,
    registry: &AgentRegistry,
    sync: &AcpSessionSyncService,
    agent_name: &str,
    trace_name: &str,
    load_surface: &str,
) -> AgentInstance {
    let mut metadata = registry.find_builtin_by_backend("gemini").await.unwrap();
    metadata.backend = None;
    metadata.handshake = Default::default();
    let params = assemble_acp_params(
        CONVERSATION.into(),
        USER.into(),
        WorkspaceInfo {
            path: directory.to_string_lossy().into_owned(),
            is_custom: true,
        },
        metadata,
        CommandSpec {
            command: which::which("python3").expect("synthetic ACP integration test requires python3"),
            args: vec![
                directory.join("peer.py").to_string_lossy().into_owned(),
                agent_name.into(),
                directory.join(trace_name).to_string_lossy().into_owned(),
                load_surface.into(),
            ],
            env: vec![],
            cwd: None,
        },
        AcpBuildExtra {
            current_model_id: Some("fixture/one".into()),
            thought_level: Some("low".into()),
            ..Default::default()
        },
        vec![],
        sync.load_snapshot_state(USER, CONVERSATION).await,
        directory.to_path_buf(),
        false,
    )
    .await;
    let skills = Arc::new(aionui_extension::resolve_skill_paths(directory, directory));
    let (manager, domain_rx, _notifications) = AcpAgentManager::build(
        Arc::new(params),
        AcpSkillManager::new(skills),
        &registry.catalog_sender(),
    )
    .await
    .unwrap();
    if let Some(session_id) = sync.load_session_id(USER, CONVERSATION).await {
        manager.set_session_id(session_id).await;
    }
    sync.attach(USER.into(), CONVERSATION.into(), domain_rx).await;
    manager.warmup_session().await.unwrap();
    AgentInstance::Acp(Arc::new(manager))
}

async fn assert_selections(
    instance: &AgentInstance,
    sync: &AcpSessionSyncService,
    model: &str,
    thought: &str,
    stored_model_option: Option<&str>,
) {
    let options = instance.get_config_options().await.unwrap().config_options;
    for (id, current) in [("model", model), ("thinking_level", thought)] {
        let option = options.iter().find(|option| option.id == id).unwrap();
        assert_eq!(option.current_value.as_deref(), Some(current), "{id}");
    }
    let model_info = instance.get_model().await.unwrap().model_info.unwrap();
    assert_eq!(model_info.current_model_id.as_deref(), Some(model));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = sync.load_snapshot_state(USER, CONVERSATION).await.unwrap();
            let selections = &snapshot.config_selections;
            if snapshot.current_model_id.as_ref().map(|value| value.as_str()) == Some(model)
                && selections
                    .iter()
                    .any(|(key, value)| key.as_str() == "thinking_level" && value.as_str() == thought)
            {
                let model_option = selections
                    .iter()
                    .find(|(key, _)| matches!(key.as_str(), "model" | "models"))
                    .map(|(_, value)| value.as_str());
                assert_eq!(model_option, stored_model_option, "stored model selector");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("displayed model and thought selections must reach SQLite");
}

async fn check_load(agent_name: &str, expect_native: bool, load_surface: &str) {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("peer.py"), PEER).unwrap();
    let db = init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
         VALUES (?1, 'local', ?1, 'hash', 'active', 0, 1, 1)",
    )
    .bind(USER)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, pinned, created_at, updated_at) \
         VALUES (?1, ?2, 'Selectors', 'acp', '{}', 'pending', 0, 1, 1)",
    )
    .bind(CONVERSATION)
    .bind(USER)
    .execute(db.pool())
    .await
    .unwrap();
    let registry = AgentRegistry::new(Arc::new(SqliteAgentMetadataRepository::new(db.pool().clone())));
    registry.hydrate().await.unwrap();
    let metadata = registry.find_builtin_by_backend("gemini").await.unwrap();
    let repo = Arc::new(SqliteAcpSessionRepository::new(db.pool().clone()));
    repo.create(&CreateAcpSessionParams {
        user_id: USER,
        conversation_id: CONVERSATION,
        agent_source: "builtin",
        agent_id: &metadata.id,
    })
    .await
    .unwrap();
    let sync = AcpSessionSyncService::new(repo);

    let fresh = open_session(temp.path(), &registry, &sync, agent_name, "new.txt", load_surface).await;
    assert_selections(&fresh, &sync, "fixture/one", "low", Some("fixture/one")).await;
    fresh.kill_and_wait(None).await;
    assert_eq!(sync.load_session_id(USER, CONVERSATION).await.as_deref(), Some(SESSION));
    assert_trace(temp.path(), "new.txt", "session/new", &SAVED_WRITES);

    // The new peer process starts with the newer state, simulating a native edit
    // between closing and reopening the same session.
    let resumed = open_session(temp.path(), &registry, &sync, agent_name, "load.txt", load_surface).await;
    if expect_native {
        assert_trace(temp.path(), "load.txt", "session/load", &[]);
        let stored_model_option = (load_surface == "generic").then_some("fixture/two");
        assert_selections(&resumed, &sync, "fixture/two", "high", stored_model_option).await;

        if load_surface == "legacy" {
            resumed.kill_and_wait(None).await;
            assert_trace(temp.path(), "load.txt", "session/load", &[]);
            return;
        }
        for (key, value) in [("model", "fixture/one"), ("thinking_level", "low")] {
            let changed = resumed.set_config_option(key, value).await.unwrap();
            assert_eq!(changed.confirmation, ConfigOptionConfirmation::Observed);
        }
        assert_selections(&resumed, &sync, "fixture/one", "low", Some("fixture/one")).await;
    } else {
        assert_selections(&resumed, &sync, "fixture/one", "low", Some("fixture/one")).await;
    }
    resumed.kill_and_wait(None).await;
    assert_trace(temp.path(), "load.txt", "session/load", &SAVED_WRITES);
}

fn assert_trace(directory: &Path, name: &str, open_method: &str, expected_writes: &[&str]) {
    let trace = std::fs::read_to_string(directory.join(name)).unwrap();
    let mut lines = trace.lines();
    assert_eq!(lines.next(), Some("initialize"));
    assert_eq!(lines.next(), Some(open_method));
    let mut writes: Vec<_> = lines.collect();
    writes.sort_unstable();
    assert_eq!(writes, expected_writes, "{name}");
}

#[tokio::test]
async fn aelix_load_adopts_native_selections_before_a_later_explicit_change() {
    tokio::time::timeout(Duration::from_secs(30), check_load("aelix-acp", true, "generic"))
        .await
        .expect("Aelix selector flow timed out");
}

#[tokio::test]
async fn another_agent_still_reconciles_saved_selections_on_load() {
    tokio::time::timeout(Duration::from_secs(30), check_load("other-acp", false, "generic"))
        .await
        .expect("non-Aelix selector flow timed out");
}

#[tokio::test]
async fn aelix_legacy_model_load_clears_the_saved_generic_model_selection() {
    tokio::time::timeout(Duration::from_secs(30), check_load("aelix-acp", true, "legacy"))
        .await
        .expect("Aelix legacy model flow timed out");
}
