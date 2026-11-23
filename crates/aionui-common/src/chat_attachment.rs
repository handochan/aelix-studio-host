//! File identity at the Host's Local and Upload attachment boundary.

use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use same_file::Handle;

#[cfg(windows)]
#[path = "chat_attachment/windows.rs"]
mod platform;
#[cfg(unix)]
#[path = "chat_attachment/unix.rs"]
mod platform;

#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    #[error("The managed upload directory is unavailable.")]
    UntrustedRoot,
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
    handle: Handle,
}

/// One pinned, authenticated directory per request. Existing refs keep their location.
#[derive(Clone)]
pub struct ManagedUploadRoot(Arc<UploadRoot>);

impl ManagedUploadRoot {
    pub fn open(input: &Path) -> io::Result<Self> {
        let open = || {
            let handle = match open_identity(input, true) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    platform::create_private_dir(input)?;
                    open_identity(input, true)?
                }
                other => other?,
            };
            platform::verify_owner(handle.as_file())?;
            Ok(Self(Arc::new(UploadRoot {
                input: input.to_owned(),
                handle,
            })))
        };
        open().inspect_err(|_: &io::Error| root_warning())
    }

    pub fn verify_current(&self) -> io::Result<()> {
        let verify = || {
            if open_identity(&self.0.input, true)? != self.0.handle {
                return Err(io::ErrorKind::InvalidData.into());
            }
            platform::verify_owner(self.0.handle.as_file())
        };
        verify().inspect_err(|_| root_warning())
    }

    pub fn destination(&self, name: &OsStr) -> io::Result<UploadDirectory> {
        check_child_name(name)?;
        self.verify_current()?;
        let directory = platform::create_child_dir(self.0.handle.as_file(), name)?;
        Ok(UploadDirectory {
            root: self.clone(),
            directory,
            path: self.0.input.join(name),
        })
    }
}

fn root_warning() {
    tracing::warn!(target: "chat_file", code = "UPLOAD_ROOT_UNTRUSTED", "managed upload directory is unavailable");
}

pub struct UploadDirectory {
    root: ManagedUploadRoot,
    directory: File,
    path: PathBuf,
}

impl UploadDirectory {
    pub fn create_new(&self, name: &OsStr) -> io::Result<(File, PathBuf)> {
        check_child_name(name)?;
        self.root.verify_current()?;
        let file = platform::create_child_file(&self.directory, name)?;
        Ok((file, self.path.join(name)))
    }
}

fn check_child_name(name: &OsStr) -> io::Result<()> {
    let path = Path::new(name);
    if path.file_name() != Some(name) || name.as_encoded_bytes().contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    #[cfg(windows)]
    if name.as_encoded_bytes().contains(&b'/') || !platform::is_supported_path(&Path::new(r"C:\").join(name)) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(())
}

pub const MAX_INLINE_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Paths ineligible at admission carry no file descriptor during the turn.
/// Repeated references share one open file. Once dispatch selects path fallback,
/// all turn/replay clones release that occurrence. Only inline files stay pinned
/// for a possible authorized replay. Uploads in one message share a root.
#[derive(Debug, Clone)]
pub enum PromptAttachment {
    Inline(Arc<Mutex<Option<ChatAttachment>>>),
    PathOnly,
    Project,
}

impl PromptAttachment {
    pub fn inline(file: ChatAttachment) -> Self {
        Self::Inline(Arc::new(Mutex::new(Some(file))))
    }

    pub fn pinned(&self) -> Option<ChatAttachment> {
        match self {
            Self::Inline(slot) => slot.lock().unwrap().clone(),
            _ => None,
        }
    }

    pub fn release(&self) {
        if let Self::Inline(slot) = self {
            slot.lock().unwrap().take();
        }
    }

    /// Duplicate positions share the OS identity but may independently fall
    /// on opposite sides of the request byte budget.
    pub fn repeat(&self) -> Self {
        match self {
            Self::Inline(_) => self.pinned().map_or(Self::PathOnly, Self::inline),
            other => other.clone(),
        }
    }
}

pub fn inline_media_mime(path: &str) -> Option<mime_guess::Mime> {
    mime_guess::from_path(path)
        .first()
        .filter(|mime| matches!(mime.type_().as_str(), "audio" | "image") && mime.subtype() != "svg")
}

struct Source {
    input: PathBuf,
    path: String,
    handle: Handle,
    upload_root: Option<ManagedUploadRoot>,
    admitted_length: u64,
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
        let cache = OnceLock::new();
        Self::open_cached(input, upload_root.map(|root| (root, &cache)))
    }

    /// Reject invalid wire paths before touching even a mapped upload root.
    /// The cache is private to this request and shares the trusted directory handle.
    pub fn open_cached(
        input: &Path,
        root: Option<(&Path, &OnceLock<io::Result<ManagedUploadRoot>>)>,
    ) -> Result<Self, AttachmentError> {
        if wire_path(input).is_err() || !platform::is_supported_path(input) {
            return Err(AttachmentError::Unavailable);
        }
        let upload_root = match root {
            Some((path, cache)) => Some(
                cache
                    .get_or_init(|| ManagedUploadRoot::open(path))
                    .as_ref()
                    .map_err(|_| AttachmentError::UntrustedRoot)?
                    .clone(),
            ),
            None => None,
        };
        let (file, anchor) = open_input(input, upload_root.as_ref())?;
        let metadata = file.metadata()?;
        if !platform::is_disk_file(&file, &metadata, false) {
            return Err(AttachmentError::Unavailable);
        }
        let handle = Handle::from_file(file)?;
        if let Some(root) = &upload_root
            && !platform::contains_handle(&root.0.handle, &handle)?
        {
            return Err(AttachmentError::OutsideUploadRoot);
        }
        tracing::trace!(target: "chat_file", code = "ATTACHMENT_OPENED", "opened attachment identity");
        let path = platform::wire_target(&anchor, platform::opened_path(handle.as_file())?)?;
        let path = wire_path(&path)?.to_owned();
        let attachment = Self(Arc::new(Source {
            input: input.to_owned(),
            path,
            handle,
            upload_root,
            admitted_length: metadata.len(),
            cursor: Mutex::new(()),
        }));
        attachment.verify_current()?;
        Ok(attachment)
    }

    pub fn path(&self) -> &str {
        &self.0.path
    }

    pub fn admitted_length(&self) -> u64 {
        self.0.admitted_length
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
        if let Some(root) = &source.upload_root {
            root.verify_current()?;
            let (current, _) = open_input(&source.input, Some(root)).map_err(|error| match error {
                AttachmentError::OutsideUploadRoot => io::ErrorKind::PermissionDenied,
                _ => io::ErrorKind::InvalidData,
            })?;
            if Handle::from_file(current)? != source.handle
                || !platform::contains_handle(&root.0.handle, &source.handle)?
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            return Ok(());
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
    let (file, _) = platform::open_path(path, directory)?;
    let metadata = file.metadata()?;
    if !platform::is_disk_file(&file, &metadata, directory) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Handle::from_file(file)
}

fn open_input(input: &Path, root: Option<&ManagedUploadRoot>) -> Result<(File, PathBuf), AttachmentError> {
    match root {
        Some(root) => platform::open_upload(input, &root.0.handle),
        None => Ok(platform::open_path(input, false)?),
    }
}

#[cfg(test)]
#[path = "chat_attachment/tests.rs"]
mod tests;
