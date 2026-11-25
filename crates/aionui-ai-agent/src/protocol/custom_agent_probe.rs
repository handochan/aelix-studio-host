//! Two-step probe for custom ACP agents.
//!
//! Step 1: `which`/`where` — resolve the first token of `command` on
//!         `$PATH`. Bounded by `execFileSync`-equivalent 5 s timeout.
//! Step 2: Spawn the CLI via `CliAgentProcess::spawn_for_sdk`, connect
//!         an `AcpProtocol` (which owns the ACP `initialize` handshake
//!         with a built-in 30 s timeout), then shut down cleanly.
//!
//! The same function is called by:
//!   - `POST /api/agents/custom/try-connect`  (manual "test connection" button)
//!   - `AgentService::create/update_custom_agent`   (test-on-save)
//!
//! Registration projects failures into a safe HTTP error.

use std::collections::HashMap;
use std::time::Duration;

use aionui_api_types::{AgentHandshake, TryConnectCustomAgentResponse};
use aionui_common::{CommandSpec, EnvVar};

use crate::protocol::acp_init_budget::{INIT_TIMEOUT_SECS, InitBudget};
use aionui_runtime::{NodeRuntimeProgressReporter, ResolvedCommand};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::capability::cli_process::CliAgentProcess;
use crate::protocol::acp::AcpProtocol;
use crate::protocol::error::AcpError;

use agent_client_protocol::schema::v1::NewSessionRequest;

/// Step 2 overall timeout. Belt-and-suspenders: `AcpProtocol::connect`
/// already caps the initialize RPC at 30 s, but a CLI that hangs
/// before writing any ACP frame at all is covered by this outer cap.
const STEP2_TIMEOUT: Duration = Duration::from_secs(35);

/// Grace period for the child to exit on its own after stdin close, before
/// we fall back to SIGKILL on the whole process group. Keep this short because
/// manual connection tests should return promptly after the ACP probe finishes.
const PROBE_KILL_GRACE: Duration = Duration::from_millis(500);

/// Probe a custom ACP agent.
///
/// Returns `Success` only if both `which` and the ACP `initialize`
/// handshake succeed. Any failure short-circuits into the
/// corresponding variant.
pub async fn try_connect_custom_agent(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> TryConnectCustomAgentResponse {
    try_connect_custom_agent_with_catalog(command, args, env, reporter)
        .await
        .0
}

/// As [`try_connect_custom_agent`], plus the catalog the probe's `session/new`
/// advertised. The probe already pays for a full spawn + `initialize` +
/// `session/new`; callers that persist agent metadata use this variant so that
/// data is stored instead of thrown away. Auth-gated or failed probes do not
/// provide a catalog.
pub async fn try_connect_custom_agent_with_catalog(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
    probe_custom_agent(command, args, env, reporter).await.into_response()
}

pub(crate) async fn probe_custom_agent(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
) -> ProbeOutcome {
    try_connect_with_policy(command, args, env, reporter, true, false).await
}

pub(crate) async fn try_connect_builtin_agent_with_catalog(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
    private: bool,
) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
    try_connect_with_policy(command, args, env, reporter, false, private)
        .await
        .into_response()
}

async fn try_connect_with_policy(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    reporter: Option<&dyn NodeRuntimeProgressReporter>,
    user_configured: bool,
    private: bool,
) -> ProbeOutcome {
    // ── Step 1 — which check ────────────────────────────────────────
    let head = first_token(command);
    let plan = if user_configured {
        aionui_runtime::ensure_user_runtime_command_with_args(head, args, reporter).await
    } else {
        aionui_runtime::ensure_implicit_runtime_command_with_args(head, args, reporter).await
    };
    let resolved = match plan {
        Ok(resolved) => resolved,
        Err(error) => {
            return ProbeOutcome::CliUnavailable(if private {
                crate::studio_aelix::probe_failure_message("command_not_found").into()
            } else {
                error.to_string()
            });
        }
    };
    if !private {
        debug!(program = %resolved.program.display(), "probe step 1 ok");
    }

    // ── Step 2 — spawn + ACP initialize ─────────────────────────────
    let spec = probe_command_spec(resolved, args, env);
    probe_resolved(spec, private).await
}

