# Host byte source v1

[Host #59](https://github.com/handochan/aelix-studio-host/issues/59) implements the byte approval and acquisition contract in [Studio #94 §10](https://github.com/handochan/aelix-studio/blob/865777ec4c13a031ecd7b20f6e42412bcd8da8e6/docs/studio-release-train/session-and-management-contracts.md#running-input-images). This is the D17 exception for approving a chat attachment and returning its RAM snapshot over the bound ACP connection. Image validation, model eligibility, turn checks and queue writes belong to the adapter.

## Negotiation and preparation

The built-in standalone connection offers `{"version":1}` under `clientCapabilities._meta["aelix/hostBytes"]`. The peer returns the same object under `agentCapabilities._meta["aelix/hostBytes"]`, alongside its existing H1 relay advertisement. Once the peer echoes the current H1 binding at session open, protected discovery includes `hostBytes: true`. An older peer retains the original H1 shape.

`POST /api/conversations/{id}/byte-grants` uses the same protected admission as H1, including WebUI Origin and CSRF. The request sends an opaque `prepareId`, the current `bindingId` and one to four ordered `refs` using the ordinary `ChatFileRef` shape. The no-store response has the standard HTTP success envelope with `grants`, `expiresAtMs` and `expiresInMs` in its data. Each grant has `id`, `byteLength` and lowercase hex `sha256`. A repeated prepare with the same auth generation and ordered refs joins the work or returns the original descriptors and expiry. `expiresAtMs` is fixed and `expiresInMs` is the remaining initial acquisition window.

The batch covers up to 6 MiB of generic bytes. The Host reserves that budget during the copy and accounts for the actual bytes on publication. It admits one in-flight preparation per binding, two per owner and four per Host. The Host holds at most 32 MiB of snapshots and reservations and 256 batch records. A successful batch starts a 60-second acquisition window for each grant. The first successful read starts the grant's 30-second reader deadline. A finished grant retains a 10-minute completion marker. A close or replacement destroys the binding's grants. The same account, runtime and binding admission is checked before publication and the first byte of a reverse reply.

A preparation times out after 30 seconds. Timeout, binding closure, or observed revocation cancels the resolver and copy at their next cooperative checkpoint. An HTTP disconnect leaves the idempotent preparation running. Timeout retires the binding's pending record immediately and a late result cannot publish. An in-flight filesystem syscall retains its owner and Host preparation slots and RAM reservation until the worker exits. File reads check cancellation between bounded reads, and a worker discards partial copies on cancellation. Capacity returns when the worker exits.

## Reverse wire

Send one `_aelix/host/bytes/read` request at a time on the current ACP connection. The method takes a `sessionId`, `bindingId`, `ref`, `readerId` and `offset`, with exactly one of `maxBytes` and `finish: true`. The first read starts at zero. An acquired reader may read at the next offset or replay its most recent offset. After checking EOF, length and SHA-256, send finish at the full length. Retry a lost finish under the same reader and offset.

The 192 KiB reply ceiling covers the whole JSON-RPC frame excluding the newline. The Host accepts RPC IDs up to 1,024 bytes of JSON and reserves room for a retry with any accepted ID. The result includes `offset`, `byteLength`, `sha256`, `eof` and base64 `data`. Finish returns `{"released":true}`, including on duplicate finish. Invalid params receive `-32602`. Unknown methods receive `-32601`. A denied, expired or unavailable grant/read receives `-33110` with fixed text. Raw reply frames use the H1 stdio writer and bypass the SDK's payload trace. Standard ACP traffic has priority before the first byte of any reverse response, including a rejection.

Standard Stop leaves the binding and its grants live. The adapter stops asking for chunks after it observes cancellation and checks the turn again before writing to native. A chunk already requested may still arrive. A grant abandoned before EOF keeps its RAM until the reader's original 30-second deadline. An unused grant expires at the original 60-second deadline. An accepted finish or binding invalidation releases the bytes earlier.

## Diagnostics

The Host disables the SDK's payload diagnostics for byte-source candidates, including initialization and ordinary ACP requests, responses and notifications. ACP wire payloads and content projections remain intact. Byte-source peers' standard RPC errors are classified by code at the Host boundary and keep fixed fields such as the lock source before free-form peer detail is discarded. Host access logs record request metadata and fixed errors for this API. The built-in agent stderr stream is discarded and the development ACP prompt dump is disabled for byte-source candidates.

Studio owns the console and crash diagnostics of application messages after delivery. The protected HTTP helper suppresses request and error bodies with `omitDiagnostics`. Standard ACP events delivered through the renderer's `message.stream` path require the same diagnostic protection. The [Studio bridge at be40c623](https://github.com/handochan/aelix-studio/blob/be40c623b6b65ccdda50f95eb37c48bb6f2752ee/packages/desktop/src/common/adapter/httpBridge.ts#L506-L521) still logs a prefix of the standard event payload.

[Contract examples](examples/byte-grants.json) · [Executable peer](../../crates/aionui-app/tests/fixtures/acp_relay.py) · [Protected HTTP and SDK TRACE tests](../../crates/aionui-app/tests/studio_auth_e2e/byte_grants.rs)
