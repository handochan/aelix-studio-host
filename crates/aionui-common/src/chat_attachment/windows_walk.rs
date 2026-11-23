//! Walk from a permitted drive root using one handle-relative NT open per name.
//! A concurrently replaced ancestor cannot introduce a new traversal.
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf};
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::*;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::SECURITY_DESCRIPTOR;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK};

use super::{drive, is_supported_path};

const NAME_SURROGATE: u32 = 1 << 29;
const MAX_LINKS: usize = 32;
const PROBE: u32 = FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT;
const DIR_ACCESS: u32 = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

#[cfg(test)]
pub(in crate::chat_attachment) struct ObservedOpen {
    pub(in crate::chat_attachment) name: OsString,
    pub(in crate::chat_attachment) no_follow: bool,
    pub(in crate::chat_attachment) backup_semantics: bool,
}

#[cfg(test)]
thread_local! {
    pub(in crate::chat_attachment) static OPENS: std::cell::RefCell<Vec<ObservedOpen>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[path = "windows_roots.rs"]
mod roots;
pub(super) use roots::volume_name;

// Each call names exactly one child of an already opened parent. Do not pass
// absolute names, separators, device names or stream names to NtCreateFile.
pub(super) fn child(
    parent: &File,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
    descriptor: *const SECURITY_DESCRIPTOR,
) -> io::Result<File> {
    super::super::check_child_name(name)?;
    nt_open(Some(parent), name, access, disposition, options, descriptor)
}

// Absolute names here are verified NT device roots. Every other call uses a
// single component relative to a pinned parent.
fn nt_open(
    parent: Option<&File>,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
    descriptor: *const SECURITY_DESCRIPTOR,
) -> io::Result<File> {
    #[cfg(test)]
    OPENS.with_borrow_mut(|opens| {
        opens.push(ObservedOpen {
            name: name.into(),
            no_follow: options & FILE_OPEN_REPARSE_POINT != 0,
            backup_semantics: options & FILE_OPEN_FOR_BACKUP_INTENT != 0,
        })
    });
    let mut wide: Vec<_> = name.encode_wide().collect();
    let length = u16::try_from(wide.len() * 2).map_err(|_| io::ErrorKind::InvalidInput)?;
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.map_or(ptr::null_mut(), AsRawHandle::as_raw_handle),
        ObjectName: &name,
        // The probe opens the final reparse point itself. Never let the IO
        // manager restart name lookup, even if a tag changes before the
        // subsequent filter-managed data open.
        Attributes: OBJ_CASE_INSENSITIVE | OBJ_DONT_REPARSE,
        SecurityDescriptor: descriptor,
        ..Default::default()
    };
    let mut handle = ptr::null_mut();
    let mut iosb = IO_STATUS_BLOCK::default();
    // SAFETY: attributes, descriptor, name and status storage live across the
    // synchronous call. The returned handle is transferred to File exactly once.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut iosb,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn tag(file: &File) -> io::Result<u32> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: info is valid writable storage of the requested information class.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        0
    } else {
        info.ReparseTag
    })
}

fn substitute(file: &File, expected: u32, parent: &Path) -> io::Result<PathBuf> {
    let mut buffer = [0u8; 16 * 1024];
    let mut count = 0;
    // SAFETY: the buffer is writable for the supplied length; no overlapped IO.
    if unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_GET_REPARSE_POINT,
            ptr::null(),
            0,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut count,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let data = &buffer[..count as usize];
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    let u16_at = |i| {
        data.get(i..i + 2)
            .map(|v| u16::from_le_bytes([v[0], v[1]]) as usize)
            .ok_or_else(invalid)
    };
    let u32_at = |i| {
        data.get(i..i + 4)
            .map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]))
            .ok_or_else(invalid)
    };
    if u32_at(0)? != expected || 8 + u16_at(4)? > data.len() {
        return Err(invalid());
    }
    let (offset, relative) = match expected {
        IO_REPARSE_TAG_SYMLINK => (20, u32_at(16)? == 1),
        IO_REPARSE_TAG_MOUNT_POINT => (16, false),
        _ => return Err(invalid()),
    };
    let start = offset + u16_at(8)?;
    let size = u16_at(10)?;
    if size == 0 || size % 2 != 0 || start % 2 != 0 || start + size > 8 + u16_at(4)? {
        return Err(invalid());
    }
    let wide: Vec<_> = data[start..start + size]
        .chunks_exact(2)
        .map(|v| u16::from_le_bytes([v[0], v[1]]))
        .collect();
    let raw = OsString::from_wide(&wide);
    let raw = raw.to_str().ok_or_else(invalid)?;
    link_target(raw, relative, parent, expected == IO_REPARSE_TAG_MOUNT_POINT)
}

fn link_target(raw: &str, relative: bool, parent: &Path, mount: bool) -> io::Result<PathBuf> {
    let path = if relative {
        let path = Path::new(raw);
        if path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        parent.join(path)
    } else {
        // The substitute name (not the display/print name) controls traversal.
        match raw.strip_prefix(r"\??\") {
            Some(nt) => PathBuf::from(format!(r"\\?\{nt}")),
            None => PathBuf::from(raw),
        }
    };
    if !(is_supported_path(&path)
        || (mount || relative && volume_name(parent).is_some()) && volume_name(&path).is_some())
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(path)
}

