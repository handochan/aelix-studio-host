//! Worker-owned staging and marker-based, fail-closed recovery.

mod ownership;

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use tracing::{info, warn};

use super::archive::{OversizedZipFile, extract_zip_archive};
use crate::error::ExtensionError;
use ownership::{Kind, OwnerLease};

pub(super) const TEMP_DIR_NAME: &str = ".import-tmp";
const LOCK_FILE_NAME: &str = ".skill-import.lock";
const PUBLISH_LOCK_FILE_NAME: &str = ".skill-import-publish.lock";

pub(super) struct ExtractedZip {
    pub staging: ZipStaging,
    pub oversized: Vec<OversizedZipFile>,
}

pub(super) struct ZipStaging {
    path: PathBuf,
    temp_root: PathBuf,
    root: PathBuf,
    owner: Option<OwnerLease>,
    container_owner: Option<OwnerLease>,
    _lease: File,
}

pub(super) struct PreparedInstall {
    pub copied_bytes: u64,
    staging: InstallStaging,
}

struct InstallStaging {
    path: PathBuf,
    root: PathBuf,
    owner: Option<OwnerLease>,
    _lease: File,
}

impl InstallStaging {
    fn create(user_skills_dir: &Path, user_root: &Path) -> Result<Self, ExtensionError> {
        fs::create_dir_all(user_skills_dir)?;
        let lease = open_lease(user_skills_dir)?;
        FileExt::lock_shared(&lease)?;
        let _publish = publication_lock(user_skills_dir)?;
        fs::create_dir_all(user_root)?;
        let path = user_root.join(format!(
            "{}leased-{}.tmp",
            super::IMPORT_STAGING_PREFIX,
            aionui_common::generate_prefixed_id("skill")
        ));
        let owner = create_owned_dir(&path, Kind::Install)?;
        Ok(Self {
            path,
            root: user_skills_dir.to_path_buf(),
            owner: Some(owner),
            _lease: lease,
        })
    }
}

impl Drop for InstallStaging {
    fn drop(&mut self) {
        let Ok(_publish) = publication_lock(&self.root) else {
            warn!(
                reason = "publication_lock",
                "skill installation staging cleanup deferred"
            );
            return;
        };
        cleanup_owned_leaf(&self.path, &mut self.owner);
    }
}

pub(super) async fn prepare_install<F>(
    user_skills_dir: &Path,
    user_root: &Path,
    copy: F,
) -> Result<PreparedInstall, ExtensionError>
where
    F: FnOnce(&Path) -> Result<u64, ExtensionError> + Send + 'static,
{
    let user_skills_dir = user_skills_dir.to_path_buf();
    let user_root = user_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let staging = InstallStaging::create(&user_skills_dir, &user_root)?;
        let copied_bytes = copy(&staging.path)?;
        Ok(PreparedInstall { copied_bytes, staging })
    })
    .await
    .map_err(|error| ExtensionError::Internal(format!("Skill installation staging task failed: {error}")))?
}

pub(super) async fn finish_install<F>(prepared: PreparedInstall, commit: F) -> Result<(), ExtensionError>
where
    F: FnOnce(&Path) -> Result<(), ExtensionError> + Send + 'static,
{
    // Keep ownership across the actual filesystem commit too. tokio::fs tasks
    // detached by cancellation must not outlive a handler-owned cleanup guard.
    tokio::task::spawn_blocking(move || {
        let result = commit(&prepared.staging.path);
        drop(prepared);
        result
    })
    .await
    .map_err(|error| ExtensionError::Internal(format!("Skill installation commit task failed: {error}")))?
}

