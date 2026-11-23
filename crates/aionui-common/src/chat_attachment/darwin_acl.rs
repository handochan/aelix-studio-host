//! Darwin extended ACLs are independent of BSD group-write mode bits.
use std::ffi::{CString, c_int, c_void};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

// Darwin SDK <sys/acl.h>, <sys/fcntl.h> and <membership.h>.
const EXTENDED: c_int = 0x100;
const ALLOW: c_int = 1;
const DENY: c_int = 2;
// Write/add data, delete, append/add subdirectory, delete child, write
// attributes and extended attributes, write security, and change owner.
const WRITE: u64 = (1 << 2) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 8) | (1 << 10) | (1 << 12) | (1 << 13);
type Opaque = *mut c_void;
unsafe extern "C" {
    fn acl_init(count: c_int) -> Opaque;
    fn acl_free(obj: Opaque) -> c_int;
    fn acl_get_fd_np(fd: c_int, kind: c_int) -> Opaque;
    fn acl_set_fd_np(fd: c_int, acl: Opaque, kind: c_int) -> c_int;
    fn acl_get_entry(acl: Opaque, index: c_int, entry: *mut Opaque) -> c_int;
    fn acl_get_tag_type(entry: Opaque, tag: *mut c_int) -> c_int;
    fn acl_get_permset_mask_np(entry: Opaque, mask: *mut u64) -> c_int;
    fn acl_get_qualifier(entry: Opaque) -> Opaque;
    fn acl_get_flagset_np(obj: Opaque, flags: *mut Opaque) -> c_int;
    fn acl_add_flag_np(flags: Opaque, flag: c_int) -> c_int;
    fn mbr_uid_to_uuid(uid: libc::uid_t, uuid: *mut u8) -> c_int;
    fn filesec_init() -> Opaque;
    fn filesec_set_property(sec: Opaque, prop: c_int, value: *const c_void) -> c_int;
    fn filesec_free(sec: Opaque);
    fn mkdirx_np(path: *const libc::c_char, sec: Opaque) -> c_int;
}

struct AclMemory(Opaque);
impl Drop for AclMemory {
    fn drop(&mut self) {
        unsafe {
            acl_free(self.0);
        }
    }
}

struct FileSecurity(Opaque);
impl Drop for FileSecurity {
    fn drop(&mut self) {
        unsafe {
            filesec_free(self.0);
        }
    }
}

fn allocated(p: Opaque) -> io::Result<Opaque> {
    if p.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(p)
    }
}

