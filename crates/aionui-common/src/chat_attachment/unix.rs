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

pub(super) fn open_path(path: &Path, directory: bool) -> io::Result<(File, PathBuf)> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_open(&mut options, directory);
    Ok((options.open(path)?, path.to_owned()))
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        result => result,
    }
}

pub(super) fn verify_owner(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn child_name(name: &std::ffi::OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    super::check_child_name(name)?;
    std::ffi::CString::new(name.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput.into())
}

fn open_at(parent: &File, name: &std::ffi::CStr, flags: i32) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: the descriptor and C string remain valid for openat and the
    // newly returned owned descriptor is transferred to File once.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn create_child_dir(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    use std::os::fd::AsRawFd;
    let name = child_name(name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY)
}

pub(super) fn create_child_file(parent: &File, name: &std::ffi::OsStr) -> io::Result<File> {
    open_at(
        parent,
        &child_name(name)?,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
    )
}
