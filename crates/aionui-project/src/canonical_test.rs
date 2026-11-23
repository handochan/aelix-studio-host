use super::*;

#[test]
fn drops_trailing_slash() {
    assert_eq!(
        canonicalize("file:///a/b/").unwrap(),
        canonicalize("file:///a/b").unwrap()
    );
}

#[test]
fn resolves_dot_dot_lexically() {
    assert_eq!(
        canonicalize("file:///a/b/../c").unwrap(),
        canonicalize("file:///a/c").unwrap()
    );
}

#[test]
fn resolves_single_dot() {
    assert_eq!(
        canonicalize("file:///a/./b").unwrap(),
        canonicalize("file:///a/b").unwrap()
    );
}

#[test]
fn collapses_repeated_separators() {
    assert_eq!(
        canonicalize("file:///a//b").unwrap(),
        canonicalize("file:///a/b").unwrap()
    );
}

#[test]
fn dot_dot_above_root_is_clamped_not_errored() {
    // Lexical clamp to root; containment (not canonicalize) rejects escapes.
    assert_eq!(
        canonicalize("file:///../../a").unwrap(),
        canonicalize("file:///a").unwrap()
    );
}

#[test]
fn is_deterministic() {
    let a = canonicalize("file:///Users/me/proj").unwrap();
    let b = canonicalize("file:///Users/me/proj").unwrap();
    assert_eq!(a, b);
}

#[test]
fn casing_folds_per_platform() {
    let mixed = canonicalize("file:///Users/Me/Aion").unwrap();
    let lower = canonicalize("file:///users/me/aion").unwrap();
    if IGNORE_PATH_CASING {
        // macOS / Windows: same folder.
        assert_eq!(mixed, lower);
        assert_eq!(mixed.as_str(), "file:///users/me/aion");
    } else {
        // Linux: two distinct folders.
        assert_ne!(mixed, lower);
    }
}

#[test]
fn symlink_dir_is_not_its_target_lexically() {
    // Pure lexical identity: two distinct path strings are two distinct
    // folders regardless of any on-disk symlink relationship.
    let link = canonicalize("file:///a/link").unwrap();
    let target = canonicalize("file:///a/target").unwrap();
    assert_ne!(link, target);
}

#[test]
fn unsupported_scheme_is_rejected() {
    let err = canonicalize("ssh://host/home/me/project").unwrap_err();
    assert_eq!(err.code(), "unsupported_resource_scheme");
}

#[test]
fn parse_scheme_accepts_file_rejects_others() {
    assert_eq!(parse_scheme("file:///a").unwrap(), Scheme::File);
    assert_eq!(
        parse_scheme("ssh://h/p").unwrap_err().code(),
        "unsupported_resource_scheme"
    );
}

#[test]
fn basename_is_final_segment() {
    let c = canonicalize("file:///Users/me/aion").unwrap();
    assert_eq!(basename(&c), "aion");
}

#[test]
fn fs_path_roundtrips_canonical() {
    let c = canonicalize("file:///Users/me/aion").unwrap();
    let p = fs_path(&c).unwrap();
    // Re-deriving the file uri from the path reproduces the canonical string.
    assert_eq!(to_file_uri(&p).unwrap(), c.as_str());
}

#[test]
fn to_file_uri_does_not_fold_casing() {
    // to_file_uri is raw capture, not identity: casing is preserved.
    let uri = to_file_uri(std::path::Path::new("/Users/Me/Aion")).unwrap();
    assert_eq!(uri, "file:///Users/Me/Aion");
}

