//! Exercise Host selector reconciliation over stdio with a synthetic ACP peer and SQLite.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aionui_ai_agent::factory::acp_assembler::{WorkspaceInfo, assemble_acp_params};
use aionui_ai_agent::manager::acp::{AcpAgentManager, AcpSessionEvent};
use aionui_ai_agent::registry::AgentRegistry;
use aionui_ai_agent::types::SendMessageData;
use aionui_ai_agent::{AcpBuildExtra, AcpSessionSyncService, AcpSkillManager, AgentInstance, AgentSendError};
use aionui_api_types::{AgentErrorCode, ConfigOptionConfirmation};
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
            'agentCapabilities': {'loadSession': load_surface != 'legacy-no-load'}, 'authMethods': [],
        }
    elif method == 'session/new':
        resumed = params.get('_meta', {}).get('claudeCode', {}).get('options', {}).get('resume')
        session_id = 'replacement-session' if resumed == 'selector-session' else 'selector-session'
        response['result'] = {'sessionId': session_id, 'configOptions': options()}
    elif method == 'session/load' and params['sessionId'] == 'selector-session':
        response['result'] = {'configOptions': options()}
        if load_surface == 'legacy':
            model, *remaining = response['result']['configOptions']
            response['result']['configOptions'] = remaining
            response['result']['models'] = {
                'currentModelId': model['currentValue'],
                'availableModels': [{'modelId': item['value'], 'name': item['name']} for item in model['options']],
            }
    elif method == 'session/load':
        response['error'] = {'code': -32002, 'message': 'Session not found'}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        entry += f' {key}={value}'
        selections[key] = value
        response['result'] = {'configOptions': options()}
    elif method == 'session/prompt' and params['sessionId'] in ('selector-session', 'replacement-session'):
        text = params['prompt'][0]['text']
        entry += ' ' + json.dumps(text)
        body = text.splitlines()[-1]
        if body == 'Close after receiving prompt':
            with open(trace, 'a') as output:
                output.write(entry + '\n')
            sys.exit(0)
        if body in ('Report activity then reject', 'Report activity then busy'):
            print(json.dumps({'jsonrpc': '2.0', 'method': 'session/update', 'params': {
                'sessionId': params['sessionId'], 'update': {'sessionUpdate': 'agent_message_chunk',
                'content': {'type': 'text', 'text': 'Already started'}}}}), flush=True)
        if body in ('Busy before work', 'Report activity then busy'):
            response['error'] = {'code': -32000, 'message': 'Session is busy'}
        elif body in ('Reject before work', 'Report activity then reject'):
            response['error'] = {'code': -32602, 'message': 'Fixture invalid params'}
        else:
            response['result'] = {'stopReason': 'end_turn'}
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
    let (manager, domain_rx) = build_session(directory, registry, sync, agent_name, trace_name, load_surface).await;
    sync.attach(USER.into(), CONVERSATION.into(), domain_rx).await;
    manager.warmup_session().await.unwrap();
    AgentInstance::Acp(Arc::new(manager))
}

