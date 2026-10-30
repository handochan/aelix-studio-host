# AGENTS.md

<!-- Maintenance rule: Only add content that tells AI assistants WHAT TO DO or WHAT NOT TO DO.
     Implementation details, design rationale, and "how the system works" belong in ARCHITECTURE.md.
     If a section doesn't contain an actionable rule or constraint, it doesn't belong here.
     Rules must reference stable concepts (layers, contracts, conventions) — never anchor them to
     specific field, type, or method names or file paths. Those drift and go stale fast; symbol-level
     detail belongs in code comments or ARCHITECTURE.md, not here. -->

Project-specific rules and conventions for AI assistants and contributors.

## Aelix Studio host operations

This repository is the Aelix Studio host, a modified clone of upstream
[iOfficeAI/AionCore](https://github.com/iOfficeAI/AionCore) (see README.md).
The sections after this one are upstream's technical rules and still apply to
the code here.

Read the shared operating rules before choosing work: the Studio
[operations guide](https://github.com/handochan/aelix-studio/blob/main/docs/contributing/studio-operations.md)
and the README of [Project 9](https://github.com/users/handochan/projects/9)
(exact ID `PVT_kwHOA-ATMs4BojoA`, shared with `aelix-studio`, `aelix-acp` and `aelix-ai`).

```sh
gh project view 9 --owner handochan --format json --jq .readme
gh project item-list 9 --owner handochan --format json --limit 100
```

- Follow the user's explicit task scope. For queued work, take only an assigned Ready Issue after its dependencies are satisfied.
- One Issue, one owner, one branch/worktree. Preserve other agents' dirty work. Update the Issue/Project at start, review submission, blockage and stop/completion; hand off with SHA/PR/worktree, verification, next action and blockers.
- One independent review from another session, then recheck affected validation. Use `needs-decision` for product/compatibility/release choices and `blocked` for unmet dependencies. Done requires actual acceptance evidence; a host PR alone does not complete a cross-repository feature.
- Fix behavior in the layer that owns it: execution/session rules in aelix-ai, protocol translation in aelix-acp (standard ACP first), UI in the Studio renderer, host behavior (DB, session loading, processes, network access) here. Do not copy native semantics into the host.
- Changes are ordinary reviewed commits with tests, never patch files. Keep internal crate, module, binary, DB and API names as upstream named them.
- Generic bug fixes may be proposed upstream only with the user's approval; releases never wait for upstream.

### Push, tags and releases

- Push only when the user authorized it, only your own branch (`git push -u origin <branch>`). Never push `main`, never force-push shared branches, never merge without approval.
- Never push tags unless the user approved that specific release. Release tags are `host-v<workspace version>-aelix.<n>` on a commit already on `main` (README.md "Releases").
- Never push upstream tags (`v*`) to origin, and never use `git push --tags`, `--follow-tags` or `--mirror`. Upstream tag commits carry upstream's release workflow, which would publish a release here once Actions are enabled.
- The `upstream` remote is fetch-only. Never open issues, PRs or comments on iOfficeAI repositories without the user's approval.
- `.github/workflows/` belongs to this repository; do not adopt upstream workflow changes. Keep CI green: fmt, clippy, tests, cargo-deny and the host smoke.
- Pin every action to a full commit SHA. Only GitHub-owned actions and the third-party actions allowlisted in the repository's Actions settings can run; ask the owner before adding another.
- Commits and PR titles use Conventional Commits in English. Never add AI signatures (Co-Authored-By, "Generated with" and similar) to commits, PRs or comments.

## High-Priority Rules

### NEVER guess an agent CLI's behavior — only assert what an approved source proves

Absolutely forbidden: inferring, guessing, or "reasoning about likely behavior" of any agent CLI
(claude, codex, gemini, opencode, hermes, aionrs, …) — its wire protocol, message shapes, field
semantics, timing, defaults, or capabilities — from a CLI's name, a plausible mental model, prior
training knowledge, or how you *think* it probably works. Every claim about an agent CLI's behavior
MUST be backed by one of these approved sources, cited explicitly by path:

1. **Captured real data** — actual sampled wire traffic recorded from the real CLI and kept where
   it can be cited (a fixture in this or a sibling Aelix repository, or a capture attached to the
   Issue). This is ground truth for what the CLI actually emitted.
2. **The ACP library source** — `agent-client-protocol` (main crate + `agent-client-protocol-schema`),
   vendored at `~/.cargo/registry/src/*/agent-client-protocol-*` — for the canonical ACP wire types
   and semantics we translate to.
3. **An official adapter's code** — a CLI's machine-generated schema (for example the codex
   `app-server` JSON schema generated by the codex binary itself), the official claude-code /
   claude-code-acp adapter source, or an equivalent first-party adapter — for inferring a CLI's
   contract from the reference implementation.

Additional reliable sources, when a claim can be grounded in them: **the CLI binary's own
`--help` / self-describing schema output** (run it and read it), and **our own passing
integration/live-e2e fixtures** that were recorded against the real CLI (not hand-authored
mocks). If none of these can substantiate a claim, the honest answer is "not yet verified —
need a capture/schema", and the next step is to capture or read a source — NOT to guess.

When you state any agent-CLI behavior, cite the source inline, for example
`verified: agent-client-protocol-schema-0.12.0/src/session.rs`. A claim with no such citation
is a guess and violates this rule. This is a non-negotiable, standing constraint — it outranks
convenience and applies to every statement, plan, commit message, and design doc.

### Do NOT state a claim as fact until you have verified it in the code yourself

This rule exists because of a repeated, costly failure mode: forming a confident conclusion from a *proxy* for the truth instead of the truth itself, then reporting it to the user as fact. Concrete instances that must never recur:

- **Trusting a sub-agent's conclusion without checking its evidence.** A spawned agent reported "the frontend has NO question renderer; the break is in the frontend." That was false — the frontend renders whatever `options[]` the backend sends; the real bug was the backend hard-coding `[Allow, Reject]` options for an AskUserQuestion. The conclusion was relayed to the user verbatim. **A sub-agent's report is a lead, not a fact. Before you repeat any load-bearing claim from an agent, open the cited file:line and confirm it says what the agent said.**
- **Declaring equivalence/correctness from a thin test.** A frame-by-frame A/B was run with one trivial prompt ("Reply PONG") that only exercised `start/text/finish`, then "core turn flow is equivalent" was declared. The prompt never triggered tool-output streaming, subagents, plans, permissions, AskUserQuestion, or mode-switch — where all the real divergences were. **A green result on inputs that don't exercise the behavior is not evidence about that behavior. Before claiming a class of behavior works, confirm your test actually produces that class of event.**

Enforced behaviors:
1. **Cite from primary source.** Any claim about what code does must be backed by a file you (this agent) have read this session — not a sub-agent's summary, not memory, not inference from names. Sub-agent findings must be spot-checked against the code before being surfaced.
2. **Verify the negative before asserting absence.** Never say "X has no Y / feature Z doesn't exist / the frontend can't do this" until you have grepped for it AND read the relevant handler. Absence is a strong claim; a single missed file falsifies it (e.g. `sideQuestion.ts`, `MessageAcpPermission` rendering dynamic `options[]`).
3. **Match the test to the claim.** When verifying behavior, the test input must exercise the exact events/paths the claim covers. Trivial/happy-path inputs prove only the trivial path. When you cannot exercise a path, say so explicitly rather than implying it passed.
4. **Trace to the break, don't guess the layer.** For a cross-layer bug (backend→wire→frontend), follow the actual data through every link and locate where it diverges from expected. Do not attribute the break to a layer by plausibility.
5. **Calibrate language to evidence.** Say "verified: <file:line>", "not yet checked", or "a sub-agent claims X (unverified)". Never launder an unverified lead into a flat assertion.

Standing discipline: self-consistent-all-green ≠ correct — verify outward (against a real agent) AND against the old/reference implementation, not just against your own happy path.

## Logging

When planning or changing a critical path or hard-to-observe flow, evaluate whether logging needs to change. In implementation plans for such changes, briefly state whether logs will be added, existing observability is sufficient, or logs are intentionally unnecessary. Do not add logs for simple refactors, test-only changes, UI copy/style changes, or when existing tests, errors, metrics, or logs already provide enough observability.

Add structured logs only when they help make production behavior diagnosable or provide extra development detail. Production normally runs at `info`, while development runs at `debug`; therefore, information needed to troubleshoot production issues must be available at `info`, `warn`, or `error`, not only `debug`.

Use log levels as follows:
- `debug` for high-frequency or detailed development-only flow details and state transitions
- `info` for low-volume production-diagnostic lifecycle boundaries, important state changes, and non-sensitive correlation context
- `warn` for malformed or unexpected data that is safely handled
- `error` for contract violations or failed operations

Production-visible logs must not include sensitive payloads such as prompts, tool input/output, file contents, command bodies, tokens, secrets, or raw provider requests/responses. If such payloads are needed for local debugging, they must be behind explicit development-only guards and never enabled by default.

## Architecture

> For detailed background and design decisions, see [ARCHITECTURE.md](./ARCHITECTURE.md).

Cargo workspace organized in four layers: Foundation → Capability → Domain → Composition. Dependencies flow strictly downward.

### Crate Hierarchy & Dependencies

- ✅ Upper layers may depend on lower layers (including cross-layer)
- ✅ Same-layer interaction through trait abstractions only
- ❌ No lower-layer depending on upper-layer
- ❌ No circular dependencies
- Changes to foundation crates require impact assessment

### Domain Crate Structure

Every domain crate must follow:
- `lib.rs` — module exports only, no business logic
- `routes.rs` — export `domain_routes(state) -> Router`, handlers do request/response transformation only
- `service.rs` — sole location for business logic, must not import axum
- `state.rs` — `#[derive(Clone)]` RouterState holding Arc-wrapped dependencies

### API Conventions

- Route prefix: `/api/`
- Resource names: kebab-case
- Response format: `ApiResponse<T>` (success) / `ErrorResponse` (failure)
- All request/response types defined in `aionui-api-types`
- `aionui-api-types` must NOT depend on axum/tower or any HTTP framework
- Use `aionui_common::ApiError` only at API/HTTP boundaries such as routes and middleware. Service/domain code must prefer crate-owned errors (`ConversationError`, `TeamError`, etc.) and map them to `ApiError` in route modules.

### WebSocket Events

- Format: `domain.camelCaseAction` (two-level structure)
- Message type: `WebSocketMessage<T>` (name + data)
- Existing kebab-case or three-level names are legacy — new events must follow the convention

### Data Layer

- Repository traits in `aionui-db`, prefixed with `I`
- Concrete implementations prefixed with `Sqlite`
- Row models in `aionui-db/src/models/`
- Params objects co-located in repository files
- Migrations: `NNN_descriptive_name.sql`, no manual DB modifications
- Services depend on traits, never on concrete implementations

### Dependency Injection

- `AppServices` is the sole service construction center
- Domain crates only define RouterState, never construct their own dependencies
- All assembly happens in `aionui-app`'s `build_*_state()` functions

### Security

- New endpoints must be evaluated for auth middleware requirement
- State-changing operations must be CSRF-protected
- Sensitive operations should have rate limiting
- Error responses must not leak internal details
- Secrets must never be hardcoded

## Code Style

- Rust 2024 edition, stable toolchain (pinned in `rust-toolchain.toml`)
- Comments in English, commit messages in English
- Each `.rs` file follows single responsibility — one module, one concern
- Target under 1000 lines per `.rs` file; exceeding it is a signal to split into submodules, not a hard limit (test files exempt)

## Development Workflow

### Subprocess Spawning

New subprocess spawn sites must go through `aionui_runtime`'s spawn Builder — never raw `tokio::process::Command`. See [ARCHITECTURE.md § Runtime Infrastructure](./ARCHITECTURE.md#runtime-infrastructure) for the correct constructor and details.

### Pushing Code

Follow "Push, tags and releases" above. Before pushing, run the checks in
"Verification Strategy" where a Rust toolchain is available, or the Docker
build in README.md "Building locally"; CI repeats all of them. `just push`
(full pre-push gate, then `git push`) may be used where a toolchain is
installed; it auto-commits lint/format fixes, so review its commit.

### Add Endpoint to Existing Crate

1. Request/response types → `aionui-api-types/src/{domain}.rs`
2. Handler function → `crates/aionui-{domain}/src/routes.rs`
3. Business logic → `crates/aionui-{domain}/src/service.rs`
4. Register route in `domain_routes()` function
5. Add test → `crates/aionui-{domain}/tests/` or `crates/aionui-app/tests/`

### Add Migration

1. Next number → `ls crates/aionui-db/migrations/`
2. Create `NNN_descriptive_name.sql` with `IF NOT EXISTS`

### Add WebSocket Event

1. Event type → `aionui-api-types`
2. Emit via `event_bus.broadcast()` in service
3. Naming: `domain.camelCaseAction`

## Test Organization

| Location                                 | What goes there                        |
| ---------------------------------------- | -------------------------------------- |
| Inline `#[cfg(test)]` in each `.rs` file | Unit tests for that module's internals |
| `crates/<crate>/tests/`                  | Integration / E2E tests for that crate |

### Testing Rules

- Database tests use `init_database_memory()`
- Prefer real in-memory DB over mocks; mock only to isolate unneeded dependencies
- New features must include tests

### Test Scope Requirements

**Happy Path (Critical Paths)**

Every new or modified feature must have integration tests covering its normal flow. Critical paths that always require test coverage:
- Authentication flow (login, token refresh, permission checks)
- Message sending and retrieval
- Agent session creation and interaction
- File upload/download
- WebSocket connection and event delivery

**Bad Path (Error Paths)**

New endpoints or business logic must include tests for these scenarios:
- Invalid input (missing fields, wrong types, oversized content)
- Resource not found (404)
- Insufficient permissions (unauthenticated, accessing another user's resources)
- Business rule violations (duplicate creation, operations not allowed in current state)

Bad path tests must assert specific error codes or error messages — asserting merely "not success" is not acceptable.

**Security Tests**

Endpoints involving authentication, authorization, or data isolation must include security tests:
- Unauthenticated requests are rejected (401)
- Cross-user data isolation (user A cannot access user B's resources)
- State-changing requests are rejected when CSRF token is missing or invalid
- Sensitive fields (passwords, tokens) never appear in responses

**WebSocket Event Tests**

New WebSocket events must verify:
- The event is emitted after the correct business operation
- Event payload conforms to `WebSocketMessage<T>` structure
- Events are only delivered to authorized subscribers (no leakage to unrelated users)

### Test Failure Handling

When a test fails, do NOT modify the test to make it pass. First determine:

1. **Test assertion still represents correct behavior** → fix implementation, not the test
2. **Requirements/interface intentionally changed** → may update test, but must confirm:
   - The change is intentional (not an unintended side effect)
   - New assertions still validate meaningful behavior
3. **Uncertain** → stop, trace back the change, clarify before proceeding

Prohibited:
- ❌ Deleting failing tests to "fix" the problem
- ❌ Weakening specific assertions to vague ones (e.g., `assert_eq!(status, 201)` → `assert!(status.is_success())`)

## Verification Strategy

> ⚠️ **When to run what:**
> - During development: only test the crate you're working on → `cargo test -p aionui-<crate>`
> - After implementation complete: full verification → `cargo test --workspace`
> - Do NOT run `cargo test --workspace` at the start of a task.
>
> ⚠️ **Performance:**
> - `cargo clippy --workspace` takes several minutes — use `run_in_background: true`.
> - `cargo test --workspace` takes 10+ minutes. MUST use `run_in_background: true` when calling via Bash tool, otherwise it will timeout.
> - `cargo clippy -p aionui-<crate>` and `cargo test -p aionui-<crate>` typically complete in under 1 minute.

### During Development (fast feedback loop)

```bash
cargo test -p aionui-<crate>                          # Test the crate you changed
cargo clippy -p aionui-<crate> -- -D warnings         # Lint the crate you changed
```

### Before Commit (affected crates)

```bash
cargo fmt --all -- --check                                                      # Format gate (instant)
cargo clippy -p aionui-<crate1> -p aionui-<crate2> -- -D warnings              # Lint affected crates
cargo test -p aionui-<crate1> -p aionui-<crate2>                               # Test affected crates
```

### Before Push (full workspace)

```bash
cargo clippy --workspace -- -D warnings               # what CI runs
cargo nextest run --workspace                         # what CI runs (or cargo test --workspace)
```
