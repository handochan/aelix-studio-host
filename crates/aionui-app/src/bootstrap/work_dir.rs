//! Resolve the conversation workspace directory.

use std::path::{Path, PathBuf};

use aionui_common::host_env::HostEnv;

/// Priority: `--work-dir` CLI flag → `AIONUI_WORK_DIR` env (when non-empty) →
/// `--data-dir` fallback.
pub(super) fn resolve_work_dir(cli_work_dir: Option<PathBuf>, data_dir: &Path) -> PathBuf {
    cli_work_dir.unwrap_or_else(|| {
        HostEnv::WorkDir
            .read()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.to_path_buf())
    })
}