pub(crate) async fn try_connect_resolved(
    spec: CommandSpec,
    redact_launch: bool,
) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
    probe_resolved(spec, redact_launch).await.into_response()
}

async fn probe_resolved(spec: CommandSpec, redact_launch: bool) -> ProbeOutcome {
    let spawned = if redact_launch {
        CliAgentProcess::spawn_sdk(spec, false, true).await
    } else {
        CliAgentProcess::spawn_for_sdk(spec).await
    };
    let proc = match spawned {
        Ok(proc) => proc,
        Err(error) => return ProbeOutcome::Fail(format!("spawn failed: {error}")),
    };

    let outcome = match tokio::time::timeout(STEP2_TIMEOUT, run_handshake(&proc, redact_launch)).await {
        Ok(outcome) => outcome,
        Err(_) => ProbeOutcome::Fail(format!(
            "ACP handshake did not complete within {}s",
            STEP2_TIMEOUT.as_secs()
        )),
    };

    // Always tear down the whole process group. `kill_on_drop(true)` only
    // signals the direct child (e.g. `npm exec ...`) — wrapper CLIs spawn
    // grandchildren (`openclaw-acp`) that survive unless we SIGKILL the
    // group explicitly via `proc.kill()`.
    if let Err(error) = proc.kill(PROBE_KILL_GRACE).await {
        warn!(pid = proc.pid(), error = %error, "probe failed to kill process group");
    }

    outcome
}

fn first_token(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

fn probe_command_spec(resolved: ResolvedCommand, args: &[String], env: &HashMap<String, String>) -> CommandSpec {
    let mut final_args: Vec<String> = resolved
        .args_prefix
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    final_args.extend(args.iter().cloned());

    let mut final_env: Vec<EnvVar> = env
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: value.clone(),
        })
        .collect();
    final_env.extend(resolved.env.iter().map(|(name, value)| EnvVar {
        name: name.to_string_lossy().into_owned(),
        value: value.to_string_lossy().into_owned(),
    }));

    CommandSpec {
        command: resolved.program,
        args: final_args,
        env: final_env,
        cwd: Some(std::env::temp_dir().to_string_lossy().into_owned()),
    }
}

/// Result of the Step 2 probe (`initialize` + `session/new`).
///
/// The probe reaches `session/new` so it can tell "reachable but not
/// authorized" (`Auth`) apart from other ACP failures (`Fail`) — `initialize`
/// alone returns `authMethods` even for already-authorized agents and cannot
/// make this distinction.
pub(crate) enum ProbeOutcome {
    /// Carries the catalog the successful `session/new` advertised (modes /
    /// models / config options), already projected into the same shape the live
    /// session path persists. `None` when the agent advertised nothing. Boxed to
    /// keep the enum small: the success payload dwarfs the error strings.
    Ok(Option<Box<AgentHandshake>>),
    Auth(String),
    CliUnavailable(String),
    Fail(String),
    RpcFailure {
        method: &'static str,
        error: AcpError,
    },
}

impl ProbeOutcome {
    fn into_response(self) -> (TryConnectCustomAgentResponse, Option<Box<AgentHandshake>>) {
        let response = match self {
            Self::Ok(catalog) => return (TryConnectCustomAgentResponse::Success, catalog),
            Self::Auth(error) => TryConnectCustomAgentResponse::FailAuth { error },
            Self::CliUnavailable(error) => TryConnectCustomAgentResponse::FailCli { error },
            Self::Fail(error) => TryConnectCustomAgentResponse::FailAcp { error },
            Self::RpcFailure { method, error } => TryConnectCustomAgentResponse::FailAcp {
                error: error
                    .public_startup_diagnostic()
                    .map_or_else(|| format!("ACP {method} failed: {error}"), |(hint, _)| hint.to_owned()),
            },
        };
        (response, None)
    }
}