async fn build_session(
    directory: &Path,
    registry: &AgentRegistry,
    sync: &AcpSessionSyncService,
    agent_name: &str,
    trace_name: &str,
    load_surface: &str,
) -> (AcpAgentManager, tokio::sync::mpsc::Receiver<AcpSessionEvent>) {
    let mut metadata = registry.find_builtin_by_backend("gemini").await.unwrap();
    metadata.backend = None;
    metadata.handshake = Default::default();
    metadata.behavior_policy.session_load_via_meta_field = load_surface == "meta";
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
            preset_context: Some("Fixture conversation rules".into()),
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
    let manager = manager.with_session_repository(sync.repo());
    if let Some(session_id) = sync.load_session_id(USER, CONVERSATION).await {
        manager.set_session_id(session_id).await;
    }
    (manager, domain_rx)
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

async fn setup() -> (tempfile::TempDir, Arc<AgentRegistry>, Arc<AcpSessionSyncService>) {
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
    (temp, registry, sync)
}

async fn check_load(agent_name: &str, expect_native: bool, load_surface: &str) {
    let (temp, registry, sync) = setup().await;
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

async fn prompt(instance: &AgentInstance, content: &str) -> Result<(), AgentSendError> {
    instance
        .send_message(SendMessageData {
            content: content.into(),
            msg_id: content.into(),
            turn_id: None,
            files: vec![],
            inject_skills: vec![],
        })
        .await
}

fn prompt_trace(directory: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(directory.join(name))
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("session/prompt "))
        .map(|text| serde_json::from_str(text).unwrap())
        .collect()
}

async fn prelude_pending(sync: &AcpSessionSyncService) -> bool {
    sync.repo()
        .get_for_user(USER, CONVERSATION)
        .await
        .unwrap()
        .unwrap()
        .has_pending_aelix_prelude(SESSION)
}

#[tokio::test]
async fn first_slash_commands_reach_acp_verbatim_and_defer_rules_until_ordinary_text() {
    for command in ["/extension install  fixture ", "/extension list", "/compact"] {
        let (temp, registry, sync) = setup().await;
        let agent = open_session(temp.path(), &registry, &sync, "aelix-acp", "wire.txt", "generic").await;

        for input in [command, "/extension list", "Continue", "/compact", "Continue again"] {
            prompt(&agent, input).await.unwrap();
        }

        assert_eq!(
            prompt_trace(temp.path(), "wire.txt"),
            [
                command,
                "/extension list",
                "[Assistant Rules]\nFixture conversation rules\n[/Assistant Rules]\n\nContinue",
                "/compact",
                "Continue again",
            ]
        );
        assert!(!prelude_pending(&sync).await);
        agent.kill_and_wait(None).await;
    }
}

#[tokio::test]
async fn first_ordinary_message_receives_rules_once() {
    let (temp, registry, sync) = setup().await;
    let agent = open_session(temp.path(), &registry, &sync, "aelix-acp", "wire.txt", "generic").await;

    let first = "Example command:\n/extension list";
    for input in [first, "/extension list", "Continue"] {
        prompt(&agent, input).await.unwrap();
    }

    assert_eq!(
        prompt_trace(temp.path(), "wire.txt"),
        [
            "[Assistant Rules]\nFixture conversation rules\n[/Assistant Rules]\n\nExample command:\n/extension list",
            "/extension list",
            "Continue",
        ]
    );
    agent.kill_and_wait(None).await;
}

#[tokio::test]
async fn other_acp_peers_keep_deferred_rules_across_resume_paths() {
    for (surface, session_id) in [
        ("generic", SESSION),
        ("legacy-no-load", SESSION),
        ("meta", "replacement-session"),
    ] {
        let (temp, registry, sync) = setup().await;
        let fresh = open_session(temp.path(), &registry, &sync, "other-acp", "fresh.txt", surface).await;
        prompt(&fresh, "/example").await.unwrap();
        assert_eq!(prompt_trace(temp.path(), "fresh.txt"), ["/example"]);
        assert!(prelude_pending(&sync).await);
        fresh.kill_and_wait(None).await;

        let resumed = open_session(temp.path(), &registry, &sync, "other-acp", "resume.txt", surface).await;
        assert_eq!(
            sync.load_session_id(USER, CONVERSATION).await.as_deref(),
            Some(session_id)
        );
        prompt(&resumed, "Continue").await.unwrap();
        prompt(&resumed, "Continue again").await.unwrap();
        assert_eq!(
            prompt_trace(temp.path(), "resume.txt"),
            [
                "[Assistant Rules]\nFixture conversation rules\n[/Assistant Rules]\n\nContinue",
                "Continue again",
            ]
        );
        resumed.kill_and_wait(None).await;
    }
}

#[tokio::test]
async fn aelix_delivers_the_deferred_prelude_once_after_the_session_is_reopened() {
    let (temp, registry, sync) = setup().await;
    let fresh = open_session(temp.path(), &registry, &sync, "aelix-acp", "compact.txt", "generic").await;
    prompt(&fresh, "/compact").await.unwrap();
    assert_eq!(prompt_trace(temp.path(), "compact.txt"), ["/compact"]);
    assert_selections(&fresh, &sync, "fixture/one", "low", Some("fixture/one")).await;
    fresh.kill_and_wait(None).await;

    let resumed = open_session(temp.path(), &registry, &sync, "aelix-acp", "resume.txt", "generic").await;
    prompt(&resumed, "/compact again").await.unwrap();
    prompt(&resumed, "Continue").await.unwrap();
    let prompts = prompt_trace(temp.path(), "resume.txt");
    assert_eq!(prompts[0], "/compact again");
    assert!(prompts[1].contains("Fixture conversation rules"), "{prompts:?}");
    assert!(prompts[1].ends_with("\n[/Assistant Rules]\n\nContinue"));
    resumed.kill_and_wait(None).await;

    let reopened = open_session(temp.path(), &registry, &sync, "aelix-acp", "reopen.txt", "generic").await;
    prompt(&reopened, "Continue again").await.unwrap();
    assert_eq!(prompt_trace(temp.path(), "reopen.txt"), ["Continue again"]);
    assert_eq!(sync.load_session_id(USER, CONVERSATION).await.as_deref(), Some(SESSION));
    reopened.kill_and_wait(None).await;
}

#[tokio::test]
async fn aelix_keeps_its_deferred_prelude_after_a_confirmed_request_rejection() {
    for (text, code) in [
        ("Reject before work", AgentErrorCode::UserAgentInvalidParams),
        ("Busy before work", AgentErrorCode::UserAgentBusy),
    ] {
        let (temp, registry, sync) = setup().await;
        let first = open_session(temp.path(), &registry, &sync, "aelix-acp", "reject.txt", "generic").await;
        assert_selections(&first, &sync, "fixture/one", "low", Some("fixture/one")).await;
        prompt(&first, "/compact").await.unwrap();
        assert!(prelude_pending(&sync).await);

        let error = prompt(&first, text).await.unwrap_err();
        assert_eq!(error.code(), Some(code));
        assert_eq!(error.stream_error().retryable, Some(false));
        assert!(prelude_pending(&sync).await);
        let prompts = prompt_trace(temp.path(), "reject.txt");
        assert_eq!(prompts.len(), 2, "the rejected request must not be sent again");
        assert!(prompts[1].starts_with("[Assistant Rules]\n"));
        first.kill_and_wait(None).await;

        let reopened = open_session(temp.path(), &registry, &sync, "aelix-acp", "restored.txt", "generic").await;
        prompt(&reopened, "Accepted request").await.unwrap();
        prompt(&reopened, "Independent next request").await.unwrap();
        let prompts = prompt_trace(temp.path(), "restored.txt");
        assert!(prompts[0].starts_with("[Assistant Rules]\n"));
        assert!(prompts[0].ends_with("\n[/Assistant Rules]\n\nAccepted request"));
        assert_eq!(prompts[1], "Independent next request");
        assert!(!prelude_pending(&sync).await);
        reopened.kill_and_wait(None).await;
    }
}

#[tokio::test]
async fn aelix_does_not_restore_a_deferred_prelude_after_activity_or_transport_loss() {
    for (text, code) in [
        ("Report activity then reject", AgentErrorCode::UserAgentInvalidParams),
        ("Report activity then busy", AgentErrorCode::UserAgentBusy),
        (
            "Close after receiving prompt",
            AgentErrorCode::UserAgentPromptOutcomeUnknown,
        ),
    ] {
        let (temp, registry, sync) = setup().await;
        let first = open_session(temp.path(), &registry, &sync, "aelix-acp", "uncertain.txt", "generic").await;
        assert_selections(&first, &sync, "fixture/one", "low", Some("fixture/one")).await;
        prompt(&first, "/compact").await.unwrap();
        assert!(prelude_pending(&sync).await);

        let error = prompt(&first, text).await.unwrap_err();
        assert_eq!(error.code(), Some(code));
        assert_eq!(error.stream_error().retryable, Some(false));
        assert!(!prelude_pending(&sync).await, "{text}");
        assert_eq!(prompt_trace(temp.path(), "uncertain.txt").len(), 2, "{text}");
        first.kill_and_wait(None).await;

        let reopened = open_session(temp.path(), &registry, &sync, "aelix-acp", "new-turn.txt", "generic").await;
        prompt(&reopened, "Independent next request").await.unwrap();
        assert_eq!(prompt_trace(temp.path(), "new-turn.txt"), ["Independent next request"]);
        reopened.kill_and_wait(None).await;
    }
}

#[tokio::test]
async fn aelix_defers_and_sends_in_the_same_session_when_assignment_persistence_is_delayed() {
    let (temp, registry, sync) = setup().await;
    let (manager, domain_rx) =
        build_session(temp.path(), &registry, &sync, "aelix-acp", "delayed.txt", "generic").await;
    manager.warmup_session().await.unwrap();
    let instance = AgentInstance::Acp(Arc::new(manager));

    // Hold SessionAssigned in the receiver until both turns have finished.
    assert_eq!(sync.load_session_id(USER, CONVERSATION).await, None);
    prompt(&instance, "/compact").await.unwrap();
    assert_eq!(sync.load_session_id(USER, CONVERSATION).await.as_deref(), Some(SESSION));
    assert!(prelude_pending(&sync).await);
    prompt(&instance, "First ordinary input").await.unwrap();
    let prompts = prompt_trace(temp.path(), "delayed.txt");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0], "/compact");
    assert!(prompts[1].ends_with("\n[/Assistant Rules]\n\nFirst ordinary input"));
    assert!(!prelude_pending(&sync).await);

    sync.attach(USER.into(), CONVERSATION.into(), domain_rx).await;
    assert_selections(&instance, &sync, "fixture/one", "low", Some("fixture/one")).await;
    assert_eq!(sync.load_session_id(USER, CONVERSATION).await.as_deref(), Some(SESSION));
    assert!(!prelude_pending(&sync).await);
    instance.kill_and_wait(None).await;
}

