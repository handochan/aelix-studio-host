use super::*;
use std::fs;
use std::io::Read;

fn assert_unavailable(result: Result<ChatAttachment, AttachmentError>) {
    assert!(matches!(result, Err(AttachmentError::Unavailable)));
}

#[test]
fn local_and_upload_require_absolute_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("Workspace");
    let upload_root = dir.path().join("Upload");
    let general = upload_root.join("general");
    let sibling = dir.path().join("Upload-copy");
    for path in [&workspace, &general, &sibling] {
        fs::create_dir_all(path).unwrap();
    }
    let file = general.join("data.txt");
    let outside = sibling.join("outside.txt");
    fs::write(&file, b"good").unwrap();
    fs::write(&outside, b"outside").unwrap();

    for (input, root, valid) in [
        (file.as_path(), None, true),
        (outside.as_path(), None, true),
        (file.as_path(), Some(upload_root.as_path()), true),
        (outside.as_path(), Some(upload_root.as_path()), false),
        (workspace.as_path(), None, false),
        (general.as_path(), Some(upload_root.as_path()), false),
        (Path::new("relative.txt"), None, false),
        (Path::new("relative.txt"), Some(upload_root.as_path()), false),
    ] {
        assert_eq!(ChatAttachment::open(input, root).is_ok(), valid);
    }
    assert!(matches!(
        ChatAttachment::open(&outside, Some(&upload_root)),
        Err(AttachmentError::OutsideUploadRoot)
    ));
    assert_unavailable(ChatAttachment::open(&file.with_extension("missing"), None));
    assert!(ChatAttachment::open(&file, Some(&sibling)).is_err());

    let parent_segments = general.join("..").join("general").join("data.txt");
    let selected = ChatAttachment::open(&parent_segments, Some(&upload_root)).unwrap();
    assert_eq!(selected.path(), fs::canonicalize(&file).unwrap().to_str().unwrap());
    assert_eq!(selected.metadata().unwrap().len(), 4);
    let read = |mut file: &File| {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    assert_eq!(selected.with_file(read).unwrap(), b"good");
    assert_eq!(selected.clone().with_file(read).unwrap(), b"good");
}

#[test]
fn containment_uses_volume_case_rules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("MiXeD");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("data"), b"inside").unwrap();

    let alternate = dir.path().join("mixed");
    if alternate.exists() {
        let candidate = alternate.join("DATA");
        assert!(ChatAttachment::open(&candidate, Some(&root)).is_ok());
    } else {
        fs::create_dir(&alternate).unwrap();
        let candidate = alternate.join("data");
        fs::write(&candidate, b"outside").unwrap();
        assert!(matches!(
            ChatAttachment::open(&candidate, Some(&root)),
            Err(AttachmentError::OutsideUploadRoot)
        ));
    }
}

#[test]
fn replaced_files_fail_before_and_during_a_read() {
    for managed_upload in [false, true] {
        for swap_before_read in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let original = dir.path().join("attachment");
            let moved = dir.path().join("moved");
            let root = managed_upload.then_some(dir.path());
            fs::write(&original, b"authorized").unwrap();
            let attachment = ChatAttachment::open(&original, root).unwrap();
            let swap = || {
                fs::rename(&original, &moved).unwrap();
                fs::write(&original, b"substitute").unwrap();
            };

            if swap_before_read {
                swap();
                assert!(attachment.metadata().is_err());
            }
            let err = attachment
                .with_file(|mut file| {
                    assert!(
                        !swap_before_read,
                        "must reject the replacement before invoking the read"
                    );
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes)?;
                    assert_eq!(bytes, b"authorized");
                    swap();
                    Ok(bytes)
                })
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }
}

#[cfg(unix)]
#[test]
fn the_managed_root_cannot_be_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    fs::create_dir(&root).unwrap();
    let original = root.join("attachment");
    fs::write(&original, b"authorized").unwrap();
    let attachment = ChatAttachment::open(&original, Some(&root)).unwrap();
    let moved = dir.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    fs::create_dir(&root).unwrap();
    // Even the same file ID at the same name cannot authorize a replaced root.
    fs::hard_link(moved.join("attachment"), &original).unwrap();
    assert!(
        attachment
            .with_file::<()>(|_| panic!("must reject the replaced root"))
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn symlinks_follow_only_the_authorized_target() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    fs::create_dir(&root).unwrap();
    let inside = root.join("inside");
    let outside = dir.path().join("outside");
    fs::write(&inside, b"inside").unwrap();
    fs::write(&outside, b"outside").unwrap();
    let link = root.join("selected");
    symlink(&inside, &link).unwrap();
    let upload = ChatAttachment::open(&link, Some(&root)).unwrap();
    assert_eq!(Path::new(upload.path()), fs::canonicalize(&inside).unwrap());
    fs::remove_file(&link).unwrap();
    symlink(&outside, &link).unwrap();
    assert!(upload.metadata().is_err());
    assert!(matches!(
        ChatAttachment::open(&link, Some(&root)),
        Err(AttachmentError::OutsideUploadRoot)
    ));
    assert_eq!(
        Path::new(ChatAttachment::open(&link, None).unwrap().path()),
        fs::canonicalize(&outside).unwrap()
    );
    let redirected_root = dir.path().join("redirected");
    symlink(&root, &redirected_root).unwrap();
    assert_unavailable(ChatAttachment::open(&inside, Some(&redirected_root)));

    let unrepresentable = root.join("one\ntwo");
    fs::write(&unrepresentable, b"invalid marker path").unwrap();
    assert_unavailable(ChatAttachment::open(&unrepresentable, None));
}