async fn run_handshake(proc: &CliAgentProcess, private: bool) -> ProbeOutcome {
    let Some((stdin, stdout)) = proc.take_stdio().await else {
        return ProbeOutcome::Fail("stdio not available after spawn_for_sdk".to_string());
    };

    // Throwaway channels — a probe session never sends a prompt, so no events,
    // permission requests, or notifications are consumed.
    let (event_tx, _event_rx) = broadcast::channel(16);
    let (permission_tx, _permission_rx) = mpsc::channel(4);
    let (notification_tx, _notification_rx) = mpsc::channel(4);

    // Race the ACP initialize handshake against the child process exiting.
    // A misconfigured CLI (e.g. an invalid package launcher command) exits
    // almost immediately with a non-zero status; without this race the
    // `AcpProtocol::connect` call would block on its whole initialize budget
    // waiting for an `initialize` reply that will never arrive.
    //
    // The probe passes the steady-state budget rather than deriving a
    // cold-start one: the caller already caps this handshake at
    // `STEP2_TIMEOUT`, so a longer inner budget could never elapse. Keeping
    // the inner budget strictly below that cap also preserves which timeout
    // reports the failure — the inner one, as `InitTimeout`.
    let connect = AcpProtocol::connect(
        stdin,
        stdout,
        event_tx,
        permission_tx,
        notification_tx,
        "custom-agent-probe",
        None,
        InitBudget {
            timeout: Duration::from_secs(INIT_TIMEOUT_SECS),
            cold_start: false,
        },
        false,
        false,
        private,
    );
    let protocol = tokio::select! {
        biased;
        res = connect => match res {
            Ok(protocol) => protocol,
            Err(error) => return ProbeOutcome::RpcFailure { method: "initialize", error },
        },
        exit = proc.wait_for_exit() => {
            let stderr = proc.take_stderr().await;
            let stderr = stderr.trim();
            let status = match exit {
                Some(s) => format!("{s}"),
                None => "unknown".to_string(),
            };
            return if stderr.is_empty() {
                ProbeOutcome::Fail(format!("CLI exited before ACP initialize completed (status={status})"))
            } else {
                ProbeOutcome::Fail(format!("CLI exited before ACP initialize completed (status={status}): {stderr}"))
            };
        }
    };

    // `initialize` only proves the agent speaks ACP, not that it is usable.
    // Open a real session (no prompt) so an auth-gated agent surfaces its
    // `auth_required` error here instead of silently appearing "online".
    // Keep the response: it carries the agent's advertised modes / models /
    // config options, which the caller persists into `agent_metadata` so the
    // picker is populated before the user ever opens a conversation. Discarding
    // it meant a probed-online agent still showed an empty picker.
    let outcome = match protocol.new_session(NewSessionRequest::new(std::env::temp_dir())).await {
        Ok((response, legacy_models)) => {
            // Same extraction the live session path performs (`agent_session_flow`):
            // models ride beside the response because the SDK dropped the field.
            let models = legacy_models
                .as_ref()
                .and_then(crate::manager::acp::legacy_session_model::LegacySessionModelState::from_state_value);
            let mut catalog = crate::manager::acp::catalog_forwarder::catalog_partial_from_session_new(
                response.modes.as_ref(),
                models.as_ref(),
                response.config_options.as_deref(),
            )
            .unwrap_or_default();
            catalog.agent_capabilities = protocol
                .agent_capabilities()
                .and_then(|caps| crate::manager::acp::sdk_to_snake_value(&caps));
            ProbeOutcome::Ok(Some(Box::new(catalog)))
        }
        Err(AcpError::AuthRequired) => {
            ProbeOutcome::Auth("Agent reachable but requires login/authorization".to_string())
        }
        Err(_) if private => ProbeOutcome::Fail(crate::studio_aelix::probe_failure_message("acp_init_failed").into()),
        Err(error) => ProbeOutcome::RpcFailure {
            method: "session/new",
            error,
        },
    };

    // Drop `protocol` so its shutdown oneshot fires before the outer cleanup
    // path (or the drop guard for timeout-cancelled callers) reaps the process
    // tree. The probe session is throwaway; the process-group kill in the
    // caller tears down the session along with the CLI.
    drop(protocol);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn probe_returns_fail_cli_when_command_missing() {
        let resp = try_connect_custom_agent("aionui-definitely-does-not-exist-xyz", &[], &HashMap::new(), None).await;
        match resp {
            TryConnectCustomAgentResponse::FailCli { error } => {
                let lower = error.to_lowercase();
                assert!(
                    lower.contains("not found") || lower.contains("no such") || lower.contains("was not found"),
                    "expected 'not found' style message, got: {error}"
                );
            }
            other => panic!("expected FailCli, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_returns_fail_acp_when_command_is_noop() {
        // `true` exits 0 immediately — Step 1 passes (on PATH), but the
        // process dies before ACP initialize completes, so Step 2 maps
        // to FailAcp.
        if cfg!(windows) {
            // `true` is a cmd builtin on Windows, not a standalone exe.
            return;
        }
        let resp = try_connect_custom_agent("true", &[], &HashMap::new(), None).await;
        assert!(
            matches!(resp, TryConnectCustomAgentResponse::FailAcp { .. }),
            "expected FailAcp, got {resp:?}"
        );
    }

    /// Regression for the production leak: a probe that talks to a wrapper
    /// CLI (`npm exec ...`, etc.) historically left the wrapper's grandchild
    /// process alive when the probe returned, because cleanup relied on
    /// `kill_on_drop(true)` which only signals the direct child. Repeated
    /// connection tests could otherwise accumulate zombie `openclaw-acp`
    /// processes.
    ///
    /// We exercise the public entry point with a CLI that exits immediately
    /// after backgrounding a long-lived grandchild — that's the production
    /// shape `npm exec openclaw --acp` collapses into when its own ACP
    /// handshake fails. The probe will see the wrapper exit (ACP fail), but
    /// by that point the grandchild has been forked. The fix must SIGKILL
    /// the whole process group before returning, so the grandchild dies too.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_kills_grandchild_left_behind_by_wrapper() {
        use std::time::Duration;
        use tokio::time::Instant;

        fn is_pid_alive(pid: i32) -> bool {
            unsafe { libc::kill(pid, 0) == 0 }
        }

        let marker = tempfile::NamedTempFile::new().unwrap();
        let marker_path = marker.path().to_owned();
        // Background a grandchild, write its pid, then exit. The probe will
        // observe `proc.wait_for_exit()` race the ACP handshake and return
        // FailAcp — the grandchild keeps running unless cleanup kills it.
        let script = format!("sleep 600 & printf '%s' \"$!\" > '{}'", marker_path.display());

        let resp = try_connect_custom_agent("sh", &["-c".to_string(), script], &HashMap::new(), None).await;
        assert!(
            matches!(resp, TryConnectCustomAgentResponse::FailAcp { .. }),
            "wrapper exits before ACP handshake; expected FailAcp, got {resp:?}"
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut marker_contents = String::new();
        while Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(&marker_path)
                && !s.trim().is_empty()
            {
                marker_contents = s;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let grandchild_pid: i32 = marker_contents.trim().parse().unwrap_or_else(|_| {
            panic!("wrapper did not write the grandchild pid: {marker_contents:?}");
        });

        // Give the OS a brief moment to reap after the probe returned.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !is_pid_alive(grandchild_pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Cleanup so a failing test does not leave an actual leak.
        unsafe {
            libc::kill(grandchild_pid, libc::SIGKILL);
        }
        panic!("grandchild pid={grandchild_pid} survived the probe — process group cleanup is broken");
    }
}
