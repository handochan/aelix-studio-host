//! File identity at the Host's Local and Upload attachment boundary.

use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use same_file::Handle;

#[cfg(windows)]
#[path = "chat_attachment/windows.rs"]
mod platform;
#[cfg(unix)]
#[path = "chat_attachment/unix.rs"]
mod platform;

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("The attachment is unavailable.")]
    Unavailable,
    #[error("The attachment is outside the managed upload directory.")]
    OutsideUploadRoot,
}

impl From<io::Error> for AttachmentError {
    fn from(_: io::Error) -> Self {
        Self::Unavailable
    }
}

struct UploadRoot {
    input: PathBuf,
    path: PathBuf,
    handle: Handle,
}

struct Source {
    input: PathBuf,
    path: String,
    handle: Handle,
    upload_root: Option<UploadRoot>,
    cursor: Mutex<()>,
}

/// An opened regular file that passed the Local or Upload policy. Clones share
/// the open file identity across admission, dispatch, and retries.
#[derive(Clone)]
pub struct ChatAttachment(Arc<Source>);

impl fmt::Debug for ChatAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChatAttachment(..)")
    }
}

impl ChatAttachment {
    /// Validate an absolute attachment path. For an upload, scope it to the
    /// server's managed directory. Never open a client-supplied UNC or device path.
    pub fn open(input: &Path, upload_root: Option<&Path>) -> Result<Self, AttachmentError> {
        if wire_path(input).is_err() || !platform::is_supported_path(input) {
            return Err(AttachmentError::Unavailable);
        }
        let upload_root = upload_root
            .map(|input| {
                let handle = open_identity(input, true)?;
                let path = platform::opened_path(handle.as_file())?;
                Ok::<_, io::Error>(UploadRoot {
                    input: input.to_owned(),
                    path,
                    handle,
                })
            })
            .transpose()?;
        let handle = open_identity(input, false)?;
        let target = platform::opened_path(handle.as_file())?;
        if let Some(root) = &upload_root
            && !inside_root(root, &target)
        {
            return Err(AttachmentError::OutsideUploadRoot);
        }
        let path = platform::wire_target(input, target)?;
        let path = wire_path(&path)?.to_owned();
        let attachment = Self(Arc::new(Source {
            input: input.to_owned(),
            path,
            handle,
            upload_root,
            cursor: Mutex::new(()),
        }));
        attachment.verify_current()?;
        Ok(attachment)
    }

    pub fn path(&self) -> &str {
        &self.0.path
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        self.verify_current()?;
        let metadata = self.0.handle.as_file().metadata()?;
        self.verify_current()?;
        Ok(metadata)
    }

    /// Run a bounded read against the same handle authorized at admission.
    /// Verify both path names again before releasing any result.
    pub fn with_file<T>(&self, read: impl FnOnce(&File) -> io::Result<T>) -> io::Result<T> {
        let _cursor = self.0.cursor.lock().map_err(|_| io::ErrorKind::Other)?;
        self.verify_current()?;
        let mut file = self.0.handle.as_file();
        let before = file.metadata()?;
        file.seek(SeekFrom::Start(0))?;
        let value = read(file)?;
        self.verify_current()?;
        let after = file.metadata()?;
        if before.len() != after.len()
            || before.modified()? != after.modified()?
            || !platform::unchanged(&before, &after)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(value)
    }

    fn verify_current(&self) -> io::Result<()> {
        let source = &self.0;
        if let Some(root) = &source.upload_root
            && (open_identity(&root.input, true)? != root.handle
                || !inside_root(root, &platform::opened_path(source.handle.as_file())?))
        {
            return Err(io::ErrorKind::InvalidData.into());
        }

        if open_identity(&source.input, false)? != source.handle
            || (source.input != Path::new(&source.path)
                && open_identity(Path::new(&source.path), false)? != source.handle)
        {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
}

fn wire_path(path: &Path) -> io::Result<&str> {
    path.to_str()
        .filter(|s| path.is_absolute() && s.trim() == *s)
        .filter(|s| {
            !s.chars()
                .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        })
        .ok_or_else(|| io::ErrorKind::InvalidInput.into())
}

fn open_identity(path: &Path, directory: bool) -> io::Result<Handle> {
    let mut options = OpenOptions::new();
    options.read(true);
    platform::configure_open(&mut options, directory);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !platform::is_disk_file(&file, &metadata, directory) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Handle::from_file(file)
}

fn inside_root(root: &UploadRoot, target: &Path) -> bool {
    let Some(parent) = target.parent() else {
        return false;
    };
    // Use the filesystem's identity for case and alias resolution. Comparing
    // strings alone can conflate distinct directories or miss case aliases.
    if parent.starts_with(&root.path) {
        return open_identity(&root.path, true).is_ok_and(|current| current == root.handle);
    }
    parent
        .ancestors()
        .any(|ancestor| open_identity(ancestor, true).is_ok_and(|current| current == root.handle))
}

#[cfg(test)]
#[path = "chat_attachment/tests.rs"]
mod tests;