#[cfg(unix)]
#[test]
fn special_files_are_refused_without_waiting_for_content() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("pipe");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path inside the test directory.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_unavailable(ChatAttachment::open(&fifo, None));
}

#[cfg(windows)]
#[test]
fn windows_path_forms() {
    for path in [
        r"\\host\share\file.txt",
        r"\\?\UNC\host\share\file.txt",
        r"\\?\unc\host\share\file.txt",
        r"\\.\C:\file.txt",
        r"\\.\pipe\name",
        r"\\?\GLOBALROOT\Device\HarddiskVolume1\file.txt",
        r"C:relative.txt",
        r"\drive-relative.txt",
        r"C:\folder\file.txt:stream",
        r"C:\folder\NUL.txt",
        r"C:\folder\COM¹.txt",
    ] {
        let path = Path::new(path);
        assert!(!platform::is_supported_path(path), "accepted {path:?}");
        assert_unavailable(ChatAttachment::open(path, None));
    }
    for path in [
        r"C:\folder\file.txt",
        r"Z:\mapped\file.txt",
        r"\\?\C:\folder\file.txt",
        r"C:\LONGFO~1\ATTACH~1.TXT",
    ] {
        assert!(platform::is_supported_path(Path::new(path)), "rejected {path}");
    }
}

#[cfg(windows)]
#[test]
fn windows_junctions_follow_only_the_authorized_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    let inside = root.join("inside");
    let outside = dir.path().join("outside");
    for target in [&inside, &outside] {
        fs::create_dir_all(target).unwrap();
        fs::write(target.join("file.txt"), b"fixture").unwrap();
    }
    let link = root.join("link");
    junction::create(&inside, &link).unwrap();
    let source = ChatAttachment::open(&link.join("file.txt"), Some(&root)).unwrap();
    junction::delete(&link).unwrap();
    junction::create(&outside, &link).unwrap();
    assert!(source.metadata().is_err());
    assert!(matches!(
        ChatAttachment::open(&link.join("file.txt"), Some(&root)),
        Err(AttachmentError::OutsideUploadRoot)
    ));
    assert!(ChatAttachment::open(&link.join("file.txt"), None).is_ok());
    junction::delete(&link).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_managed_root_cannot_be_replaced_through_a_parent_junction() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let substitute = dir.path().join("substitute");
    for parent in [&original, &substitute] {
        fs::create_dir_all(parent.join("managed")).unwrap();
    }
    fs::write(original.join("managed/file.txt"), b"authorized").unwrap();
    fs::hard_link(original.join("managed/file.txt"), substitute.join("managed/file.txt")).unwrap();

    // Move the alias, leaving the opened directory and file in place.
    let alias = dir.path().join("parent-junction");
    junction::create(&original, &alias).unwrap();
    let root = alias.join("managed");
    let attachment = ChatAttachment::open(&root.join("file.txt"), Some(&root)).unwrap();
    assert_unavailable(ChatAttachment::open(&root.join("file.txt"), Some(&alias)));
    junction::delete(&alias).unwrap();
    junction::create(&substitute, &alias).unwrap();

    // The path still names the same file ID, but the managed root changed.
    assert!(
        attachment
            .with_file::<()>(|_| panic!("must reject the replaced root"))
            .is_err()
    );
    junction::delete(&alias).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_verbatim_and_existing_short_aliases_reopen_the_same_file() {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Long Attachment Name.txt");
    fs::write(&path, b"fixture").unwrap();
    let long = ChatAttachment::open(&path, Some(dir.path())).unwrap();
    let verbatim = fs::canonicalize(&path).unwrap();
    assert_eq!(
        ChatAttachment::open(&verbatim, Some(dir.path())).unwrap().path(),
        long.path()
    );

    let name: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut short = [0; 32768];
    // SAFETY: name is NUL terminated and short has room for the given WCHAR count.
    let length = unsafe { GetShortPathNameW(name.as_ptr(), short.as_mut_ptr(), short.len() as u32) };
    assert!((1..short.len()).contains(&(length as usize)));
    // NTFS may return the long names when 8.3 alias generation is disabled.
    let alias = PathBuf::from(OsString::from_wide(&short[..length as usize]));
    assert_eq!(
        ChatAttachment::open(&alias, Some(dir.path())).unwrap().path(),
        long.path()
    );
}
