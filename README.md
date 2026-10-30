# Aelix Studio host

This repository is the **Aelix Studio host**: a modified version of
[AionCore](https://github.com/iOfficeAI/AionCore), the Rust backend server
(`aioncore`) that [Aelix Studio](https://github.com/handochan/aelix-studio)
bundles. It is maintained for Aelix Studio, separately from the upstream
project. Aelix Studio consumes its tagged releases, pinned by version and
checksum.

- **Upstream:** [iOfficeAI/AionCore](https://github.com/iOfficeAI/AionCore),
  cloned with its full history. The current upstream base is recorded in
  [`scripts/release/policy.json`](scripts/release/policy.json): tag `v0.2.2`,
  commit `47e66d0d151123e973b3fd1e77afcb5671b3f8c5`.
- **License:** see [License](#license) and [`LICENSE`](LICENSE).
- **Modified:** yes. See [Modifications](#modifications) and [`NOTICE`](NOTICE).

The internal crate, module, binary, database and API names stay as upstream
named them (`aionui-*`, `aioncore`), so upstream fixes can be adopted with
small diffs. Architecture and code conventions are in
[ARCHITECTURE.md](ARCHITECTURE.md) and [AGENTS.md](AGENTS.md).

## License

The facts below are recorded without a legal interpretation.

- [`LICENSE`](LICENSE) is the Apache License, Version 2.0, with the appendix
  notice `Copyright 2025 AionUi (aionui.com)`. Upstream added it in
  `b0d65d7` ("chore: add Apache 2.0 license", 2026-05-08). It is kept verbatim.
- GitHub's license detection reports Apache-2.0 for both
  iOfficeAI/AionCore and this repository.
- Upstream's root `Cargo.toml` declares `[workspace.package] license = "MIT"`.
  That line dates from `673a5e7` ("feat: initialize cargo workspace with 17
  crate stubs", 2026-04-12), before `LICENSE` was added, and has not changed
  since. Only `aionui-assistant` inherits it; the other 26 crates declare no
  license. This repository keeps that metadata unchanged; the discrepancy is
  not resolved here.
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
- Crate manifests: `publish = false` on all workspace crates.

Host behavior changes for Aelix Studio (for example identity and network
defaults, permission recovery, session synchronization) land as separate,
reviewed commits and are listed here when they do.

## Releases

| Item         | Value                                                                    |
| ------------ | ------------------------------------------------------------------------ |
| Tag          | `host-v<upstream version>-aelix.<n>`, for example `host-v0.2.2-aelix.1`  |
| Archives     | `aioncore-<tag>-<rust target>.tar.gz` (`.zip` for Windows)               |
| Checksums    | `aioncore-checksums.txt` (`sha256sum -c` format)                         |
| Provenance   | `aioncore-<tag>-provenance.json`                                         |
| Targets      | darwin-arm64, darwin-x64, linux-arm64, linux-x64, win32-x64               |

- `<upstream version>` is the Cargo workspace version of the release commit
  (the release gate checks it); `<n>` starts at 1 for each upstream version.
  `aioncore --version` therefore reports the upstream version (for example
  `aioncore 0.2.2`); the tag and provenance identify the Aelix build.
- Release tags never start with `v`, so they cannot collide with the upstream
  `v*` tags, and the release workflow only reacts to `host-v*`.
- Each archive contains `aioncore` (`aioncore.exe`), `LICENSE`, `NOTICE` and
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
`aioncore-checksums.txt`, and downloads
`https://github.com/handochan/aelix-studio-host/releases/download/<tag>/<archive>`.
The archive name keeps Studio's existing `aioncore-${tag}-${rust target}`
resolver pattern.

### How a release is built

[`.github/workflows/release.yml`](.github/workflows/release.yml):

1. **Validate ref.** Only a pushed `host-v<version>-aelix.<n>` tag whose
   version equals the workspace version and whose commit is already on `main`
   can publish. Pull requests (when release inputs change) and manual runs are
   dry runs labelled `dryrun-<commit>`.
2. **Build** each target on its own runner: macOS 15 (arm64), macOS 15 Intel,
   Ubuntu 24.04 arm64 and x64, Windows Server 2022.
   [`build.py`](scripts/release/build.py) checks the pinned Rust toolchain and a
   clean checkout, builds `--locked --release`, and writes the receipt and
   archive. Linux builds run in [`hostBuild.Dockerfile`](scripts/release/hostBuild.Dockerfile)
   (Debian 11, pinned image digest and apt snapshot) and must stay within the
   upstream GLIBC ceilings (x64 2.34, arm64 2.30). Windows uses
   `-C target-feature=+crt-static`.
3. **Host smoke** ([`smoke.py`](scripts/release/smoke.py)) runs each built
   binary on its runner: startup and listening event, `/health`, custom ACP
   agent registration with a synthetic agent, conversation, runtime warm-up,
   launch-only model selector, unavailable-model rejection, a prompt/reply
   turn, idle state and graceful shutdown.
4. **Assemble** ([`provenance.py`](scripts/release/provenance.py)) cross-checks
   every receipt, archive member and smoke report, then writes the checksums
   and provenance.
5. **Publish** (tag runs only) creates the GitHub release. It refuses to
   replace an existing release.

Creating a release tag requires the repository owner's approval (see
`AGENTS.md`). Dry-run assets are available as workflow artifacts for 30 days.

### Building locally

Linux targets need only Docker on a machine of the same CPU architecture:

```sh
python3 scripts/release/build.py --target linux-arm64 --label dryrun-local \
  --output /tmp/aioncore-linux-arm64
python3 scripts/release/smoke.py --binary /tmp/aioncore-linux-arm64/aioncore
```

On Linux the smoke can run directly; from macOS, run it inside the build image
(`docker run ... python3 scripts/release/smoke.py ...`). Native targets need
the Rust toolchain pinned in `rust-toolchain.toml`.

## Dependency checks

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs migration
immutability, `cargo fmt`, `cargo clippy -D warnings`, `cargo nextest`, the
release-script tests, a linux-x64 host smoke, and `cargo deny check`
([`deny.toml`](deny.toml)): advisories, licenses, bans and sources.

- **Licenses:** third-party crates must use a license on the allow list.
  Workspace crates are unpublished and excluded (see [License](#license)).
- **Advisories:** the upstream v0.2.2 `Cargo.lock` already matches 19 RustSec
  advisories (rustls, rustls-webpki, webpki-roots, zip, zstd, tokio,
  crypto-bigint and others). They are listed in `deny.toml` as a baseline so
  that any new advisory fails CI. They are known, unresolved issues, not
  accepted risks; a dependency update is a separate change.

## Tags and upstream remotes

- Never push upstream tags (`v*`) or `--tags`/`--follow-tags`/`--mirror` to
  this repository. A workflow runs from the pushed tag's own commit, and
  upstream tag commits contain upstream's release workflow (`push: tags: v*`,
  `contents: write`), so pushing a new upstream tag would try to publish a
  release here. The upstream tags already on origin do not trigger anything
  by existing.
- Repository Actions settings add a second guard: actions must be pinned to a
  full commit SHA, and only GitHub-owned actions plus `Swatinem/rust-cache`
  and `taiki-e/install-action` may run. Upstream's workflows reference actions
  by tag and fail before running any step. Adding another third-party action
  requires the repository owner to extend that allowlist.
- Keep the `upstream` remote fetch-only, and prefer
  `git config remote.upstream.tagOpt --no-tags` in local clones.
- Upstream changes are adopted deliberately (stable tags or selected commits)
  on a separate branch with review and the same CI; `.github/workflows/` here
  is ours and upstream workflow changes are not taken.
