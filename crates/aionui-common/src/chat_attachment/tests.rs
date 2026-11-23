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
    ManagedUploadRoot::open(&upload_root).unwrap();
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
    ManagedUploadRoot::open(&root).unwrap();
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
            let managed = dir.path().join("managed");
            ManagedUploadRoot::open(&managed).unwrap();
            let original = managed.join("attachment");
            let moved = managed.join("moved");
            let root = managed_upload.then_some(managed.as_path());
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
    ManagedUploadRoot::open(&root).unwrap();
    let original = root.join("attachment");
    fs::write(&original, b"authorized").unwrap();
    let attachment = ChatAttachment::open(&original, Some(&root)).unwrap();
    let moved = dir.path().join("moved");
    fs::rename(&root, &moved).unwrap();
    ManagedUploadRoot::open(&root).unwrap();
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
fn cached_root_checks_the_candidates_ancestor_when_a_directory_name_is_reused() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let moved = dir.path().join("moved");
    let alias = dir.path().join("parent-alias");
    fs::create_dir(&original).unwrap();
    symlink(&original, &alias).unwrap();
    let input = alias.join("managed");
    let cache = OnceLock::new();
    let root = Some((input.as_path(), &cache));
    ManagedUploadRoot::open(&input).unwrap();
    fs::write(input.join("approved"), b"approved").unwrap();
    ChatAttachment::open_cached(&input.join("approved"), root).unwrap();

    fs::rename(&original, &moved).unwrap();
    fs::remove_file(&alias).unwrap();
    symlink(&moved, &alias).unwrap();
    fs::create_dir_all(original.join("managed")).unwrap();
    let outside = original.join("managed/unrelated");
    fs::write(&outside, b"separate directory").unwrap();

    assert!(ChatAttachment::open_cached(&input.join("approved"), root).is_ok());
    assert!(matches!(
        ChatAttachment::open_cached(&outside, root),
        Err(AttachmentError::OutsideUploadRoot)
    ));
}

#[cfg(unix)]
#[test]
fn symlinks_follow_only_the_authorized_target() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    ManagedUploadRoot::open(&root).unwrap();
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
    assert!(matches!(
        ChatAttachment::open(&inside, Some(&redirected_root)),
        Err(AttachmentError::UntrustedRoot)
    ));

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
fn remove_junction_fixture(link: &Path) {
    // junction::delete strips only the reparse data. The empty directory
    // entry must also be removed before creating a new junction at that name.
    junction::delete(link).unwrap();
    fs::remove_dir(link).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_junctions_follow_only_the_authorized_target() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    ManagedUploadRoot::open(&root).unwrap();
    let inside = root.join("inside");
    let outside = dir.path().join("outside");
    for target in [&inside, &outside] {
        fs::create_dir_all(target).unwrap();
        fs::write(target.join("file.txt"), b"fixture").unwrap();
    }
    let link = root.join("link");
    junction::create(&inside, &link).unwrap();
    let source = ChatAttachment::open(&link.join("file.txt"), Some(&root)).unwrap();
    remove_junction_fixture(&link);
    junction::create(&outside, &link).unwrap();
    assert!(source.metadata().is_err());
    assert!(matches!(
        ChatAttachment::open(&link.join("file.txt"), Some(&root)),
        Err(AttachmentError::OutsideUploadRoot)
    ));
    assert!(ChatAttachment::open(&link.join("file.txt"), None).is_ok());
    remove_junction_fixture(&link);
}

#[cfg(windows)]
#[test]
fn windows_managed_root_cannot_be_replaced_through_a_parent_junction() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original");
    let substitute = dir.path().join("substitute");
    for parent in [&original, &substitute] {
        fs::create_dir(parent).unwrap();
        ManagedUploadRoot::open(&parent.join("managed")).unwrap();
    }
    fs::write(original.join("managed/file.txt"), b"authorized").unwrap();
    fs::hard_link(original.join("managed/file.txt"), substitute.join("managed/file.txt")).unwrap();

    // Move the alias, leaving the opened directory and file in place.
    let alias = dir.path().join("parent-junction");
    junction::create(&original, &alias).unwrap();
    let root = alias.join("managed");
    let attachment = ChatAttachment::open(&root.join("file.txt"), Some(&root)).unwrap();
    assert!(matches!(
        ChatAttachment::open(&root.join("file.txt"), Some(&alias)),
        Err(AttachmentError::UntrustedRoot)
    ));
    remove_junction_fixture(&alias);
    junction::create(&substitute, &alias).unwrap();

    // The path still names the same file ID, but the managed root changed.
    assert!(
        attachment
            .with_file::<()>(|_| panic!("must reject the replaced root"))
            .is_err()
    );
    remove_junction_fixture(&alias);
}

