# aion-mcp source and Host changes

The source is `crates/aion-mcp` from [iOfficeAI/aionrs v0.2.11](https://github.com/iOfficeAI/aionrs/tree/8e61a90329fa9f67c4fdf7e97fe02c24dba33f75/crates/aion-mcp), commit `8e61a90329fa9f67c4fdf7e97fe02c24dba33f75`. `LICENSE` is the byte-for-byte repository-root Apache-2.0 license. The source tag has no root or package NOTICE file. Host issue #29 tracks the modification. Host issue #64 tracks proposing it upstream.

## Changes against v0.2.11

| File | Changed hunks |
| --- | --- |
| `Cargo.toml` | Resolve inherited package fields and workspace dependencies in the Host workspace. Keep sibling crates on the aionrs tag. Add `bytes` for the retained HTTP chunk and `http` for transport tests. |
| `src/transport/mod.rs`, `types.rs` | Export the shared reader and add the typed transport read error. |
| `src/transport/response_limits.rs` | Check body length hints and received bytes before appending. Frame SSE incrementally, using a bounded buffer across chunks. Verify exact limits, split delimiters, multiple frames, chunked bodies without length, and slow-drip input. |
| `src/transport/streamable_http.rs` | Use the body/frame readers for Streamable HTTP, retaining session headers and RPC handling. Use bounded, payload-free errors for failed reads and avoid echoing raw malformed JSON. |
| `src/transport/sse.rs` | Use the same frame reader for endpoint negotiation and the persistent listener. Propagate terminal read failures to pending and subsequent requests. Race receive-side failures against the POST acknowledgement. Stop the listener when its transport is dropped. |
| `tests/streamable_http_crlf.rs` | Verify handshake, tool discovery and calls through both runtime SSE paths. |

The JSON and individual SSE frame limits are 16 MiB, following rmcp 3.9. The frame limit counts the wire delimiters and comments. A Streamable HTTP SSE reply has a separate 32 MiB total budget to allow preceding events while awaiting one RPC result. A persistent SSE connection retains an individual-frame limit for its lifetime. MCP HTTP error bodies already discarded by their callers allocate no application buffer. The Host OAuth token adapter reads error bodies up to 64 KiB, following the rmcp 3.9 error-body limit. A limit failure closes the response and returns `MCP_RESPONSE_BODY_TOO_LARGE` or `MCP_SSE_EVENT_TOO_LARGE`.

To compare the full source diff during an upgrade, extract `crates/aion-mcp` at the source commit using `git archive` from an aionrs checkout and compare that directory to `vendor/aion-mcp` with `git diff --no-index`. Carry over the hunks still needed by the new tag, then reconcile the Cargo patch, lockfile, source/license pin in `scripts/release/license-sources.json`, and root NOTICE. Verify that Cargo resolves a single `aion-mcp` for both Host and `aion-agent`.
