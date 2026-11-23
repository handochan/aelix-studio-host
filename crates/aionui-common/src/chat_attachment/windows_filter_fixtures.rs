//! Hosted-Windows fixtures. Only the record belonging to the pinned fixture is
//! examined when a filesystem filter hides its reparse attribute from callers.
use super::*;
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::ptr;
use windows_sys::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE};
use windows_sys::Win32::Storage::CloudFilters::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::IO::{DeviceIoControl, OVERLAPPED};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_FILE_RECORD, NTFS_FILE_RECORD_INPUT_BUFFER, NTFS_FILE_RECORD_OUTPUT_BUFFER,
};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_sys::Win32::System::SystemServices::{IO_REPARSE_TAG_CLOUD, IO_REPARSE_TAG_WOF};

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}
fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

fn fixture_tag(path: &Path) -> u32 {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .unwrap();
    let mut attr = FILE_ATTRIBUTE_TAG_INFO::default();
    assert_ne!(
        unsafe {
            GetFileInformationByHandleEx(
                f.as_raw_handle(),
                FileAttributeTagInfo,
                (&raw mut attr).cast(),
                size_of_val(&attr) as u32,
            )
        },
        0
    );
    if attr.ReparseTag != 0 && attr.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return attr.ReparseTag;
    }
    // The WOF minifilter can hide the tag even on reparse-point queries. Ask
    // NTFS for this exact file's metadata. Never substitute a neighbouring record.
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    assert_ne!(unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) }, 0);
    let id = u64::from(info.nFileIndexHigh) << 32 | u64::from(info.nFileIndexLow);
    let mask = 0x0000_ffff_ffff_ffff;
    let vol = File::open(format!(r"\\.\{}:", platform::drive(path).unwrap())).unwrap();
    let input = NTFS_FILE_RECORD_INPUT_BUFFER {
        FileReferenceNumber: (id & mask) as i64,
    };
    let mut out = vec![0u8; 131072];
    let mut count = 0;
    let ok = unsafe {
        DeviceIoControl(
            vol.as_raw_handle(),
            FSCTL_GET_NTFS_FILE_RECORD,
            (&raw const input).cast(),
            size_of_val(&input) as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut count,
            ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "NTFS fixture query: {}", io::Error::last_os_error());
    out.truncate(count as usize);
    assert_eq!(u64::from_le_bytes(out[..8].try_into().unwrap()) & mask, id & mask);
    let start = std::mem::offset_of!(NTFS_FILE_RECORD_OUTPUT_BUFFER, FileRecordBuffer);
    let record = &out[start..start + u32_at(&out, 8) as usize];
    assert_eq!(&record[..4], b"FILE");
    assert_eq!(u64::from(u16_at(record, 16)), id >> 48);
    let mut off = usize::from(u16_at(record, 20));
    while u32_at(record, off) != u32::MAX {
        let length = u32_at(record, off + 4) as usize;
        assert!(length >= 24 && off + length <= record.len());
        if u32_at(record, off) == 0xc0 {
            // NTFS $REPARSE_POINT, resident fixture data
            assert_eq!(record[off + 8], 0);
            let data = usize::from(u16_at(record, off + 20));
            assert!(u32_at(record, off + 16) >= 8 && data + 8 <= length);
            return u32_at(record, off + data);
        }
        off += length;
    }
    panic!(
        "fixture has no NTFS reparse attribute (handle attributes {:#x})",
        attr.FileAttributes
    )
}

fn check_bytes(path: &Path, expected: &[u8]) {
    let file = ChatAttachment::open(path, None).unwrap();
    let data = file
        .with_file(|mut handle| {
            let mut data = Vec::new();
            handle.read_to_end(&mut data)?;
            Ok(data)
        })
        .unwrap();
    assert_eq!(data.len(), expected.len());
    assert!(data == expected, "validator returned different logical bytes");
}

#[test]
#[ignore = "requires compact.exe /c /exe:xpress4k on the Windows runner"]
fn windows_real_wof_tag_and_bytes() {
    let path = PathBuf::from(std::env::var_os("HOST60_WOF_FILE").expect("XPRESS4K fixture required"));
    let expected = fs::read(std::env::var_os("HOST60_WOF_REFERENCE").unwrap()).unwrap();
    let t = fixture_tag(&path);
    assert_eq!(t, IO_REPARSE_TAG_WOF);
    check_bytes(&path, &expected);
    println!("WOF tag {t:#010x}, {} XPRESS4K fixture bytes verified", expected.len());
}