pub(super) fn open(input: &Path, follow_leaf: bool, leaf_access: u32) -> io::Result<(File, PathBuf)> {
    let mut path = input.to_owned();
    for hop in 0..=MAX_LINKS {
        let start = roots::resolve(&path, hop != 0)?;
        let (mut prefix, mut parent) = (start.prefix, start.root);
        let mut parts = start.names.iter().peekable();
        if parts.peek().is_none() {
            return Ok((parent, prefix));
        }
        while let Some(name) = parts.next() {
            let last = parts.peek().is_none();
            let access = if last { leaf_access } else { DIR_ACCESS };
            let probe = child(&parent, name, access, FILE_OPEN, PROBE, ptr::null())?;
            let reparse = tag(&probe)?;
            if reparse & NAME_SURROGATE != 0 {
                if last && !follow_leaf {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                let mut next = substitute(&probe, reparse, &prefix)?;
                for part in parts {
                    next.push(part);
                }
                path = next;
                break;
            }
            let opened = data_handle(&parent, name, access, probe, reparse)?;
            prefix.push(name);
            if last {
                return Ok((
                    opened,
                    if volume_name(&prefix).is_some() {
                        input.to_owned()
                    } else {
                        prefix
                    },
                ));
            }
            parent = opened;
        }
    }
    Err(io::ErrorKind::InvalidData.into())
}

fn data_handle(parent: &File, name: &OsStr, access: u32, probe: File, reparse: u32) -> io::Result<File> {
    if reparse == 0 {
        return Ok(probe);
    }
    if reparse & NAME_SURROGATE != 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    // Give non-name-surrogate providers their normal data-open semantics,
    // anchored to this parent with name reparsing disabled.
    let filtered = child(
        parent,
        name,
        access,
        FILE_OPEN,
        FILE_OPEN_FOR_BACKUP_INTENT,
        ptr::null(),
    )?;
    if same_file::Handle::from_file(probe)? != same_file::Handle::from_file(filtered.try_clone()?)? {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(filtered)
}

pub(super) fn is_surrogate(file: &File) -> io::Result<bool> {
    Ok(tag(file)? & NAME_SURROGATE != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_surrogate_tags_select_normal_data_opens() {
        use std::io::Read;
        use windows_sys::Win32::System::SystemServices::{
            IO_REPARSE_TAG_CLOUD, IO_REPARSE_TAG_DEDUP, IO_REPARSE_TAG_WOF,
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data"), b"logical data").unwrap();
        let (parent, _) = open(dir.path(), true, DIR_ACCESS).unwrap();
        let name = OsStr::new("data");
        // Inject only the tag. Both the probe and data open are real NT opens.
        // A separate provisioned test reads an actual filter-managed file.
        for reparse in [IO_REPARSE_TAG_CLOUD, IO_REPARSE_TAG_DEDUP, IO_REPARSE_TAG_WOF] {
            OPENS.with_borrow_mut(Vec::clear);
            let probe = child(&parent, name, FILE_GENERIC_READ, FILE_OPEN, PROBE, ptr::null()).unwrap();
            let mut file = data_handle(&parent, name, FILE_GENERIC_READ, probe, reparse).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"logical data");
            OPENS.with_borrow(|opens| {
                assert_eq!(opens.len(), 2);
                assert!(opens[0].no_follow && !opens[1].no_follow);
                assert!(opens.iter().all(|o| o.name == name && o.backup_semantics));
            });
        }
    }

    #[test]
    fn substitute_names_must_resolve_to_a_drive() {
        let parent = Path::new(r"C:\folder");
        for raw in [
            r"\\server\share\leaf",
            r"\??\UNC\server\share\leaf",
            r"\Device\Mup\server\share",
            r"\\?\GLOBALROOT\Device\Mup\server\share",
            r"\??\Volume{1234}\leaf",
            r"Z:leaf",
        ] {
            assert!(link_target(raw, false, parent, false).is_err(), "{raw}");
        }
        assert!(link_target(r"\??\Z:\leaf", false, parent, false).is_ok());
        assert!(link_target(r"..\other\leaf", true, parent, false).is_ok());
        assert!(link_target(r"\\server\share\leaf", true, parent, false).is_err());
    }
}

#[cfg(test)]
mod swap_tests {
    use super::*;

    #[test]
    fn kernel_stops_a_link_inserted_between_probe_and_filter_open() {
        let dir = tempfile::tempdir().unwrap();
        let selected = dir.path().join("selected");
        std::fs::write(&selected, b"original").unwrap();
        std::fs::write(dir.path().join("other"), b"fixture").unwrap();
        let (parent, _) = open(dir.path(), true, DIR_ACCESS).unwrap();
        let name = OsStr::new("selected");
        let probe = child(&parent, name, FILE_GENERIC_READ, FILE_OPEN, PROBE, ptr::null()).unwrap();
        std::fs::rename(&selected, dir.path().join("pinned")).unwrap();
        std::os::windows::fs::symlink_file("other", &selected).unwrap();
        OPENS.with_borrow_mut(Vec::clear);
        let err = data_handle(
            &parent,
            name,
            FILE_GENERIC_READ,
            probe,
            windows_sys::Win32::System::SystemServices::IO_REPARSE_TAG_CLOUD,
        )
        .unwrap_err();
        assert_eq!(
            err.raw_os_error(),
            Some(unsafe { RtlNtStatusToDosError(STATUS_REPARSE_POINT_ENCOUNTERED) } as i32),
            "expected the kernel's reparse rejection before the post-open identity check: {err}"
        );
        OPENS.with_borrow(|opens| {
            assert_eq!(opens.len(), 1);
            assert_eq!(opens[0].name, name);
            assert!(!opens[0].no_follow);
        });
    }
}