fn check(result: c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn owner_uuid(uid: libc::uid_t) -> io::Result<[u8; 16]> {
    let mut id = [0; 16];
    let status = unsafe { mbr_uid_to_uuid(uid, id.as_mut_ptr()) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    Ok(id)
}

fn entries(file: &File, mut visit: impl FnMut(Opaque) -> io::Result<()>) -> io::Result<()> {
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // Darwin filesec reports ENOENT when this open object has no ACL.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let acl = AclMemory(acl);
    let mut selector = 0; // ACL_FIRST_ENTRY
    loop {
        let mut entry = ptr::null_mut();
        if unsafe { acl_get_entry(acl.0, selector, &mut entry) } != 0 {
            let error = io::Error::last_os_error();
            // Darwin acl_get_entry returns -1/EINVAL at the end of a valid ACL.
            return if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(())
            } else {
                Err(error)
            };
        }
        visit(entry)?;
        selector = -1; // ACL_NEXT_ENTRY
    }
}

pub(super) fn verify(file: &File, uid: libc::uid_t) -> io::Result<()> {
    entries(file, |entry| {
        let mut tag = 0;
        check(unsafe { acl_get_tag_type(entry, &mut tag) })?;
        if tag == DENY {
            return Ok(());
        }
        if tag != ALLOW {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let mut mask = 0;
        check(unsafe { acl_get_permset_mask_np(entry, &mut mask) })?;
        if mask & WRITE != 0 {
            let qualifier = AclMemory(allocated(unsafe { acl_get_qualifier(entry) })?);
            // An extended allow ACE's qualifier is a freshly allocated guid_t.
            if unsafe { *qualifier.0.cast::<[u8; 16]>() } != owner_uuid(uid)? {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        Ok(())
    })
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    const FILESEC_MODE: c_int = 4;
    const FILESEC_ACL: c_int = 5;
    const NO_INHERIT: c_int = 1 << 17;
    let name = CString::new(path.as_os_str().as_bytes())?;
    let acl = AclMemory(allocated(unsafe { acl_init(0) })?);
    let sec = FileSecurity(allocated(unsafe { filesec_init() })?);
    let mut flags = ptr::null_mut();
    check(unsafe { acl_get_flagset_np(acl.0, &mut flags) })?;
    check(unsafe { acl_add_flag_np(flags, NO_INHERIT) })?;
    let mode: libc::mode_t = 0o700;
    check(unsafe { filesec_set_property(sec.0, FILESEC_MODE, (&raw const mode).cast()) })?;
    check(unsafe { filesec_set_property(sec.0, FILESEC_ACL, (&raw const acl.0).cast()) })?;
    // Publish a private directory atomically, even under an inheritable ACL.
    if unsafe { mkdirx_np(name.as_ptr(), sec.0) } != 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(e)
        };
    }
    let (file, _) = super::open_path(path, true)?;
    super::verify_mode(&file)?;
    // Never erase ACEs on an existing object substituted for the new name.
    // mkdirx_np installed a non-inheriting empty ACL on our new object.
    entries(&file, |_| Err(io::ErrorKind::InvalidData.into()))?;
    check(unsafe { acl_set_fd_np(file.as_raw_fd(), acl.0, EXTENDED) })?;
    super::verify_owner(&file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_attachment::ManagedUploadRoot;
    unsafe extern "C" {
        fn acl_create_entry(acl: *mut Opaque, entry: *mut Opaque) -> c_int;
        fn acl_set_tag_type(entry: Opaque, tag: c_int) -> c_int;
        fn acl_set_qualifier(entry: Opaque, guid: *const c_void) -> c_int;
        fn acl_set_permset_mask_np(entry: Opaque, mask: u64) -> c_int;
    }
    fn grant(file: &File, guid: [u8; 16], mask: u64, inherit: bool) {
        let mut acl = AclMemory(allocated(unsafe { acl_init(1) }).unwrap());
        let mut entry = ptr::null_mut();
        check(unsafe { acl_create_entry(&mut acl.0, &mut entry) }).unwrap();
        check(unsafe { acl_set_tag_type(entry, ALLOW) }).unwrap();
        check(unsafe { acl_set_qualifier(entry, guid.as_ptr().cast()) }).unwrap();
        check(unsafe { acl_set_permset_mask_np(entry, mask) }).unwrap();
        if inherit {
            let mut flags = ptr::null_mut();
            check(unsafe { acl_get_flagset_np(entry, &mut flags) }).unwrap();
            check(unsafe { acl_add_flag_np(flags, 1 << 6) }).unwrap(); // DIRECTORY_INHERIT
        }
        check(unsafe { acl_set_fd_np(file.as_raw_fd(), acl.0, EXTENDED) }).unwrap();
    }
    #[test]
    fn adoption_recheck_and_new_root_under_inheritable_acl() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = owner_uuid(if unsafe { libc::geteuid() } == 1 { 0 } else { 1 }).unwrap();
        let parent = File::open(dir.path()).unwrap();
        grant(&parent, foreign, WRITE, true);
        let inherited = dir.path().join("inherited");
        std::fs::create_dir(&inherited).unwrap();
        let existing = File::open(&inherited).unwrap();
        assert_eq!(
            verify(&existing, unsafe { libc::geteuid() }).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(ManagedUploadRoot::open(&inherited).is_err());

        let path = dir.path().join("new");
        let root = ManagedUploadRoot::open(&path).unwrap();
        let file = File::open(path).unwrap();
        entries(&file, |_| panic!("new root inherited an ACE")).unwrap();
        let owner = owner_uuid(unsafe { libc::geteuid() }).unwrap();
        grant(&file, owner, WRITE, false);
        root.verify_current().unwrap();
        grant(&file, foreign, 1 << 1, false);
        root.verify_current().unwrap();
        for bit in [2, 4, 5, 6, 8, 10, 12, 13] {
            grant(&file, foreign, 1 << bit, false);
            assert_eq!(
                root.verify_current().unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
    }
}