#[cfg(windows)]
#[test]
fn windows_symlinks_check_the_substitute_before_any_target_open() {
    use std::os::windows::fs::symlink_file;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("managed");
    ManagedUploadRoot::open(&root).unwrap();
    let link = root.join("selected");
    let local = root.join("real file");
    fs::write(&local, b"selected").unwrap();
    symlink_file(&local, &link).unwrap();
    platform::walk::OPENS.with_borrow_mut(Vec::clear);
    let source = ChatAttachment::open(&link, Some(&root)).unwrap();
    platform::walk::OPENS.with_borrow(|opens| {
        let link_index = opens.iter().position(|name| name == "selected").unwrap();
        assert!(opens[link_index + 1..].iter().any(|name| name == "real file"));
    });
    assert_eq!(source.metadata().unwrap().len(), 8);
    fs::remove_file(&link).unwrap();
    symlink_file(r"\\localhost\h60-unopened-share\remote-leaf", &link).unwrap();

    for operation in 0..3 {
        platform::walk::OPENS.with_borrow_mut(Vec::clear);
        match operation {
            0 => assert_unavailable(ChatAttachment::open(&link, None)),
            1 => assert_unavailable(ChatAttachment::open(&link, Some(&root))),
            _ => assert!(source.metadata().is_err()),
        }
        platform::walk::OPENS.with_borrow(|opens| {
            assert_eq!(opens.last().unwrap(), "selected");
            assert!(
                opens
                    .iter()
                    .all(|name| !name.to_string_lossy().contains("h60-unopened-share"))
            );
            assert!(opens.iter().all(|name| name != "remote-leaf"));
        });
    }
    fs::remove_file(&link).unwrap();
    symlink_file(&link, &link).unwrap();
    assert_unavailable(ChatAttachment::open(&link, None));
}

#[cfg(windows)]
#[test]
fn windows_trailing_dots_and_spaces_have_explicit_wire_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let folder = fs::canonicalize(dir.path()).unwrap();
    let normal = std::path::PathBuf::from(folder.to_str().unwrap().strip_prefix(r"\\?\").unwrap());
    fs::write(folder.join("leaf"), b"plain").unwrap();
    fs::write(folder.join("leaf."), b"literal dot").unwrap();
    fs::write(folder.join("leaf "), b"literal space").unwrap();
    for (path, expected_length) in [
        (normal.join("leaf."), Some(5)),
        (folder.join("leaf."), Some(11)),
        (normal.join("leaf "), None),
        (folder.join("leaf "), None),
    ] {
        assert_eq!(
            ChatAttachment::open(&path, None).ok().map(|f| f.admitted_length()),
            expected_length
        );
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires the Windows CI SMB share and provisioned NTFS short name"]
fn windows_real_mapping_and_short_name() {
    let path =
        |name| PathBuf::from(std::env::var_os(name).expect("Windows CI must provision the share and real alias"));
    let mapped = path("HOST60_MAPPED_FILE");
    let long = path("HOST60_LONG_FILE");
    let short = path("HOST60_SHORT_FILE");
    assert_ne!(
        long.file_name(),
        short.file_name(),
        "8.3 evidence requires a distinct leaf alias"
    );
    let selected = ChatAttachment::open(&long, None).unwrap();
    assert_eq!(ChatAttachment::open(&short, None).unwrap().path(), selected.path());
    let link = path("HOST60_LINK_TO_MAPPED_FILE");
    std::os::windows::fs::symlink_file(&mapped, &link).unwrap();
    for input in [mapped.clone(), link] {
        let remote = ChatAttachment::open(&input, None).unwrap();
        assert_eq!(platform::drive(Path::new(remote.path())), platform::drive(&mapped));
        assert_eq!(
            remote
                .with_file(|mut f| {
                    let mut bytes = Vec::new();
                    f.read_to_end(&mut bytes)?;
                    Ok(bytes)
                })
                .unwrap(),
            b"mapped-file"
        );
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires a WOF-compressed file on the Windows runner"]
fn windows_non_surrogate_filter_reads_logical_bytes() {
    let path = PathBuf::from(std::env::var_os("HOST60_WOF_FILE").expect("Windows CI must provide a WOF file"));
    let selected = ChatAttachment::open(&path, None).unwrap();
    let bytes = selected
        .with_file(|mut file| {
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .unwrap();
    assert_eq!(bytes.len(), 65536);
    assert!(bytes.iter().all(|b| *b == b'W'));
}

#[cfg(unix)]
#[test]
fn managed_root_mode_and_owner_are_checked_on_adoption_and_recheck() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("managed");
    let root = ManagedUploadRoot::open(&path).unwrap();
    let stat = fs::metadata(&path).unwrap();
    assert_eq!(stat.uid(), unsafe { libc::geteuid() });
    assert_eq!(stat.mode() & 0o777, 0o700);
    let dest = root.destination("general".as_ref()).unwrap();
    let (_, stored) = dest.create_new("fixture".as_ref()).unwrap();
    for mode in [0o720, 0o702, 0o777] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        assert!(ManagedUploadRoot::open(&path).is_err());
        assert!(dest.create_new("forbidden".as_ref()).is_err());
        assert!(matches!(
            ChatAttachment::open(&stored, Some(&path)),
            Err(AttachmentError::UntrustedRoot)
        ));
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(root.verify_current().is_ok());
    // A foreign-owned directory can be tested without changing any ownership.
    if fs::metadata("/").unwrap().uid() != unsafe { libc::geteuid() } {
        assert!(ManagedUploadRoot::open(Path::new("/")).is_err());
    }
}

#[cfg(windows)]
#[test]
#[ignore = "requires Windows CI fixtures with explicit current/foreign owners and read/write ACEs"]
fn windows_checks_upload_root_owner_and_dacl() {
    let base = PathBuf::from(std::env::var_os("HOST60_ACL_FIXTURES").expect("missing Windows ACL fixtures"));
    for (name, trusted) in [
        ("created-by-host", true),
        ("readable", true),
        ("writable", false),
        ("foreign", false),
    ] {
        assert_eq!(ManagedUploadRoot::open(&base.join(name)).is_ok(), trusted, "{name}");
    }
}
