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

Every release archive contains `LICENSE` and `NOTICE`.

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

Further host changes (network defaults and session synchronization) land as
separate, reviewed commits.

## Releases

| Item         | Value                                                                    |
| ------------ | ------------------------------------------------------------------------ |
| Tag          | `host-v<upstream version>-aelix.<n>`, for example `host-v0.2.2-aelix.2`  |
| Archives     | `aelix-studio-host-<upstream version>-aelix.<n>-<rust target>.tar.gz` (`.zip` for Windows)               |
| Checksums    | `aelix-studio-host-checksums.txt` (`sha256sum -c` format)                         |
| Provenance   | `aelix-studio-host-<upstream version>-aelix.<n>-provenance.json`                                         |
| Targets      | darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64               |

- `<upstream version>` is the Cargo workspace version of the release commit
  (the release gate checks it); `<n>` starts at 1 for each upstream version.
  `aelix-studio-host --version` therefore reports the upstream version (for example
  `aelix-studio-host 0.2.2`); the tag and provenance identify the Aelix build.
- Release tags never start with `v`, so they cannot collide with the upstream
  `v*` tags, and the release workflow only reacts to `host-v*`.
- Each archive contains `aelix-studio-host` (`aelix-studio-host.exe`), `LICENSE`, `NOTICE` and
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
   and provenance.
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
  to aionrs.
- **Advisories:** the upstream v0.2.2 `Cargo.lock` already matches 19 RustSec
  advisories (rustls, rustls-webpki, webpki-roots, zip, zstd, tokio,
  crypto-bigint and others). They are listed in `deny.toml` as a baseline so
  that any new advisory fails CI. They are known, unresolved issues, not
  accepted risks. The owner decided they must be resolved before the Beta
  release ([#7](https://github.com/handochan/aelix-studio-host/issues/7)).

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