#[tokio::test]
async fn first_command_after_a_stale_session_defers_rules_before_assignment_is_persisted() {
    for clear_binding in [false, true] {
        let (temp, registry, sync) = setup().await;
        let repo = sync.repo();
        assert!(
            repo.update_aelix_prelude_for_user(
                USER,
                CONVERSATION,
                "stale-session",
                aionui_db::AelixPreludeChange::Defer,
            )
            .await
            .unwrap()
        );
        if clear_binding {
            assert!(repo.clear_session_id_for_user(USER, CONVERSATION).await.unwrap());
        }

        let (manager, _domain_rx) =
            build_session(temp.path(), &registry, &sync, "aelix-acp", "stale.txt", "generic").await;
        manager.warmup_session().await.unwrap();
        let agent = AgentInstance::Acp(Arc::new(manager));

        let expected_binding = if clear_binding { None } else { Some(SESSION) };
        assert_eq!(
            sync.load_session_id(USER, CONVERSATION).await.as_deref(),
            expected_binding
        );
        prompt(&agent, "/extension list").await.unwrap();
        assert!(prelude_pending(&sync).await);
        prompt(&agent, "Continue").await.unwrap();
        assert_eq!(
            prompt_trace(temp.path(), "stale.txt"),
            [
                "/extension list",
                "[Assistant Rules]\nFixture conversation rules\n[/Assistant Rules]\n\nContinue",
            ]
        );
        assert!(!prelude_pending(&sync).await);
        agent.kill_and_wait(None).await;
    }
}

#[tokio::test]
async fn aelix_does_not_dispatch_or_auto_replay_when_the_prelude_binding_really_changes() {
    let (temp, registry, sync) = setup().await;
    let instance = open_session(temp.path(), &registry, &sync, "aelix-acp", "rebound.txt", "generic").await;
    assert_selections(&instance, &sync, "fixture/one", "low", Some("fixture/one")).await;
    prompt(&instance, "/compact").await.unwrap();
    assert!(prelude_pending(&sync).await);
    assert!(
        sync.repo()
            .update_session_id_for_user(USER, CONVERSATION, "replacement")
            .await
            .unwrap()
    );

    let error = prompt(&instance, "Must stay in the original session")
        .await
        .unwrap_err();
    assert_eq!(error.stream_error().retryable, Some(false));
    assert_eq!(
        error.stream_error().detail.as_deref(),
        Some("The Aelix session binding changed")
    );
    assert_eq!(prompt_trace(temp.path(), "rebound.txt"), ["/compact"]);
    assert_eq!(
        sync.load_session_id(USER, CONVERSATION).await.as_deref(),
        Some("replacement")
    );
    assert!(!prelude_pending(&sync).await);
    instance.kill_and_wait(None).await;
}
