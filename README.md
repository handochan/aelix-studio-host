# Aelix Studio host

This repository is the **Aelix Studio host**: a modified version of
[AionCore](https://github.com/iOfficeAI/AionCore), the Rust backend server
(`aelix-studio-host`, formerly `aioncore`) for
[Aelix Studio](https://github.com/handochan/aelix-studio). It is maintained
for Aelix Studio, separately from the upstream project. Aelix Studio consumes
its tagged releases, pinned by version and checksum.

- **Upstream:** [iOfficeAI/AionCore](https://github.com/iOfficeAI/AionCore),
  cloned with its full history. The current upstream base is recorded in
  [`scripts/release/policy.json`](scripts/release/policy.json): tag `v0.2.2`,
  commit `47e66d0d151123e973b3fd1e77afcb5671b3f8c5`.
- **License:** see [License](#license) and [`LICENSE`](LICENSE).
- **Modified:** yes. See [Modifications](#modifications) and [`NOTICE`](NOTICE).

The executable is named `aelix-studio-host` (`aelix-studio-host.exe` on
Windows). Internal crate, module, database and API names stay as upstream
named them (`aionui-*`), so upstream fixes can be adopted with small diffs.
Architecture and code conventions are in
[ARCHITECTURE.md](ARCHITECTURE.md) and [AGENTS.md](AGENTS.md).

## License

The facts below are recorded without a legal interpretation.

- [`LICENSE`](LICENSE) is the Apache License, Version 2.0, with the appendix
  notice `Copyright 2025 AionUi (aionui.com)`. Upstream added it in
  `b0d65d7` ("chore: add Apache 2.0 license", 2026-05-08). It is kept verbatim.
- GitHub's license detection reports Apache-2.0 for both
  iOfficeAI/AionCore and this repository.
- The root `Cargo.toml` declares `[workspace.package] license = "Apache-2.0"`,
  aligned with `LICENSE` by the repository owner's decision on 2026-10-31
  ([#2](https://github.com/handochan/aelix-studio-host/issues/2)). Only
  `aionui-assistant` inherits it; the other 26 crates declare no license.
  Upstream's baseline declared `MIT`, added in `673a5e7` (2026-04-12) before
  `LICENSE` was added. This repository corrects that metadata; it does not
  replace or edit the upstream license text.
- Upstream has no README or NOTICE file. The sibling upstream projects
  [AionUi](https://github.com/iOfficeAI/AionUi) (`package.json`) and
  [aionrs](https://github.com/iOfficeAI/aionrs) (`Cargo.toml` at `v0.2.11`, a
  dependency of this workspace) declare Apache-2.0.

Every new release archive contains `LICENSE`, `NOTICE`,
`THIRD-PARTY-NOTICES.txt` and `third-party-licenses.json`.

## Modifications

Changes are ordinary commits on top of the upstream base; there are no patch
files. `git log 47e66d0d151123e973b3fd1e77afcb5671b3f8c5..main` lists all of
them, and each release's provenance file names the base and release commits.
Categories so far:

- Repository tooling: CI and release workflows, release scripts
  (`scripts/release/`), `deny.toml`, this README, `NOTICE`, and the
  repository rules in `AGENTS.md`.
- Removed upstream automation: `release-please.yml`, `build-manual.yml` and the
  test that asserted their contents. `release-please-config.json`,
  `.release-please-manifest.json`, `Cross.toml` and `CHANGELOG.md` are kept
  unchanged but are not used by this repository's workflows.
- Crate manifests: `publish = false` on all workspace crates; workspace
  license metadata corrected to Apache-2.0.
- Host identity: executable, CLI help/invocations and User-Agent use
  `aelix-studio-host`. Updates require explicit host opt-in and a configured
  repository (see below).
- Permission recovery: ordinary session loading adopts the returned permission
  mode/configuration and clears stale permission intent, startup seeds and raw
  selections before reconciliation. Model/thought preferences and later explicit
  permission choices are retained. This ports the three-file recovery patch from
  [Studio PR #18](https://github.com/handochan/aelix-studio/pull/18) into source,
  with unit and synthetic stdio/SQLite/process-rebuild regression tests
  ([#3](https://github.com/handochan/aelix-studio-host/issues/3)).
- Prompt recovery: connection loss after prompt dispatch ends the turn with
  `USER_AGENT_PROMPT_OUTCOME_UNKNOWN` and `retryable: false`. The model or tools
  may already have run, so the host does not replay that turn. Failures observed
  before dispatch retain their existing retry policy. The dispatch boundary is
  handoff to the SDK, which provides no delivery receipt; loss during handoff
  is conservatively treated as an unknown outcome. An explicit user resend is
  a new turn ([#6](https://github.com/handochan/aelix-studio-host/issues/6)).

Further host changes (network defaults and session synchronization) land as
separate, reviewed commits.

## Releases

| Item       | Value                                                                                      |
| ---------- | ------------------------------------------------------------------------------------------ |
| Tag        | `host-v<upstream version>-aelix.<n>`, for example `host-v0.2.2-aelix.2`                    |
| Archives   | `aelix-studio-host-<upstream version>-aelix.<n>-<rust target>.tar.gz` (`.zip` for Windows) |
| Checksums  | `aelix-studio-host-checksums.txt` (`sha256sum -c` format)                                  |
| Provenance | `aelix-studio-host-<upstream version>-aelix.<n>-provenance.json`                           |
| Targets    | darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64                                |

- `<upstream version>` is the Cargo workspace version of the release commit
  (the release gate checks it); `<n>` starts at 1 for each upstream version.
  `aelix-studio-host --version` therefore reports the upstream version (for example
  `aelix-studio-host 0.2.2`); the tag and provenance identify the Aelix build.
- Release tags never start with `v`, so they cannot collide with the upstream
  `v*` tags, and the release workflow only reacts to `host-v*`.
- Each archive contains `aelix-studio-host` (`aelix-studio-host.exe`), `LICENSE`, `NOTICE`,
  `THIRD-PARTY-NOTICES.txt`, `third-party-licenses.json` and
  that target's `build-receipt.json`. Rust targets are
  `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`,
  `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`. Upstream's
  `aarch64-pc-windows-msvc` build is not produced.
- The provenance file records the release commit and tree, the upstream base
  (repository, tag, commit, commits since), the workflow run, the Rust
  toolchain, `Cargo.lock`/`LICENSE`/`NOTICE` hashes and, per target: archive and
  binary SHA-256, Mach-O code identity (macOS; unchanged by later code
  signing), compiler versions, Rust flags, the Linux build container and its
  measured GLIBC requirement, and the host smoke result. It is a plain JSON
  record, not a signed attestation.
- Third-party notice generation is mandatory for release and dry-run builds.
  Each receipt and the assembled provenance record the generator/helper hashes,
  cargo-about version, verified tool archive and executable hashes, arguments,
  result/config/diagnostic hashes, policy hashes, package counts and both notice
  file hashes. A source-only audit cannot pass archive assembly.

Studio pins a release by tag plus the archive SHA-256 from
`aelix-studio-host-checksums.txt`, and downloads
`https://github.com/handochan/aelix-studio-host/releases/download/<tag>/<archive>`.
The release asset label omits the tag's leading `host-v`: tag
`host-v0.2.2-aelix.2` produces, for example,
`aelix-studio-host-0.2.2-aelix.2-aarch64-apple-darwin.tar.gz` and
`aelix-studio-host-0.2.2-aelix.2-provenance.json`. Dry runs retain their
`dryrun-<commit>` label, for example
`aelix-studio-host-dryrun-<commit>-aarch64-apple-darwin.tar.gz`.

The already published `host-v0.2.2-aelix.1` release is immutable and keeps
its original `aioncore-…` archives, checksum and provenance names, and
`aioncore` executable. This naming rule applies to newly built assets.
Studio must adopt both the archive naming rule and executable member name in
its bundle preparation, resolver and verification together with a new exact
release pin ([Studio #28](https://github.com/handochan/aelix-studio/issues/28),
[identity #14](https://github.com/handochan/aelix-studio/issues/14)); host-only
checks do not establish that Studio launches it.

### How a release is built

[`.github/workflows/release.yml`](.github/workflows/release.yml):

1. **Validate ref.** This workflow publishes only for a pushed
   `host-v<version>-aelix.<n>` tag whose version equals the workspace version
   and whose commit is already on `main`. Pull requests (when release inputs
   change) and manual runs are dry runs labelled `dryrun-<commit>`. The gate
   runs from the tagged commit's own workflow file, so a `host-v*` tag on a
   commit with an edited `release.yml` would not be held by it; see
   [What actually controls releases](#what-actually-controls-releases).
2. **Build** each target on its own runner: macOS 15 (arm64), macOS 15 Intel,
   Ubuntu 24.04 arm64 and x64, Windows Server 2022.
   [`build.py`](scripts/release/build.py) checks the pinned Rust toolchain and a
   clean checkout whose files equal their git blobs byte for byte (Windows
   checks out with `core.autocrlf=false`; converted line endings would change
   compiled-in text such as migration checksums), builds `--locked --release`,
   and writes the receipt and archive. Linux builds run in [`hostBuild.Dockerfile`](scripts/release/hostBuild.Dockerfile)
   (Debian 11, pinned image digest and apt snapshot) and must stay within the
   upstream GLIBC ceilings (x64 2.34, arm64 2.30). Windows uses
   `-C target-feature=+crt-static`.
3. **Host smoke** ([`smoke.py`](scripts/release/smoke.py)) runs each built
   binary on its runner: startup and listening event, `/health`,
   `update_check_disabled_by_default` (HTTP 400 / `UPDATE_CHECK_DISABLED`), custom ACP
   agent registration with a synthetic agent, ACP client identity, conversation,
   runtime warm-up,
   launch-only model selector, unavailable-model rejection, a prompt/reply
   turn, idle state and graceful shutdown.
4. **Assemble** ([`provenance.py`](scripts/release/provenance.py)) cross-checks
   every receipt, archive member and smoke report, then writes the checksums
   and provenance. It also rechecks the archived third-party notices against the
   current crate/native policies and their original-notice hashes.
5. **Publish** (tag runs only) creates the GitHub release. This workflow
   refuses an existing release and never replaces its assets; that is a
   property of this workflow, not of the repository.

Creating a release tag requires the repository owner's approval (see
`AGENTS.md`). Dry-run assets are available as workflow artifacts for 30 days.

### What actually controls releases

The workflow gate is a consistency check, not an access control. What keeps a
release trustworthy is: push access to this repository, the push/tag rules in
`AGENTS.md`, the `block-upstream-v-tags` ruleset (no creation, update or
deletion of `refs/tags/v*`), and Studio pinning each archive by SHA-256.

### Building locally

Linux targets need only Docker on a machine of the same CPU architecture:

```sh
python3 scripts/release/build.py --target linux-arm64 --label dryrun-local \
  --output /tmp/aelix-studio-host-linux-arm64
python3 scripts/release/smoke.py --binary /tmp/aelix-studio-host-linux-arm64/aelix-studio-host
```

On Linux the smoke can run directly; from macOS, run it inside the build image
(`docker run ... python3 scripts/release/smoke.py ...`). Native targets need
the Rust toolchain pinned in `rust-toolchain.toml`.

## Update checks and existing data

`POST /api/system/check-update` is disabled by default, including requests
that supply a `repo`. It returns HTTP 400 / `UPDATE_CHECK_DISABLED` with an explanation before making
an update request. To opt in, set `AELIX_STUDIO_HOST_UPDATE_CHECK=1` before
starting the host and explicitly supply a repository in the request or via
`AIONUI_GITHUB_REPO=owner/repo`. There is no built-in upstream repository
fallback. This endpoint retains its existing semantic-version release lookup;
it does not install host releases or interpret `host-v*` release tags.

The HTTP User-Agent is `aelix-studio-host/<workspace version>`; the DingTalk
stream registration's `ua` identity and ACP `clientInfo.name` are
`aelix-studio-host`. The canonical ACP `Implementation` contract defines the
name as implementation metadata (`agent-client-protocol-schema-1.5.0/src/v1/agent.rs`).

The executable rename does not migrate data. The host still uses `--data-dir`
(default `data`), `aionui-backend.db`, existing `AIONUI_*_DIR` overrides and
existing log filenames. Work directories come from `--work-dir`,
`AIONUI_WORK_DIR`, then `--data-dir`; bundled resources are located relative
to the executable's parent directory, not its filename. Studio owns the
choice of its data directory and its separate app identity work.

### Interfaces whose names stay unchanged

Studio continues to consume these compatibility interfaces. Renaming the
executable and release assets does not rename their wire markers, identifiers,
storage paths or configuration:

- Stdout markers: `AIONCORE_LISTENING` and `AIONCORE_READY`.
- Log filename suffix: `.aioncore.log`.
- Benign peer-already-running result: exit code `3` and
  `BOOTSTRAP_PEER_ALREADY_RUNNING` (the explanatory text uses the new host name).
- Bootstrap authentication: `AIONCORE_BOOTSTRAP_SECRET` and the
  `x-aioncore-bootstrap-secret` HTTP header.
- Existing data directories, `aionui-backend.db`, and `AIONUI_*` environment
  variables (including directory and helper/runtime overrides).
- Internal crate/module names and API contract identifiers remain upstream names.

Studio's archive/member/resolver changes are tracked in
[Studio #28](https://github.com/handochan/aelix-studio/issues/28). This PR changes
the host only.

Other external access defaults, mirrors and connection-attempt auditing
remain PR ② of [#2](https://github.com/handochan/aelix-studio-host/issues/2).

## Dependency checks

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs migration
immutability, `cargo fmt`, `cargo clippy -D warnings`, `cargo nextest`, the
release-script tests, a linux-x64 host smoke, and `cargo deny check`
([`deny.toml`](deny.toml)): advisories, licenses, bans and sources.

- **Licenses:** crates must use a license on the allow list. Every crate
  marked `publish = false` is skipped, including third-party ones: our
  workspace crates (see [License](#license)) and, today, only aionrs'
  code-less hakari `workspace-hack` crate. `[sources]` limits git dependencies
  to aionrs. The BZip2 implementation used by zip 9 has a scoped license
  allowance for `libbz2-rs-sys =0.2.6` (`bzip2-1.0.6`); its distributed
  license text was checked separately from the general allow list.
- **Advisories:** dependency updates resolve 19 of the 20 inherited RustSec
  advisories, including all four zip notices, in
  [#7](https://github.com/handochan/aelix-studio-host/issues/7). Calamine 0.38
  also addresses RUSTSEC-2026-0436, published on 2026-11-03. New advisories
  fail CI.
- **Unresolved advisory baseline (2026-11-09):** three advisories published
  after #7 match the current `Cargo.lock`. RUSTSEC-2026-0459 is
  `libsqlite3-sys` 0.30.1, through `rusqlite` and `sqlx`. RUSTSEC-2026-0462 is
  `rmcp` 1.7.0. RUSTSEC-2026-0441 is `pin-utils` 0.1.0, which is unmaintained.
  They are listed in `deny.toml` so CI fails only on new advisories. This does
  not fix the advisories or accept their risks: each must be fixed before Beta
  in [#23](https://github.com/handochan/aelix-studio-host/issues/23), with its
  baseline entry removed when fixed.
- **User-approved advisory exception (2026-11-03):** RUSTSEC-2024-0449 is an
  unmaintained notice for `unsafe-libyaml`, with no patched version. It remains
  through both the host's `serde_yaml` and upstream aionrs' `aion-memory`;
  changing only the host parser cannot remove it. The owner approved this one
  exception separately from the unresolved baseline. Removal is tracked in
  [Later #18](https://github.com/handochan/aelix-studio-host/issues/18).

### Third-party release notices

[`notices.py`](scripts/release/notices.py) uses **cargo-about 0.8.4**, with
all five runner archives pinned by SHA-256 in
[`license-tools.json`](scripts/release/license-tools.json). This version supplies
both macOS architectures, Linux musl executables for the Debian build containers,
and Windows x64. Its root-manifest/target filtering and SPDX alternative selection
avoid inventing a second license detector. No ClearlyDefined data or mutable
upstream downloads are used during license resolution. The tool is downloaded
from its fixed release, checksum-verified, extracted without executing archive
paths, and its version is checked before use.

Each target uses the default features of `aionui-app`, its own target-filtered
graph and `--locked --offline --fail`. Build dependencies are deliberately retained
as a conservative superset; dev dependencies are excluded. The five graphs differ,
so no cross-target union is substituted. Crate SPDX choices are checked against
`deny.toml`, including exact-name/exact-version exceptions. Unsupported policy
syntax, a missing license text/original, nonzero exit, or an error/parse diagnostic
on stderr fails the build even if the tool exits zero. `deny.toml` is not modified.

The separate native-source allow list is
[`native-sources.json`](scripts/release/native-sources.json), following the
[owner's host #20 decision](https://github.com/handochan/aelix-studio-host/issues/20).
Every entry pins the package version, original `.crate` SHA-256, unpacked package
file inventory, native file inventory, build recipe, component/source hashes,
license conditions and verbatim notice-file hashes. Components include libgit2
1.9.3 with its complete `COPYING`/LINKING EXCEPTION, llhttp, xdiff, PCRE, libgit2's
alternative zlib/winhttp sources, SQLite 3.46.0's disclaimer/blessing and optional
SQLCipher source, AWS-LC and its Fiat/s2n-bignum/Jitter Entropy code, ring, zlib/
zlib-ng, Zstandard 1.5.7 and the BZip2 Rust implementation/header. Snapshot-only
subcomponents are identified by their containing package version and exact source
hash rather than an invented upstream version. Compiler probes, benchmark/test
files and Windows import libraries are also inventoried, with their package
licenses; this does not assert they are runtime C implementations.

Discovery scans C/C++/Objective-C, assembly, headers and prebuilt native libraries
in the resolved package sources, together with `links` declarations. It checks
all files of each native package as well as native files and build recipes, so
new/removed/changed source or templates cannot silently pass. Private workspace
packages are scanned too. Unknown native packages, versions, hashes, uncovered
source paths, missing originals or unknown Cargo native-link outputs fail closed.
The scanner is an inventory of packaged source, not a general static analyzer of
arbitrary future build scripts or code downloaded by them.

`cargo build --message-format=json-render-diagnostics` supplies the actual
`build-script-executed` link evidence for each release binary. Native records
distinguish explicit `static=` links, system/dynamic link declarations and no
native link output. A system library does not bypass source/notice checks: the
same pinned packaged-source superset is retained, and the actual link result is
recorded. This avoids forcing vendoring or changing dependency features. The
audited build recipes are `libgit2-sys/build.rs` (system probe then vendored C,
including llhttp/xdiff/PCRE), `libsqlite3-sys/build.rs` (bundled or system),
`libz-sys/build.rs`, `zstd-sys/build.rs`, `ring/build.rs` and `aws-lc-sys/builder/`.
Their hashes are part of the allow list. Native composite-license scanning is
kept separate from Rust wrapper SPDX selection; checksum-verified wrapper
clarifications for libgit2-sys and zstd-sys prevent cargo-about's deprecated-GPL
parse error without discarding the native originals or suppressing diagnostics.

The TXT and JSON retain original licensing files recursively (including `NOTICE`,
`COPYING`, `COPYRIGHT`, `AUTHORS` and `LICENSES/`), original copyright/SPDX comment
blocks, and git repository-root licenses. Thus aionrs's original
`Copyright 2026 iOfficeAI` survives. JSON retains precise file bytes as base64
with SHA-256; TXT renders them for reading. Where published crates omit a license
file, [`license-sources.json`](scripts/release/license-sources.json) records the
exact published VCS revision and hashes of upstream originals checked into
[`licenses/`](scripts/release/licenses). These supplements cover ACP, SIMD,
clipboard-win, CRC, include_dir, objc2 and MCP crates; generated canonical text
alone is not treated as their original copyright notice.

For the current lockfile, the package/repository-root scan found **zero separate
Apache NOTICE files** in the five target graphs and the supplemented upstream
license locations. This is recorded as an empty `apacheNoticeFiles` list, not as
a fabricated NOTICE. If a selected source contains one, the recursive scan keeps
it verbatim. The original host `NOTICE` remains a separate archive member.
Missing originals fail generation. The source audit and actual per-target counts,
link choices, originals and `missingNotices` result are available in each build
receipt/inventory and dry-run artifact. Studio installer integration remains a
separate step after pinning a host release.

```sh
python3 -I -B -m unittest discover -s scripts/release -p 'test_*.py' -v
python3 -I -B scripts/release/notices.py --verify-output dist/release-linux-x64
```

## Tags and upstream remotes

- Never push upstream tags (`v*`) or `--tags`/`--follow-tags`/`--mirror` to
  this repository. A workflow runs from the pushed tag's own commit, and
  upstream tag commits contain upstream's release workflow (`push: tags: v*`,
  `contents: write`), so pushing a new upstream tag would try to publish a
  release here. The upstream tags already on origin do not trigger anything
  by existing.
- The `block-upstream-v-tags` ruleset rejects creating, updating or deleting
  `refs/tags/v*`; `host-v*` tags are not affected.
- Repository Actions settings require every action to be pinned to a full
  commit SHA and allow only GitHub-owned actions plus `Swatinem/rust-cache`
  and `taiki-e/install-action`. Upstream jobs that reference actions by tag
  fail before their first step. Jobs made only of `run:` steps are not stopped
  by this: upstream's `build-manual.yml` has two (matrix preparation and
  summary, `workflow_dispatch` only, `contents: read`) and stayed dispatchable
  on `main` until this repository's workflows replaced it. Adding another
  third-party action requires the repository owner to extend the allowlist.
- Keep the `upstream` remote fetch-only, and every local clone must set
  `git config remote.upstream.tagOpt --no-tags`.
- Upstream changes are adopted deliberately (stable tags or selected commits)
  on a separate branch with review and the same CI; `.github/workflows/` here
  is ours and upstream workflow changes are not taken.

## Host-managed outbound access

This follows [Studio #15](https://github.com/handochan/aelix-studio/issues/15)
and the 2026-11-01/02 decisions in [host #2](https://github.com/handochan/aelix-studio-host/issues/2).
Implicit host downloads and remote-image fetching are off by default. Provider
endpoints and explicitly configured custom agents, MCP servers and custom skills
remain user choices. In particular, a user's npm/npx command is allowed: npm's
normal public-registry default applies unless a host mirror is configured.

| Surface | Default behavior | Explicit configuration |
| --- | --- | --- |
| Host update check | `UPDATE_CHECK_DISABLED` | `AELIX_STUDIO_HOST_UPDATE_CHECK=1` plus repository |
| Managed Node download | No public fallback; local cache/bundle still usable | `AELIX_STUDIO_HOST_NODE_MIRROR=https://mirror.example/node/dist/` |
| Host-predefined npx registry agents / implicit npm | `OUTBOUND_DISABLED` without a host mirror | `AELIX_STUDIO_HOST_NPM_REGISTRY=https://mirror.example/repository/npm/` |
| User-configured npm/npx (custom agent or MCP) | Allowed, normal npm configuration/public default | The same optional npm mirror; applies to explicit paths and Node npx-cli.js wrappers too |
| Remote images (`/api/fs/fetch-remote-image`) | HTTP 403, `REMOTE_IMAGE_DISABLED` with reason; no request | `AELIX_STUDIO_HOST_REMOTE_IMAGES=1` preserves existing image host validation |
| OfficeCLI missing binary / daily update | No installer/update HTTP; restore Studio's binary or PATH | Studio owns installation and updates |
| Public WebUI tunnel / Moltbook | Removed from builtin bundle/catalog | Custom imports are unaffected |

No predefined pnpx/bunx/uvx launch sites were found in the checked host registry.
Implicit uses of these launchers are rejected because they have no host mirror
contract. Known Node npx-cli.js wrappers follow the npm policy. This controls
host launch preparation, not arbitrary code run by a configured agent/skill.
MCP preparation failures are saved as conversation tips and emit an
owner/conversation-scoped warning in the existing
`message.stream` tips channel, with a reason; they do not terminate a turn or
silently disappear. Connection-test history is not overwritten by a session
attachment failure. Capability-incompatible transports remain silent selection
filters.

### Settings Studio must forward

These are host-process environment settings, read at launch. Set them in the
build/resource-preparation environment **and** the host launch environment where
applicable; setting them only in the renderer does not configure the host.

| Setting | Build / resource preparation | Host execution |
| --- | --- | --- |
| `AELIX_STUDIO_HOST_NODE_MIRROR` | Required when host prepares/downloads managed Node; use the official archive bytes under the mirror prefix | Required only if a local cache/bundle is unavailable and downloading is intended |
| `AELIX_STUDIO_HOST_NPM_REGISTRY` | Forward if preparation intentionally invokes npm/npx | Enables host-predefined registry agents; overrides the registry for user npm/npx when set |
| `AELIX_STUDIO_HOST_REMOTE_IMAGES` | No build download required | Omit/other values disable fetching; exactly `1` opts in. Restart the host after changing it |

Mirror URLs must be HTTP(S), without embedded credentials, query or fragment.
Node appends `v24.11.0/node-v24.11.0-<platform>.<extension>`; redirects are
rejected and errors never fall back to nodejs.org. All downloaded Node archives,
including mirror responses, **must** match the five pinned values from the
[official Node 24.11.0 SHASUMS256](https://nodejs.org/dist/v24.11.0/SHASUMS256.txt)
before extraction. The verbatim source manifest is checked in beside the
runtime implementation. Repacking an archive changes its hash and is rejected.
Studio must also verify these same archive hashes before supplying an expanded
Node bundle. Expanded local directories retain the existing layout/version
validation; an archive hash is not misrepresented as a directory hash.

For npm/npx, registry selection and offline/background-request settings are
injected after environment cleaning. With no host mirror, explicit user commands
retain normal npm configuration; with a mirror, serve metadata, tarballs and
transitive dependencies there. npm update notifications/audit/fund are disabled.
A configured package or its install scripts can still make their own requests;
these settings are source selection, not a general process firewall.
Studio integration/installed-app acceptance remains in [Studio #28](https://github.com/handochan/aelix-studio/issues/28).

### Builtin upgrade and internet-dependent skills

Retirement affects only host-owned builtin directories and `source=builtin`
catalog rows. User custom skills, including same-name imports, are preserved.
If an upgrade cannot delete/refresh the old tree (locked file, permissions or
rename failure), the host starts with a filtered fallback and warns. Retired
skills and old OfficeCLI-family skills are hidden from discovery, catalog,
read and session-use paths. Unsafe OfficeCLI installer instructions are never
returned from builtin paths, even if physical cleanup failed. A later successful
refresh restores safe current builtin skills.

`aionui-webui-setup` documents same-network/VPN access only. The unchanged
`morph-ppt-3d`, `mermaid`, `openclaw-setup` and `ui-ux-pro-max` skills include
internet-dependent instructions/resources and run on user request; importing
or invoking them is not an offline/zero-egress acceptance claim.

### Outbound-attempt verification

Linux CI wraps the default functional smoke with `outbound_audit.py`: startup,
blocked predefined registry-agent checks, remote-image rejection, retired
builtin absence, OfficeCLI missing-binary requests, synthetic ACP session,
prompt/reply, idle and shutdown. A second audited flow uses real Node/npm/npx
against a loopback registry (bare and absolute-path npx). Fixtures are prepared
before auditing; no fixture is shipped in the host release.

`strace -ff` follows the complete process tree and records attempted IPv4/IPv6
connect/send calls, including failed calls. Loopback/Unix IPC are allowed; DNS
port 53 counts as outbound even on loopback. Incomplete/unparseable evidence
fails closed (87). If nonempty `HTTP_PROXY`, `HTTPS_PROXY` or `ALL_PROXY` is
present (case-insensitive), or `npm_config_proxy`/`npm_config_https_proxy` is
set, the auditor refuses execution with 87. It also checks launch-time project,
user and global `.npmrc` files, including explicit npm config paths and config
redirects; nonempty proxy entries or unreadable configuration fail closed.
Values and credentials are never included in its report. `NO_PROXY` is not an
exemption. This preflight does not discover command-line config overrides or
files/configurations created or selected later by a child in another directory;
those cases are outside the accepted fixture, and tracing a proxy endpoint alone
does not establish its ultimate destination.
Parent and child-process TCP/UDP positive controls must each be detected and
fail the audit with 86. JSON reports are uploaded; raw buffer-suppressed traces
are deleted. This measures the exercised Linux defaults/local-mirror fixture,
not arbitrary configured providers, third-party agents, proxies or internet skills.

```sh
python3 -I -B scripts/release/outbound_audit.py --report smoke/outbound.json -- \
  python3 -I -B scripts/release/smoke.py --binary target/debug/aelix-studio-host --report smoke/report.json
```

The direct Codex session/title handshake and MCP connection-test client names
also identify `aelix-studio-host`. Internal subprocess labels, crate/module names,
cookies and the compatibility interfaces listed above retain their identifiers.