// Expected values were captured from the original derivation with url 2.5.8
// and form_urlencoded 1.2.2. Each row checks both the raw URI and stored key.
#[test]
fn legacy_file_uri_encoding() {
    #[cfg(unix)]
    let (path_root, raw_root) = ("/Repo", "file:///Repo");
    #[cfg(windows)]
    let (path_root, raw_root) = (r"C:\Repo", "file:///C:/Repo");
    #[cfg(target_os = "macos")]
    let key_root = "file:///repo";
    #[cfg(all(unix, not(target_os = "macos")))]
    let key_root = "file:///Repo";
    #[cfg(windows)]
    let key_root = "file:///C:/repo";

    let vectors = [
        ("^", "^", None),
        ("has space", "has%20space", None),
        ("%", "%25", None),
        ("%5E", "%255E", Some("%255e")),
        ("#", "%23", None),
        ("?", "%3F", None),
        ("[", "[", None),
        ("]", "]", None),
        ("{", "%7B", None),
        ("}", "%7D", None),
        ("|", "|", None),
        ("`", "%60", None),
        ("é", "%C3%A9", None),
        ("e\u{301}", "e%CC%81", None),
        ("雪", "%E9%9B%AA", None),
        ("🚀", "%F0%9F%9A%80", None),
        (
            "Mix^ %{#?[]}|}`雪🚀",
            "Mix^%20%25%7B%23%3F[]%7D|%7D%60%E9%9B%AA%F0%9F%9A%80",
            Some("mix^%20%25%7B%23%3F[]%7D|%7D%60%E9%9B%AA%F0%9F%9A%80"),
        ),
        ("quote\"<>", "quote%22%3C%3E", None),
        ("line\nbreak", "line%0Abreak", None),
        ("tab\tchar", "tab%09char", None),
    ];

    for (leaf, raw_tail, folded_tail) in vectors {
        let path = Path::new(path_root).join(leaf);
        let uri = to_file_uri(&path).unwrap();
        assert_eq!(uri, format!("{raw_root}/{raw_tail}"), "{path:?}");
        let key_tail = if IGNORE_PATH_CASING {
            folded_tail.unwrap_or(raw_tail)
        } else {
            raw_tail
        };
        assert_eq!(
            canonicalize(&uri).unwrap().as_str(),
            format!("{key_root}/{key_tail}"),
            "{uri}"
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_file_uri_unix_paths() {
    let vectors = [
        ("/", "file:///", "file:///"),
        (
            r"/Repo/contains\backslash",
            "file:///Repo/contains%5Cbackslash",
            if IGNORE_PATH_CASING {
                "file:///repo/contains%5Cbackslash"
            } else {
                "file:///Repo/contains%5Cbackslash"
            },
        ),
    ];
    for (path, raw, key) in vectors {
        assert_file_uri_vector(path, raw, key);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_file_uri_non_utf8_path() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let path = Path::new(OsStr::from_bytes(b"/Repo/Byte\xff^"));
    let uri = to_file_uri(path).unwrap();
    assert_eq!(uri, "file:///Repo/Byte%FF^");
    assert_eq!(canonicalize(&uri).unwrap().as_str(), "file:///Repo/Byte%FF^");
}

#[cfg(windows)]
#[test]
fn legacy_file_uri_windows_paths() {
    let vectors = [
        (r"C:\", "file:///C:/", "file:///C:/"),
        (r"\\?\C:\Repo^Mixed", "file:///C:/Repo^Mixed", "file:///C:/repo^mixed"),
        (
            r"\\Server\Share^Name\Repo^Mixed",
            "file://server/Share^Name/Repo^Mixed",
            "file://server/share^name/repo^mixed",
        ),
        (
            r"\\?\UNC\Server\Share^Name\Repo^Mixed",
            "file://server/Share^Name/Repo^Mixed",
            "file://server/share^name/repo^mixed",
        ),
        (
            r"\\SERVER\Finance$\é 🚀",
            "file://server/Finance$/%C3%A9%20%F0%9F%9A%80",
            "file://server/finance$/%C3%A9%20%F0%9F%9A%80",
        ),
    ];
    for (path, raw, key) in vectors {
        assert_file_uri_vector(path, raw, key);
    }
}

#[test]
fn legacy_file_uri_drive_authority() {
    #[cfg(target_os = "macos")]
    let (drive, path) = ("file:///c:/repo^mixed", "/C:/Repo^Mixed");
    #[cfg(all(unix, not(target_os = "macos")))]
    let (drive, path) = ("file:///C:/Repo^Mixed", "/C:/Repo^Mixed");
    #[cfg(windows)]
    let (drive, path) = ("file:///C:/repo^mixed", r"C:\Repo^Mixed");

    for input in [
        "file://server/C:/Repo%5EMixed",
        "file://server/C|/Repo^Mixed",
        "file://server/./C:/Repo^Mixed",
        "file://server/dir/../C:/Repo^Mixed",
        "file://localhost/C:/Repo^Mixed",
    ] {
        assert_eq!(canonicalize(input).unwrap().as_str(), drive, "{input}");
        assert_eq!(uri_to_path(input).unwrap(), Path::new(path), "{input}");
    }

    // A drive that only looks valid after collapsing a leading empty segment
    // or decoding a percent escape was treated as a share by url 2.5.8.
    let shares = [
        (
            "file://server//C:/Repo^Mixed",
            r"\\server\C:\Repo^Mixed",
            "file://server/c:/repo^mixed",
        ),
        (
            "file://server/C%3A/Repo^Mixed",
            r"\\server\C:\Repo^Mixed",
            "file://server/c:/repo^mixed",
        ),
        (
            "file://server/Share/Repo^Mixed",
            r"\\server\Share\Repo^Mixed",
            "file://server/share/repo^mixed",
        ),
    ];
    for (input, path, key) in shares {
        if cfg!(windows) {
            assert_eq!(uri_to_path(input).unwrap(), Path::new(path), "{input}");
            assert_eq!(canonicalize(input).unwrap().as_str(), key, "{input}");
        } else {
            assert_eq!(
                uri_to_path(input).unwrap_err().code(),
                "folder_canonicalize_failed",
                "{input}"
            );
            assert_eq!(
                canonicalize(input).unwrap_err().code(),
                "folder_canonicalize_failed",
                "{input}"
            );
        }
    }
}

fn assert_file_uri_vector(path: &str, raw: &str, key: &str) {
    assert_eq!(to_file_uri(Path::new(path)).unwrap(), raw, "{path}");
    assert_eq!(canonicalize(raw).unwrap().as_str(), key, "{path}");
}