impl ZipStaging {
    fn create(user_skills_dir: &Path) -> Result<Self, ExtensionError> {
        fs::create_dir_all(user_skills_dir)?;
        let lease = open_lease(user_skills_dir)?;
        FileExt::lock_shared(&lease)?;
        let _publish = publication_lock(user_skills_dir)?;
        let (temp_root, container_owner) = reserve_container(user_skills_dir)?;
        let path = temp_root.join(aionui_common::generate_prefixed_id("skills"));
        let owner = create_owned_dir(&path, Kind::Zip)?;
        Ok(Self {
            path,
            temp_root,
            root: user_skills_dir.to_path_buf(),
            owner: Some(owner),
            container_owner: Some(container_owner),
            _lease: lease,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ZipStaging {
    fn drop(&mut self) {
        let Ok(_publish) = publication_lock(&self.root) else {
            warn!(reason = "publication_lock", "skill zip staging cleanup deferred");
            return;
        };
        // The worker/result owns both record and root leases until all writes
        // stop. A cancelled handler cannot schedule cleanup ahead of its writer.
        cleanup_owned_leaf(&self.path, &mut self.owner);
        if let Some(owner) = self.container_owner.take() {
            // Only an empty, verified control container may be removed. Never
            // recurse through unmarked children, including legacy user data.
            if owner.try_exclusive() {
                match fs::remove_dir(&self.temp_root) {
                    Ok(()) => remove_record(owner),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => remove_record(owner),
                    Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                    Err(error) => warn!(error_kind = ?error.kind(), "skill staging container cleanup failed"),
                }
            }
        }
    }
}

fn create_owned_dir(path: &Path, kind: Kind) -> Result<OwnerLease, ExtensionError> {
    fs::create_dir(path)?;
    match OwnerLease::publish(path, kind) {
        Ok(owner) => Ok(owner),
        Err(error) => {
            // No payload has been written. create_dir established ownership of
            // this empty leaf, independently of its not-yet-published record.
            let _ = fs::remove_dir(path);
            Err(error.into())
        }
    }
}

fn reserve_container(root: &Path) -> Result<(PathBuf, OwnerLease), ExtensionError> {
    let mut path = root.join(TEMP_DIR_NAME);
    for _ in 0..4 {
        match fs::create_dir(&path) {
            Ok(()) => match OwnerLease::publish(&path, Kind::Container) {
                Ok(owner) => return Ok((path, owner)),
                Err(error) => {
                    let _ = fs::remove_dir(&path);
                    if error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(error.into());
                    }
                    // A surviving owner record is not ours to overwrite, even
                    // when its original empty container has already vanished.
                    path = root.join(format!(
                        "{TEMP_DIR_NAME}-{}",
                        aionui_common::generate_prefixed_id("container")
                    ));
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let real_directory = fs::symlink_metadata(&path)
                    .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
                if real_directory
                    && let Some(owner) = OwnerLease::open_verified(&path)
                    && owner.kind() == Kind::Container
                {
                    return Ok((path, owner));
                }
                // An unmarked same-name directory may be an existing skill or
                // interrupted publication. Do not add a marker or touch it.
                path = root.join(format!(
                    "{TEMP_DIR_NAME}-{}",
                    aionui_common::generate_prefixed_id("container")
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists).into())
}

fn cleanup_owned_leaf(path: &Path, owner: &mut Option<OwnerLease>) {
    match remove_leaf(path) {
        Ok(()) => {
            if let Some(owner) = owner.take() {
                remove_record(owner);
            }
        }
        Err(error) => warn!(error_kind = ?error.kind(), "owned skill staging cleanup failed"),
    }
}

fn remove_leaf(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn remove_record(owner: OwnerLease) {
    if let Err(error) = owner.remove() {
        warn!(error_kind = ?error.kind(), "skill staging owner record cleanup failed");
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

// The same proof is used by startup reclamation and filesystem/DB listing.
// No record, a partial record, an active lease or an alive/unknown PID means
// "preserve", regardless of the directory name. Containers never hide data.
pub(super) fn is_abandoned(storage_root: &Path, path: &Path) -> bool {
    inactive_in_scope(storage_root, path).is_some_and(|owner| owner.kind() != Kind::Container)
}

fn inactive_in_scope(storage_root: &Path, path: &Path) -> Option<OwnerLease> {
    if !path.starts_with(storage_root) || path == storage_root {
        return None;
    }
    let owner = OwnerLease::inactive(path)?;
    let parent = path.parent()?;
    let in_user_root = parent.parent() == Some(storage_root.join("users").as_path())
        && fs::symlink_metadata(storage_root.join("users")).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
        && fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink());
    let valid = match owner.kind() {
        Kind::Container => parent == storage_root,
        Kind::Install => parent == storage_root || in_user_root,
        Kind::Zip => {
            parent.parent() == Some(storage_root)
                && fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink())
                && OwnerLease::open_verified(parent).is_some_and(|owner| owner.kind() == Kind::Container)
        }
    };
    valid.then_some(owner)
}

pub(super) fn cleanup_abandoned(user_skills_dir: &Path) -> Result<bool, ExtensionError> {
    match fs::symlink_metadata(user_skills_dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let lease = open_lease(user_skills_dir)?;
    match FileExt::try_lock_exclusive(&lease) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            info!(reason = "active_import", "skill import staging cleanup deferred");
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    }
    let _publish = publication_lock(user_skills_dir)?;
    let mut removed = sweep_records(user_skills_dir, user_skills_dir, false)?;
    let users = user_skills_dir.join("users");
    match fs::symlink_metadata(&users) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(users)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    removed |= sweep_records(&entry.path(), user_skills_dir, false)?;
                }
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if removed {
        info!("removed abandoned owned skill staging");
    }
    Ok(removed)
}

fn sweep_records(root: &Path, storage_root: &Path, zip_children: bool) -> Result<bool, ExtensionError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut removed = false;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(stage_name) = name.to_str().and_then(|name| name.strip_suffix(ownership::SUFFIX)) else {
            continue;
        };
        if !ownership::valid_stage_name(stage_name) {
            continue;
        }
        let stage = root.join(stage_name);
        let Some(owner) = inactive_in_scope(storage_root, &stage) else {
            continue;
        };
        match owner.kind() {
            Kind::Container if !zip_children => {
                // A replaced/symlinked container is not a traversal authority.
                let real_directory = fs::symlink_metadata(&stage)
                    .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink());
                if real_directory {
                    // Keep the root exclusive lease/publication mutex, but
                    // release this record while children verify its shared
                    // container proof. No publisher can change it here.
                    drop(owner);
                    removed |= sweep_records(&stage, storage_root, true)?;
                    let Some(owner) = inactive_in_scope(storage_root, &stage) else {
                        continue;
                    };
                    match fs::remove_dir(&stage) {
                        Ok(()) => {
                            remove_record(owner);
                            removed = true;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
                        Err(error) => return Err(error.into()),
                    }
                } else if !stage.try_exists()? {
                    remove_record(owner);
                    removed = true;
                }
            }
            Kind::Install if !zip_children => {
                remove_leaf(&stage)?;
                remove_record(owner);
                removed = true;
            }
            Kind::Zip if zip_children => {
                remove_leaf(&stage)?;
                remove_record(owner);
                removed = true;
            }
            _ => {}
        }
    }
    Ok(removed)
}

fn open_lease(root: &Path) -> std::io::Result<File> {
    open_lock(root, LOCK_FILE_NAME)
}

fn publication_lock(root: &Path) -> std::io::Result<File> {
    let file = open_lock(root, PUBLISH_LOCK_FILE_NAME)?;
    FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn open_lock(root: &Path, name: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))
}

#[cfg(test)]
mod tests;
