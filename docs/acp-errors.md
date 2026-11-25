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
(original numeric code), optional `data` (structured ACP data), and
`retryable: false`. Runtime ensure and private-peer handling filter `data` to
approved diagnostics. Other responses preserve keys such as `lockedBy`,
`sessionId`, and an explicit `holder: null` when present. The host reads these
reasons from structured fields.

- HTTP runtime/config errors retain status 502, expose the host code at the
  top level, and carry the public error payload in `details`.
- A streamed error carries the payload in `data` of the conversation stream
  event. Saved error tips carry it in `content.error` when read as messages.
- All four codes are non-retryable and do not trigger automatic turn replay.
  `USER_AGENT_PROMPT_OUTCOME_UNKNOWN` remains the distinct non-retryable
  transport-loss outcome from host #6.

Standard ACP reserves `-32000` for authentication (verified:
`agent-client-protocol-schema-1.5.0/src/v1/error.rs`). These application interpretations
apply only after initialization identifies `agentInfo.name` as `aelix-acp`.
Other agents keep the existing authentication and generic protocol error behavior.

## HTTP startup diagnostics

| Endpoint | HTTP status | Top-level `code` for ACP `-33004` |
| --- | --- | --- |
| `POST /api/conversations/{id}/runtime/ensure` | 502 | `BAD_GATEWAY` |
| `POST /api/agents/custom` and `PUT /api/agents/custom/{id}` registration probe | 400 | `BAD_REQUEST` |

For an allowlisted ACP application failure, the existing HTTP error envelope
includes `details.kind`. The first allowed kind is `socket_path_too_long`,
which Aelix ACP returns with code `-33004`. The Host includes `details.limitBytes`
when ACP supplies a positive integer up to 65535. Startup diagnostics contain
only these approved fields. Unknown kinds, peer messages, paths, credentials,
stderr, and other ACP data are omitted. True upstream unavailability retains
the generic `BAD_GATEWAY` response without startup details. Registration failures
retain the `acp_init_failed:` message prefix.

Example runtime ensure response

```json
{
  "success": false,
  "error": "ACP runtime initialization failed.",
  "code": "BAD_GATEWAY",
  "details": {
    "kind": "socket_path_too_long",
    "limitBytes": 104
  }
}
```

Studio reads `details.kind` to select the localized shorter, writable
`TMPDIR` hint. Sources for the wire error and Studio lookup are
[ACP's session implementation](https://github.com/handochan/aelix-acp/blob/7a2ca0e8e7edcc5fe41e15359e9e20a0fa9cc576/src/aelix_acp/session.py)
and [Studio PR #150](https://github.com/handochan/aelix-studio/pull/150).

## Tool metadata

Tool-call `_meta` is stored in the message content alongside `update`, with
opaque keys and values. SQLite JSON merge-patch preserves omitted fields,
removes keys patched with null, and replaces arrays with the newest array.
Statusless metadata updates retain terminal tool status, including after a turn
ends; the background watcher updates existing cards without creating a turn. History
previews compact large tool output without truncating `_meta`.

The application error codes and tool metadata cover host #4 scopes 2 and 3.
Scope 1 waits for aelix-ai #655 and aelix-acp #22.
