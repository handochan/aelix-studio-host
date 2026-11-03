# ACP errors and tool metadata

The host exposes four application error codes for Studio locale entries:

| Host code | Original ACP code | Structured reason |
| --- | --- | --- |
| `USER_AGENT_SESSION_LOCKED` | `-33001` | `data.kind = session_locked`, `sessionId`, `lockedBy`, `holder`, `retryable` |
| `USER_AGENT_TURN_FAILED_AFTER_DISPATCH` | `-33002` | `data.kind = turn_failed_after_dispatch`, `retryable`, optional `cause` |
| `USER_AGENT_BUSY` | `-32000` | Busy or session limit; `data` may be absent |
| `USER_AGENT_OPERATION_REFUSED` | `-32004` | Inspect `data.kind`, including `restrictions_unenforceable`, `profile_unavailable`, `extension_owned_by_launch` |

Verified adapter contract: [aelix-acp `16d6555`, Studio integration](https://github.com/handochan/aelix-acp/blob/16d6555b08825d1d63c3b082de2b10a1152562c0/docs/studio-integration.md),
[src/aelix_acp/server.py](https://github.com/handochan/aelix-acp/blob/16d6555b08825d1d63c3b082de2b10a1152562c0/src/aelix_acp/server.py),
and [src/aelix_acp/wire/errors.py](https://github.com/handochan/aelix-acp/blob/16d6555b08825d1d63c3b082de2b10a1152562c0/src/aelix_acp/wire/errors.py).

The public error payload includes `code` (host locale code), `protocol_code`
(original numeric code), `data` (original structured ACP data, when present),
and `retryable: false`. Keys inside `data` remain verbatim, including
`lockedBy`, `sessionId`, and an explicit `holder: null`. The host does not use
human-readable error messages to determine these reasons.

- HTTP runtime/config errors retain status 502, expose the host code at the
  top level, and carry the public error payload in `details`.
- A streamed error carries the payload in `data` of the conversation stream
  event. Saved error tips carry it in `content.error` when read as messages.
- All four codes are non-retryable and do not trigger automatic turn replay.
  `USER_AGENT_PROMPT_OUTCOME_UNKNOWN` remains the distinct non-retryable
  transport-loss outcome from host #6.

Standard ACP reserves `-32000` for authentication (verified:
`agent-client-protocol-schema-1.5.0/src/v1/error.rs`). The busy interpretation
applies only after initialization identifies `agentInfo.name` as `aelix-acp`.
Other agents keep the existing authentication behavior.

Tool-call `_meta` is stored in the message content alongside `update`, with
opaque keys and values. SQLite JSON merge-patch preserves omitted fields,
removes keys patched with null, and replaces arrays with the newest array.
A statusless metadata update retains the terminal tool status. History
previews compact large tool output without truncating `_meta`.

This change covers host #4 scopes 2 and 3. Replay suppression is unchanged;
scope 1 waits for aelix-ai #655 and aelix-acp #22.
