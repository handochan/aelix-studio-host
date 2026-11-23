//! Security descriptors for the existing managed upload directory.
use super::walk;
use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_REPARSE_POINT,
};
use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE, LocalFree};
use windows_sys::Win32::Security::Authorization::*;
use windows_sys::Win32::Security::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::SystemServices::*;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct LocalMemory(*mut c_void);
impl Drop for LocalMemory {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

// TokenUser contains a pointer into the returned, pointer-aligned allocation.
struct User(Vec<usize>);
impl User {
    fn current() -> io::Result<Self> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut len = 0;
        unsafe {
            GetTokenInformation(token.as_raw_handle(), TokenUser, ptr::null_mut(), 0, &mut len);
        }
        if (len as usize) < size_of::<TOKEN_USER>() {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; (len as usize).div_ceil(size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                storage.as_mut_ptr().cast(),
                len,
                &mut len,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(storage))
    }

    fn sid(&self) -> PSID {
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn descriptor(&self) -> io::Result<LocalMemory> {
        let mut text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(self.sid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _guard = LocalMemory(text.cast());
        // ConvertSidToStringSidW returns a NUL-terminated SID in LocalAlloc memory.
        let mut len = 0;
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        let sddl: Vec<_> = format!("O:{sid}D:P(A;OICI;FA;;;{sid})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut sd = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalMemory(sd))
    }
}

pub(in crate::chat_attachment) fn create_private_dir(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let (parent, _) = walk::open(parent, true, FILE_TRAVERSE | FILE_READ_ATTRIBUTES)?;
    private_child_dir(&parent, name)?;
    Ok(())
}

pub(super) fn private_child_dir(parent: &File, name: &OsStr) -> io::Result<File> {
    let descriptor = User::current()?.descriptor()?;
    let access = FILE_GENERIC_READ | FILE_TRAVERSE;
    let file = match walk::child(
        parent,
        name,
        access,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
        descriptor.0.cast(),
    ) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => walk::child(
            parent,
            name,
            access,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_OPEN_FOR_BACKUP_INTENT,
            ptr::null(),
        )?,
        Err(err) => return Err(err),
    };
    if !file.metadata()?.is_dir() || walk::is_surrogate(&file)? {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

pub(in crate::chat_attachment) fn verify_owner(file: &File) -> io::Result<()> {
    let user = User::current()?;
    let mut sd = ptr::null_mut();
    let mut owner = ptr::null_mut();
    let mut acl = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut acl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _guard = LocalMemory(sd);
    if owner.is_null() || acl.is_null() || unsafe { EqualSid(owner, user.sid()) } == 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    const WRITE: u32 = GENERIC_ALL
        | GENERIC_WRITE
        | WRITE_DAC
        | WRITE_OWNER
        | DELETE
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_ATTRIBUTES
        | FILE_WRITE_EA
        | FILE_DELETE_CHILD;
    // GetSecurityInfo returns a system-validated self-relative descriptor. All
    // ACE and SID pointers below borrow that allocation until _guard drops.
    for index in 0..unsafe { (*acl).AceCount } {
        let mut ace = ptr::null_mut();
        if unsafe { GetAce(acl, u32::from(index), &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        let kind = u32::from(header.AceType);
        match kind {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => continue,
            ACCESS_ALLOWED_ACE_TYPE
            | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
            | ACCESS_ALLOWED_OBJECT_ACE_TYPE
            | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE => {}
            _ => return Err(io::ErrorKind::PermissionDenied.into()),
        }
        if header.AceSize < 8 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let words = ace.cast::<u32>();
        if unsafe { *words.add(1) } & WRITE == 0 {
            continue;
        }
        let mut offset = 8;
        if matches!(
            kind,
            ACCESS_ALLOWED_OBJECT_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_OBJECT_ACE_TYPE
        ) {
            if header.AceSize < 12 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let flags = unsafe { *words.add(2) };
            offset = 12 + 16 * (flags & 3).count_ones() as usize;
        }
        if offset + 8 > usize::from(header.AceSize) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let sid_bytes = unsafe { ace.cast::<u8>().add(offset) };
        let sid_length = 8 + 4 * usize::from(unsafe { *sid_bytes.add(1) });
        let sid = sid_bytes.cast();
        if offset + sid_length > usize::from(header.AceSize) || unsafe { IsValidSid(sid) } == 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if unsafe { EqualSid(sid, user.sid()) } == 0 && unsafe { IsWellKnownSid(sid, WinCreatorOwnerRightsSid) } == 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(())
}