struct Library(HMODULE);
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            FreeLibrary(self.0);
        }
    }
}
type Register = unsafe extern "system" fn(*const u16, *const CF_SYNC_REGISTRATION, *const CF_SYNC_POLICIES, u32) -> i32;
type Convert = unsafe extern "system" fn(HANDLE, *const c_void, u32, i32, *mut i64, *mut OVERLAPPED) -> i32;
type Unregister = unsafe extern "system" fn(*const u16) -> i32;
struct Registration<'a>(&'a [u16], Unregister);
impl Drop for Registration<'_> {
    fn drop(&mut self) {
        let hr = unsafe { (self.1)(self.0.as_ptr()) };
        assert!(hr >= 0, "CfUnregisterSyncRoot HRESULT={hr:#010x}");
    }
}

#[test]
#[ignore = "registers a private test sync root on a disposable Windows volume"]
fn windows_real_hydrated_cloud_placeholder() {
    let base = PathBuf::from(std::env::var_os("HOST60_FILTER_BASE").unwrap());
    let dir = tempfile::tempdir_in(base).unwrap();
    let file = dir.path().join("hydrated.bin");
    let expected = b"already hydrated local fixture bytes";
    fs::write(&file, expected).unwrap();
    // Load the optional Cloud Files platform only in this fixture.
    let library = Library(unsafe {
        LoadLibraryExW(
            wide(OsStr::new("cldapi.dll")).as_ptr(),
            ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    });
    if library.0.is_null() {
        let code = io::Error::last_os_error().raw_os_error().unwrap();
        println!(
            "::notice::Cloud Files registration API unavailable: LoadLibraryExW Win32={code}, HRESULT_FROM_WIN32={:#010x}",
            0x80070000u32 | code as u32
        );
        return;
    }
    // All function signatures match cfapi.h. The module outlives every call.
    let register: Register =
        unsafe { std::mem::transmute(GetProcAddress(library.0, c"CfRegisterSyncRoot".as_ptr().cast()).unwrap()) };
    let convert: Convert =
        unsafe { std::mem::transmute(GetProcAddress(library.0, c"CfConvertToPlaceholder".as_ptr().cast()).unwrap()) };
    let unregister: Unregister =
        unsafe { std::mem::transmute(GetProcAddress(library.0, c"CfUnregisterSyncRoot".as_ptr().cast()).unwrap()) };
    let name = wide(OsStr::new("H-2 fixture"));
    let version = wide(OsStr::new("1.0"));
    let root = wide(dir.path().as_os_str());
    let id = uuid::Uuid::now_v7();
    let reg = CF_SYNC_REGISTRATION {
        StructSize: size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: name.as_ptr(),
        ProviderVersion: version.as_ptr(),
        SyncRootIdentity: id.as_bytes().as_ptr().cast(),
        SyncRootIdentityLength: 16,
        ProviderId: windows_sys::core::GUID::from_u128(id.as_u128()),
        ..Default::default()
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_ALWAYS_FULL,
            Modifier: 0,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
            Modifier: 0,
        },
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_CONVERT_TO_UNRESTRICTED,
        ..Default::default()
    };
    let hr = unsafe { register(root.as_ptr(), &reg, &policies, 0) };
    if hr < 0 {
        println!("::notice::CfRegisterSyncRoot HRESULT={hr:#010x}; real Cloud Files conversion unavailable");
        return;
    }
    let _registered = Registration(&root, unregister);
    let handle = std::fs::OpenOptions::new().read(true).write(true).open(&file).unwrap();
    let hr = unsafe {
        convert(
            handle.as_raw_handle(),
            b"H2".as_ptr().cast(),
            2,
            CF_CONVERT_FLAG_MARK_IN_SYNC,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert!(hr >= 0, "CfConvertToPlaceholder HRESULT={hr:#010x}");
    type Info = unsafe extern "system" fn(HANDLE, CF_PLACEHOLDER_INFO_CLASS, *mut c_void, u32, *mut u32) -> i32;
    let info: Info =
        unsafe { std::mem::transmute(GetProcAddress(library.0, c"CfGetPlaceholderInfo".as_ptr().cast()).unwrap()) };
    let mut basic = [0usize; 32];
    let mut used = 0;
    let hr = unsafe {
        info(
            handle.as_raw_handle(),
            CF_PLACEHOLDER_INFO_BASIC,
            basic.as_mut_ptr().cast(),
            size_of_val(&basic) as u32,
            &mut used,
        )
    };
    assert!(hr >= 0, "CfGetPlaceholderInfo HRESULT={hr:#010x}");
    assert!(used as usize >= std::mem::offset_of!(CF_PLACEHOLDER_BASIC_INFO, FileIdentity));
    assert_eq!(
        unsafe { (*basic.as_ptr().cast::<CF_PLACEHOLDER_BASIC_INFO>()).InSyncState },
        CF_IN_SYNC_STATE_IN_SYNC
    );
    drop(handle);
    let t = fixture_tag(&file);
    assert_eq!(t & !0xf000, IO_REPARSE_TAG_CLOUD);
    check_bytes(&file, expected);
    println!("Cloud Files tag {t:#010x}, in-sync hydrated bytes verified");
}
