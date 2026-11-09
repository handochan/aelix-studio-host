# Host #23 dependency changes

Scope and remaining SQLite blocker: [owner decision](https://github.com/handochan/aelix-studio-host/issues/23#issuecomment-6743527141).
The rusqlite/sqlx manifests and versions, aionrs tag, migrations, and CI workflows
are unchanged. No exception was added. RUSTSEC-2026-0459 remains an unresolved
Beta blocker; RUSTSEC-2024-0449 remains the previously approved exception.

## Targeted updates

Only event-listener, lru, rmcp, and the AWS consumers of the old Smithy HTTP/runtime
were explicitly updated. The other changes below are their resolved dependencies.
The before column is main `1fa1e19`; an added version coexists with the old version
unless the row says otherwise.

| Crate | Before | After | Reason |
|---|---|---|---|
| event-listener | 5.4.1 | 5.4.2 | RUSTSEC-2026-0221; lockfile only |
| lru | 0.18.0 | 0.18.2 | RUSTSEC-2026-0253; satisfies aionrs' existing `0.18` requirement |
| rmcp | 1.7.0 | 3.9.0 | RUSTSEC-2026-0462 |
| rmcp-macros | 1.7.0 | 3.9.0 | Matching SDK macros |
| base64 | 0.22.1 | add 0.23.1 | rmcp 3.9 dependency; existing consumers stay on 0.22.1 |
| darling | 0.23.0 | add 0.24.1 | rmcp-macros dependency |
| darling_core | 0.23.0 | add 0.24.1 | darling dependency |
| darling_macro | 0.23.0 | add 0.24.1 | darling dependency |
| aws-config | 1.8.16 | 1.13.0 | Replace consumer of Smithy HTTP 0.63 |
| aws-runtime | 1.7.3 | 1.10.2 | Replace consumer of Smithy HTTP 0.63 |
| aws-sdk-bedrock | 1.141.0 | 1.166.0 | Replace consumer of Smithy HTTP 0.63 |
| aws-sdk-sso | 1.98.0 | 1.119.0 | Replace aws-config dependency on Smithy HTTP 0.63 |
| aws-sdk-ssooidc | 1.100.0 | 1.121.0 | Replace aws-config dependency on Smithy HTTP 0.63 |
| aws-sdk-sts | 1.103.0 | 1.124.0 | Replace host/aionrs consumer of Smithy HTTP 0.63 |
| aws-smithy-runtime | 1.11.1 | 1.16.2 | Remove direct pin-utils requirement |
| aws-smithy-http | 0.63.6 and 0.64.3 | 0.64.3 only | Remove old direct pin-utils requirement |
| aws-smithy-http-client | 1.1.12 | 1.5.2 | Updated AWS runtime requirement |
| aws-smithy-json | 0.62.5 | 0.65.1 | Updated AWS SDK requirement |
| aws-smithy-observability | 0.2.6 | 0.3.0 | Updated AWS runtime requirement |
| aws-smithy-query | 0.60.15 | 0.64.0 | Updated STS requirement |
| aws-smithy-xml | 0.60.15 | 0.64.1 | Updated AWS query requirement |
| aws-types | 1.3.15 | 1.7.0 | Updated AWS SDK/config requirement |
| aws-smithy-schema | absent | 1.1.2 | Updated AWS SDK/runtime requirement |
| arc-swap | absent | 1.9.4 | Updated AWS SDK requirement |
| pin-utils | 0.1.0 | removed | RUSTSEC-2026-0441; no patched version, remove both incoming paths |
| http | 0.2.12 and 1.4.0 | 1.4.0 only | Old Smithy HTTP dependency removed |
| http-body | 0.4.6 and 1.0.1 | 1.0.1 only | Old Smithy HTTP dependency removed |
| reqwest | 0.12.28 | add 0.13.5 | Test-only rmcp HTTP backend; host stays on 0.12.28 |
| sse-stream | absent | 0.2.6 | Test-only rmcp HTTP/SSE backend |
| wasm-streams | 0.4.2 | add 0.5.0 | reqwest 0.13 target-specific dependency |

Same-version consumers select the updated dependencies above: aion-providers
0.2.11 and aionui-system 0.2.2 select the updated AWS crates; aion-tools 0.2.11
selects lru 0.18.2; async-lock 3.4.2, async-process 2.5.0,
event-listener-strategy 0.5.4, and sqlx-core 0.8.6 select event-listener 5.4.2.
AWS feature resolution also drops the unused HTTP 0.2 dependency of aws-sigv4
1.6.4 and HTTP 0.2/http-body 0.4 dependencies of aws-smithy-types 1.9.1.
Existing Windows dependency selections for anstyle-query, anstyle-wincon,
arboard, quinn-udp, and socket2 are preserved from main rather than accepting
unrelated resolver changes.

Initial reverse tree: pin-utils had exactly two direct consumers,
aws-smithy-http 0.63.6 and aws-smithy-runtime 1.11.1. Both reached the host's
AWS configuration/Bedrock integration and aionrs' provider dependencies.
After these updates, pin-utils is absent from Cargo.lock and from the five-target
default-feature graph. No aionrs source or dependency declaration was patched.

## rmcp API and verification scope

The team stdio bridge uses `ContentBlock::text` in place of the removed
`Content::text`; success/error text and structured error codes remain the same.
The existing server handler, tool constructors, schema macros, and error type
continue to compile with 3.9.0. Existing boundary errors provide observability;
no new production logs are needed for this API/type migration.

The shipped host enables rmcp's server/stdio features. Its HTTP connection probe
is implemented separately in aionui-mcp. The SDK HTTP client is enabled only in
development dependencies to exercise its actual loopback transport and limits;
it does not replace the host probe or change production TLS/client selection.

Source references in the published rmcp 3.9.0 crate:

- `src/model/content.rs`: `ContentBlock::text` and text serialization.
- `src/transport/common/client_side_http.rs`: unconditional 16 MiB JSON and
  64 KiB error-response limits; both length hints and received chunks are checked.
- `src/transport/common/reqwest/streamable_http_client.rs`: default client POST
  routes JSON/error bodies through that bounded reader; no opt-in is required.
- `src/transport/streamable_http_client.rs`: default 16 MiB SSE event limit.

Repository regression tests exercise stdio initialization, the filtered tool list,
successful text output, sanitized remote tool errors, SDK HTTP initialization,
session headers, tool listing and JSON-RPC errors, and rejection of oversized
JSON/error bodies with declared length or chunked responses held open past the
limit. These are synthetic loopback transport checks, not real provider/CLI or
field acceptance. Full CI and five-target build results belong in the PR and
Issue handoff for the exact tested SHA.
