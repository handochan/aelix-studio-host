# aion-mcp source and Host changes

The source is `crates/aion-mcp` from [iOfficeAI/aionrs v0.2.11](https://github.com/iOfficeAI/aionrs/tree/8e61a90329fa9f67c4fdf7e97fe02c24dba33f75/crates/aion-mcp), commit `8e61a90329fa9f67c4fdf7e97fe02c24dba33f75`. `LICENSE` is the byte-for-byte repository-root Apache-2.0 license. The source tag has no root or package NOTICE file. Host issue #29 tracks the modification. Host issue #64 tracks proposing it upstream.

## Changes against v0.2.11

| File | Changed hunks |
| --- | --- |
| `Cargo.toml` | Resolve inherited package fields and workspace dependencies from the pinned aionrs manifest. Keep sibling crates on the same tag and depend on the shared Host HTTP readers. |
| `src/transport/stdio.rs` | Apply the Host child-environment policy after stdio overrides. |
| `src/transport/types.rs` | Add the typed transport read error. |
| `src/transport/streamable_http.rs` | Use the body/frame readers for Streamable HTTP, retaining session headers and RPC handling. Use bounded, payload-free errors for failed reads and avoid echoing raw malformed JSON. |
| `src/transport/sse.rs` | Cap endpoint negotiation and active RPC windows. Dispatch only matching replies and surface terminal read errors during POST acknowledgement. Stop the listener when its transport is dropped. |
| `src/manager.rs` | Allocate distinct JSON-RPC IDs for tool calls so overlapping requests retain independent response budgets. |

The Host workspace excludes this package so the existing `AIONRS` developer override can select the local SDK without a duplicate path package in Cargo.lock. Default release builds use the vendored patch. The shared readers and boundary tests live in `crates/aionui-mcp-http`; the Host transport integration tests cover handshake, tool discovery and calls through both runtime SSE paths.

The JSON and individual SSE frame limits are 16 MiB, following rmcp 3.9. The frame limit counts the wire delimiters and comments. A Streamable HTTP SSE reply has a separate 32 MiB total budget to allow preceding events while awaiting one RPC result. Persistent SSE also uses that budget for endpoint negotiation and for each outstanding RPC. All incoming wire bytes count while an RPC waits, including comments and replies for other IDs. Idle connections retain the frame limit. MCP HTTP error bodies already discarded by their callers allocate no application buffer. The Host OAuth token adapter reads error bodies up to 64 KiB, following the rmcp 3.9 error-body limit. A limit failure closes the response and returns `MCP_RESPONSE_BODY_TOO_LARGE` or `MCP_SSE_EVENT_TOO_LARGE`.

[UPSTREAM.diff](UPSTREAM.diff) records the full source changes. Regenerate it from the repository root with:

```sh
python3 -I -B scripts/verification/aion_mcp_upstream_diff.py
```

The script fetches the pinned tag read-only, verifies its commit, and excludes the generated diff itself. It can reuse a local aionrs repository with `--upstream-checkout`. CI runs the same command with `--check` and tests the vendored crate against a standalone lockfile seeded from the Host lockfile.

During an aionrs upgrade, carry over the hunks and shared-reader code still needed by the new tag. Reconcile the Cargo patch, lockfile, source/license pin in `scripts/release/license-sources.json`, and root NOTICE. Verify that Cargo resolves a single `aion-mcp` for both Host and `aion-agent`.
