//! Take an immutable, bounded copy of the normal chat attachment target.
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

use aionui_ai_agent::protocol::byte_grants::{ByteGrantError, SNAPSHOT_LIMIT};
use aionui_api_types::ChatFileRef;
use aionui_project::{FileOp, ProjectService};
use same_file::Handle;

pub(crate) async fn snapshot_files(
    project: Arc<ProjectService>,
    owner: String,
    refs: Vec<ChatFileRef>,
) -> Result<Vec<Vec<u8>>, ByteGrantError> {
    let upload_root = std::env::temp_dir().join("aionui");
    let mut paths = Vec::with_capacity(refs.len());
    for file in refs {
        paths.push(
            project
                .resolve_chat_file_ref(&owner, &file, &upload_root, FileOp::Read)
                .await
                .map_err(|_| ByteGrantError::File)?,
        );
    }

    tokio::task::spawn_blocking(move || {
        let mut remaining = SNAPSHOT_LIMIT;
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let bytes = snapshot(Path::new(&path), remaining).map_err(|_| ByteGrantError::File)?;
            remaining -= bytes.len();
            files.push(bytes);
        }
        Ok(files)
    })
    .await
    .map_err(|_| ByteGrantError::File)?
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

fn snapshot(path: &Path, remaining: usize) -> io::Result<Vec<u8>> {
    let handle = Handle::from_file(open_regular(path)?)?;
    let mut file = handle.as_file();
    let before = file.metadata()?;
    let length = usize::try_from(before.len()).map_err(|_| io::ErrorKind::InvalidData)?;
    if length > remaining {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    if file.read(&mut [0])? != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }

    let current = Handle::from_file(open_regular(path)?)?;
    if handle != current
        || !unchanged(&before, &file.metadata()?)?
        || !unchanged(&before, &current.as_file().metadata()?)?
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
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
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(snapshot(&path, 4).unwrap(), b"abcd");
        assert_eq!(snapshot(&path, 3).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(snapshot(dir.path(), 4).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            snapshot(&path.with_extension("missing"), 4).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
