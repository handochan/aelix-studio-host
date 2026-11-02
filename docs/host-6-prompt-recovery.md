# Prompt connection-loss recovery (#6)

## Baseline traced on main e55433d

| Link | Base source | Behavior |
| --- | --- | --- |
| Prompt | `crates/aionui-ai-agent/src/protocol/acp.rs:330-331, 517-521` | Connectivity check, SDK request, generic SDK error conversion |
| Conversion | `crates/aionui-ai-agent/src/protocol/error.rs:325-334` | SDK internal error becomes `AgentInternal` |
| Classification | `crates/aionui-ai-agent/src/protocol/send_error.rs:400-450, 535-545` | Unclassified internal failure is retryable |
| Connection classification | `crates/aionui-ai-agent/src/protocol/send_error.rs:273-286, 345-356` | `Disconnected` and `NotConnected` are retryable |
| Policy | `crates/aionui-conversation/src/turn_recovery_policy.rs:39-54` | Clean, retryable ACP failure can replay once |
| Execution | `crates/aionui-conversation/src/turn_orchestrator.rs:483-500, 541-584` | Rebuild task and use the same initial send |

The SDK documents that pending requests at EOF and requests made after EOF
receive the same error (verified:
`agent-client-protocol-2.0.0/src/concepts/connections.rs:94-103`). The discriminator
is `is_incoming_transport_closed` (verified:
`agent-client-protocol-2.0.0/src/jsonrpc.rs:2990-3011`). Handoff enqueues a request
without an application delivery receipt (verified:
`agent-client-protocol-2.0.0/src/jsonrpc.rs:3350-3448`).

## Change and boundary

Before SDK handoff, a known closed connection still produces `NotConnected`
with the existing retry classification. Incoming EOF/read failure and outgoing
write failure mark the connection dead before the SDK wakes pending callers.
After prompt handoff, connection loss produces `PromptOutcomeUnknown`.
Failure between enqueue and actual delivery is conservatively unknown: the
host cannot prove that no execution occurred.

The public stream/message error is `USER_AGENT_PROMPT_OUTCOME_UNKNOWN`, with
`retryable: false`, an explanation that the model or tools may already have
run, and no suggested retry action. The recovery policy rejects this code even
if another producer incorrectly marks it retryable. An explicit user resend
is a new turn; no deduplication key is added. Explicit non-internal ACP error
replies retain their current classifications even if the connection also closes.

`protocol/error.rs` and `protocol/send_error.rs` overlap with #4. This change
does not implement its `-33001`, `-33002`, or `-32000` display work. The API
foundation change is additive (one error-code variant); the full gate checks
workspace consumers. No endpoint, auth, or storage schema changes.

Logging assessment: existing production-level send-failure and turn-recovery
logs include safe error text, code, retryability, and the recovery decision.
They suffice for diagnosis. No prompt/tool payload or new raw transport
logging is added.

## Reproduce

POSIX, built host, Python standard library only:

```sh
python3 -I -B scripts/verification/smoke_prompt_recovery.py \
  --binary /path/to/aelix-studio-host --output /tmp/host-6-synthetic
```

The synthetic ACP peer holds a loopback model request before SIGKILL, stdout
EOF, or a stdout read error. Assertions cover one prompt/model request,
persisted non-retryable unknown outcome, no replay, and final idle state.
The pre-dispatch retry control kills an established idle connection; it requires
recovery, two session-open requests, and one prompt/model request. A separate
initial session-creation failure checks the existing build-failure behavior:
a terminal setup error with zero prompt/model requests (no new retry policy).
The CI host-smoke job runs this script and retains its report/logs.

For the original reproduction using real ACP and native source snapshots:

```sh
/path/to/native/.venv/bin/python -I -B scripts/verification/smoke_real_acp_crash.py \
  --host /path/to/aelix-studio-host --python /path/to/native/.venv/bin/python \
  --aelix /path/to/native/.venv/bin/aelix --acp-source /path/to/acp/src \
  --output /tmp/host-6-real --report /tmp/host-6-real/report.json
```

This probe adapts the Issue #6 handoff's `crash_probe.py` (originally based on
`aelix-acp/scripts/smoke_host.py`). It waits for a model request before killing
ACP, then asserts one model request, one prompt, no adapter restart, and a
persisted unknown outcome. `--kill-native` also kills the native process group;
`--expected-model-requests 2` supports an unpatched-path control.

Evidence levels remain separate: real host/ACP/native processes with a
synthetic loopback model do not establish real-provider behavior, Studio
rendering, installer acceptance, or release readiness.

## Recorded verification

[Historical verification for reviewed head `ebedbd1`](https://github.com/handochan/aelix-studio-host/issues/6#issuecomment-6581821347)
is retained in Issue #6. On Linux arm64 Docker, the five synthetic
scenarios passed. Real ACP `16d6555` and native `e19ab55` source snapshots
produced one model request and no adapter restart after killing ACP, both
alone and together with its native process group. Restoring only the prompt
protocol file to the base produced two model requests and one adapter restart
with the same probe. The fixed file was then restored.

Affected-crate tests: 2,205 passed, 12 existing ignored; affected clippy with
`-D warnings` passed. The regular host smoke also passed (13 checks). Full
workspace gate and exact-head CI results belong in the PR/Issue handoff.
