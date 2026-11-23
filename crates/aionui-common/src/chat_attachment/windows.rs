#[path = "windows_security.rs"]
mod security;
#[path = "windows_walk.rs"]
pub(super) mod walk;
pub(super) use security::{create_private_dir, verify_owner};

use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::windows::ffi::OsStringExt;

use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW};

pub(super) fn drive(path: &Path) -> Option<char> {
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => Some(char::from(drive)),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn is_supported_path(path: &Path) -> bool {
    path.is_absolute()
        && drive(path).is_some()
        && path.components().all(|component| match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                let stem = name.split('.').next().unwrap_or_default().trim_end();
                let stem = stem.to_ascii_uppercase();
                !name.contains(':')
                    && !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
                    && !stem
                        .strip_prefix("COM")
                        .or_else(|| stem.strip_prefix("LPT"))
                        .is_some_and(|n| {
                            matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
                        })
            }
            _ => true,
        })
}

pub(super) fn open_path(path: &Path, directory: bool) -> io::Result<(File, PathBuf)> {
    walk::open(
        path,
        !directory,
        windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ,
    )
}

pub(super) fn create_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
    security::private_child_dir(parent, name)
}

pub(super) fn create_child_file(parent: &File, name: &OsStr) -> io::Result<File> {
    use windows_sys::Wdk::Storage::FileSystem::{FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN_REPARSE_POINT};
    walk::child(
        parent,
        name,
        windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE,
        FILE_CREATE,
        FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE,
        std::ptr::null(),
    )
}

pub(super) fn is_disk_file(file: &File, metadata: &Metadata, directory: bool) -> bool {
    // SAFETY: the handle is owned by the borrowed File.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return false;
    }
    if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    }
}

pub(super) fn opened_path(file: &File) -> io::Result<PathBuf> {
    let mut flags = 0;
    let mut path = vec![0; 512];
    loop {
        // SAFETY: the File owns the handle and path has length writable WCHARs.
        let length =
            unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), path.as_mut_ptr(), path.len() as u32, flags) }
                as usize;
        if length == 0 {
            let err = io::Error::last_os_error();
            if flags == 0 && err.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_PATH_NOT_FOUND as i32) {
                flags = windows_sys::Win32::Storage::FileSystem::VOLUME_NAME_GUID;
                continue;
            }
            return Err(err);
        }
        if length < path.len() {
            return Ok(OsString::from_wide(&path[..length]).into());
        }
        path.resize(length + 1, 0);
    }
}

pub(super) fn wire_target(input: &Path, target: PathBuf) -> io::Result<PathBuf> {
    if is_supported_path(&target) {
        return Ok(target);
    }

    if walk::volume_name(&target).is_some() && is_supported_path(input) {
        // A mounted volume can have no drive letter. Keep the checked mount
        // spelling; verify_current resolves it again and compares the handle.
        return std::path::absolute(input);
    }
    // The final path of a mapped drive can be UNC. Keep only a target under
    // that drive's own root, and hand out the equivalent drive-letter path.
    let letter = drive(input).ok_or(io::ErrorKind::InvalidInput)?;
    let root_path = PathBuf::from(format!("{letter}:\\"));
    let (root, _) = open_path(&root_path, true)?;
    let root = opened_path(&root)?;
    let relative = target.strip_prefix(root).map_err(|_| io::ErrorKind::InvalidData)?;
    let path = root_path.join(relative);
    if !is_supported_path(&path) || relative.as_os_str().is_empty() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(path)
}

pub(super) fn unchanged(_: &Metadata, _: &Metadata) -> bool {
    true
}
