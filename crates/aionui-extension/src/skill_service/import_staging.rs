//! A process lease and drop-owned staging directory, including detached workers.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use tracing::{info, warn};

use crate::error::ExtensionError;

use super::archive::{OversizedZipFile, extract_zip_archive};

pub(super) const TEMP_DIR_NAME: &str = ".import-tmp";
const LOCK_FILE_NAME: &str = ".skill-import.lock";
const LEASED_PREFIX: &str = "leased-";

pub(super) struct ExtractedZip {
    pub staging: ZipStaging,
    pub oversized: Vec<OversizedZipFile>,
}

pub(super) struct ZipStaging {
    path: PathBuf,
    temp_root: PathBuf,
    // Kept outside .import-tmp and never unlinked: removing a locked inode
    // would let a peer create a different lock and race active imports.
    _lease: File,
}

impl ZipStaging {
    fn create(user_skills_dir: &Path) -> Result<Self, ExtensionError> {
        fs::create_dir_all(user_skills_dir)?;
        let lease = open_lease(user_skills_dir)?;
        FileExt::lock_shared(&lease)?;
        let temp_root = user_skills_dir.join(TEMP_DIR_NAME);
        fs::create_dir_all(&temp_root)?;
        let path = temp_root.join(format!(
            "{LEASED_PREFIX}{}",
            aionui_common::generate_prefixed_id("skills")
        ));
        fs::create_dir(&path)?;
        Ok(Self {
            path,
            temp_root,
            _lease: lease,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ZipStaging {
    fn drop(&mut self) {
        // Synchronous on purpose: cancellation/runtime shutdown cannot drop a
        // scheduled cleanup future. During extraction the blocking worker owns
        // this guard; after extraction its result/handler owns it until import
        // completes. A detached worker drops its unclaimed result too.
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(error_kind = ?error.kind(), "failed to clean skill zip extraction staging");
        }
        if let Err(error) = fs::remove_dir(&self.temp_root)
            && !matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            )
        {
            warn!(error_kind = ?error.kind(), "failed to remove empty skill zip temporary root");
        }
    }
}

pub(super) async fn extract_to_staging(
    user_skills_dir: &Path,
    archive_path: &Path,
) -> Result<ExtractedZip, ExtensionError> {
    let archive_path = archive_path.to_path_buf();
    extract_with(user_skills_dir.to_path_buf(), move |destination| {
        extract_zip_archive(&archive_path, destination)
    })
    .await
}

async fn extract_with<F>(user_skills_dir: PathBuf, extract: F) -> Result<ExtractedZip, ExtensionError>
where
    F: FnOnce(&Path) -> Result<Vec<OversizedZipFile>, ExtensionError> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        // Creation and ownership both happen inside the worker. Dropping the
        // awaiting handler before this closure even starts cannot leak staging.
        let staging = ZipStaging::create(&user_skills_dir)?;
        let oversized = extract(staging.path())?;
        Ok(ExtractedZip { staging, oversized })
    })
    .await
    .map_err(|error| ExtensionError::SkillImportInvalidZip(format!("Zip extraction task failed: {error}")))?
}

pub(super) fn cleanup_abandoned(user_skills_dir: &Path) -> Result<bool, ExtensionError> {
    let temp_root = user_skills_dir.join(TEMP_DIR_NAME);
    match fs::symlink_metadata(&temp_root) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let lease = open_lease(user_skills_dir)?;
    match FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            info!(reason = "active_import", "skill zip staging cleanup deferred");
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = match fs::symlink_metadata(&temp_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(&temp_root)?;
        return Ok(true);
    }
    let mut removed = false;
    for entry in fs::read_dir(&temp_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let abandoned = if name.starts_with(LEASED_PREFIX) {
            true // the exclusive lease proves no participating import is active
        } else if let Some(pid) = legacy_owner(&name) {
            !aionui_runtime::process_may_exist(pid)
        } else {
            false // unknown/older owner: never guess that it is safe to delete
        };
        if !abandoned {
            info!(reason = "legacy_or_unknown_owner", "skill zip staging cleanup deferred");
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
        removed = true;
    }
    // An older host does not use our lease and can create a child after the
    // snapshot above. Never recursively delete the shared root at the end.
    match fs::remove_dir(&temp_root) {
        Ok(()) => removed = true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
            ) => {}
        Err(error) => return Err(error.into()),
    }
    if removed {
        info!("removed abandoned skill zip import staging");
    }
    Ok(removed)
}

fn legacy_owner(name: &str) -> Option<u32> {
    let (pid, nonce) = name.strip_prefix("skills-")?.split_once('-')?;
    if nonce.is_empty() || !nonce.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok().filter(|pid| *pid > 0)
}

fn open_lease(user_skills_dir: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(user_skills_dir.join(LOCK_FILE_NAME))
}

#[cfg(test)]
mod tests;
