use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) fn is_supported_path(_: &Path) -> bool {
    true
}

pub(super) fn configure_open(options: &mut OpenOptions, directory: bool) {
    // The shared temp root itself must be a directory, and a FIFO substituted
    // for an attachment must not stall admission or identity checks.
    let flags = if directory {
        libc::O_DIRECTORY | libc::O_NOFOLLOW
    } else {
        libc::O_NONBLOCK | libc::O_NOCTTY
    };
    options.custom_flags(flags);
}

pub(super) fn is_disk_file(_: &File, metadata: &Metadata, directory: bool) -> bool {
    if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    }
}

pub(super) fn wire_target(_: &Path, target: PathBuf) -> io::Result<PathBuf> {
    Ok(target)
}

#[cfg(target_os = "macos")]
pub(super) fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let mut path = [0 as libc::c_char; libc::MAXPATHLEN as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path to a MAXPATHLEN buffer.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let bytes = unsafe { CStr::from_ptr(path.as_ptr()) }.to_bytes();
    Ok(PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

pub(super) fn unchanged(before: &Metadata, after: &Metadata) -> bool {
    (before.ctime(), before.ctime_nsec()) == (after.ctime(), after.ctime_nsec())
}
