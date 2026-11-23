//! Take an immutable, bounded copy of the normal chat attachment target.
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use aionui_ai_agent::protocol::byte_grants::{ByteGrantError, SNAPSHOT_LIMIT};
use aionui_api_types::ChatFileRef;
use aionui_project::{FileOp, ProjectService, ResolvedChatFile};
use same_file::Handle;
use tokio_util::sync::CancellationToken;

pub(crate) async fn snapshot_files(
    project: Arc<ProjectService>,
    owner: String,
    refs: Vec<ChatFileRef>,
    cancel: CancellationToken,
) -> Result<Vec<Vec<u8>>, ByteGrantError> {
    if cancel.is_cancelled() {
        return Err(ByteGrantError::Revoked);
    }
    // The normal resolver may perform synchronous filesystem queries. Keep
    // those and the copy off async workers, retaining the preparation charge
    // until this worker has discarded any partial result.
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let upload_root = std::env::temp_dir().join("aionui");
        let mut remaining = SNAPSHOT_LIMIT;
        let mut files = Vec::with_capacity(refs.len());
        for file in refs {
            let source = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ByteGrantError::Revoked),
                    source = project.resolve_chat_file(&owner, &file, &upload_root, FileOp::Read) =>
                        source.map_err(|_| ByteGrantError::File),
                }
            })?;
            let result = match source {
                ResolvedChatFile::Project(path) => snapshot(Path::new(&path), remaining, &cancel),
                ResolvedChatFile::Attachment(attachment) => attachment.with_file(|file| {
                    let before = file.metadata()?;
                    read_snapshot(file, &before, remaining, &cancel)
                }),
            };
            let bytes = result.map_err(|_| {
                if cancel.is_cancelled() {
                    ByteGrantError::Revoked
                } else {
                    ByteGrantError::File
                }
            })?;
            remaining -= bytes.len();
            files.push(bytes);
        }
        Ok(files)
    })
    .await
    .map_err(|_| ByteGrantError::File)?
}

fn check_cancel(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::ErrorKind::Interrupted.into())
    } else {
        Ok(())
    }
}

fn open_regular(path: &Path) -> io::Result<File> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    // A regular file raced into a FIFO must not hold a preparation open.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

fn snapshot(path: &Path, remaining: usize, cancel: &CancellationToken) -> io::Result<Vec<u8>> {
    check_cancel(cancel)?;
    let handle = Handle::from_file(open_regular(path)?)?;
    let file = handle.as_file();
    let before = file.metadata()?;
    let bytes = read_snapshot(file, &before, remaining, cancel)?;
    let current = Handle::from_file(open_regular(path)?)?;
    if handle != current
        || !unchanged(&before, &file.metadata()?)?
        || !unchanged(&before, &current.as_file().metadata()?)?
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

fn read_snapshot(
    mut file: &File,
    before: &Metadata,
    remaining: usize,
    cancel: &CancellationToken,
) -> io::Result<Vec<u8>> {
    let length = usize::try_from(before.len()).map_err(|_| io::ErrorKind::InvalidData)?;
    if length > remaining {
        return Err(io::ErrorKind::InvalidData.into());
    }
    check_cancel(cancel)?;
    let mut bytes = vec![0; length];
    for chunk in bytes.chunks_mut(64 * 1024) {
        check_cancel(cancel)?;
        file.read_exact(chunk)?;
    }
    check_cancel(cancel)?;
    if file.read(&mut [0])? != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    check_cancel(cancel)?;
    Ok(bytes)
}

fn unchanged(before: &Metadata, after: &Metadata) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
            return Ok(false);
        }
    }
    Ok(after.is_file() && before.len() == after.len() && before.modified()? == after.modified()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_grant_copy_enforces_the_exact_length_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source");
        let cancel = CancellationToken::new();
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(snapshot(&path, 4, &cancel).unwrap(), b"abcd");
        assert_eq!(
            snapshot(&path, 3, &cancel).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            snapshot(dir.path(), 4, &cancel).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            snapshot(&path.with_extension("missing"), 4, &cancel)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        cancel.cancel();
        assert_eq!(
            snapshot(&path, 4, &cancel).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
