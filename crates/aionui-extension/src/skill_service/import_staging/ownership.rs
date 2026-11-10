//! Ownership records live beside staging, outside imported/copied payloads.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub(super) const SUFFIX: &str = ".aelix-staging-owner";
const SCHEMA: &str = "aelix.skill-staging/1";
const MAX_RECORD_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    Zip,
    Install,
    Container,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: String,
    stage_name: String,
    kind: Kind,
    pid: u32,
    token: String,
}

pub(super) struct OwnerLease {
    file: File,
    marker: PathBuf,
    record: Record,
}

impl OwnerLease {
    // Caller has reserved an empty directory with create_dir and holds the
    // root publication mutex. Nothing writes payload before this returns.
    pub fn publish(path: &Path, kind: Kind) -> io::Result<Self> {
        Self::publish_with(path, kind, || Ok(()))
    }

    pub fn publish_with<F>(path: &Path, kind: Kind, before_publish: F) -> io::Result<Self>
    where
        F: FnOnce() -> io::Result<()>,
    {
        let marker = marker_path(path)?;
        match fs::symlink_metadata(&marker) {
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let record = Record {
            schema: SCHEMA.into(),
            stage_name: stage_name(path)?.into(),
            kind,
            pid: std::process::id(),
            token: aionui_common::generate_prefixed_id("owner"),
        };
        let pending = marker.with_file_name(format!(
            "{}.pending-{}",
            marker.file_name().unwrap().to_string_lossy(),
            record.token
        ));
        let cleanup = Pending(pending.clone());
        let mut file = OpenOptions::new().create_new(true).write(true).open(&pending)?;
        file.write_all(&serde_json::to_vec(&record).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        before_publish()?;
        fs::rename(&pending, &marker)?;
        drop(cleanup);
        Self::open_verified(path).ok_or_else(|| io::Error::other("published staging owner could not be verified"))
    }

    pub fn open_verified(path: &Path) -> Option<Self> {
        let marker = marker_path(path).ok()?;
        let metadata = fs::symlink_metadata(&marker).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_RECORD_BYTES {
            return None;
        }
        let file = OpenOptions::new().read(true).write(true).open(&marker).ok()?;
        FileExt::try_lock_shared(&file).ok()?;
        let record = read_record(&file)?;
        if record.schema != SCHEMA
            || record.stage_name != stage_name(path).ok()?
            || record.pid == 0
            || record.token.is_empty()
        {
            return None;
        }
        Some(Self { file, marker, record })
    }

    pub fn kind(&self) -> Kind {
        self.record.kind
    }

    pub fn try_exclusive(&self) -> bool {
        FileExt::try_lock_exclusive(&self.file).is_ok()
    }

    pub fn inactive(path: &Path) -> Option<Self> {
        let owner = Self::open_verified(path)?;
        // A live owner, including EPERM/unknown PID state, is never reclaimed
        // or hidden. The exclusive nonblocking lease also protects PID races.
        if aionui_runtime::process_may_exist(owner.record.pid) {
            return None;
        }
        FileExt::try_lock_exclusive(&owner.file).ok()?;
        Some(owner)
    }

    pub fn remove(self) -> io::Result<()> {
        let Self { file, marker, record } = self;
        drop(file);
        // Publication/removal is serialized by the caller's root mutex/lease.
        // Keep a replaced/unrecognized record rather than unlinking by name.
        let current = File::open(&marker).ok().and_then(|file| read_record(&file));
        if current.as_ref() != Some(&record) {
            tracing::warn!(
                reason = "owner_record_changed",
                "kept unverified skill staging owner record"
            );
            return Ok(());
        }
        match fs::remove_file(marker) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

fn read_record(file: &File) -> Option<Record> {
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

pub(super) fn marker_path(path: &Path) -> io::Result<PathBuf> {
    Ok(path.with_file_name(format!("{}{SUFFIX}", stage_name(path)?)))
}

fn stage_name(path: &Path) -> io::Result<&str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_stage_name(name))
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))
}

pub(super) fn valid_stage_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

struct Pending(PathBuf);
impl Drop for Pending {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
