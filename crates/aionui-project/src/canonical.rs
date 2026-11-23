use std::cell::Cell;
use std::path::{Component, Path, PathBuf};

use percent_encoding::{AsciiSet, CONTROLS, percent_encode};
use url::{SyntaxViolation, Url};

use crate::types::ProjectError;

/// Whether the `file:` provider folds path casing on this platform.
/// macOS / Windows are case-insensitive; Linux is case-sensitive. Making this
/// a compile-time constant keeps `resource_canonical` machine-platform-relative
/// as the design specifies (local single-DB desktop, so no cross-platform key).
pub const IGNORE_PATH_CASING: bool = cfg!(any(target_os = "macos", target_os = "windows"));

/// Provider scheme. Only `file:` is registered for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    File,
}

/// A lexically-normalized canonical resource URI — the folder dedupe key.
/// Produced only by [`canonicalize`]; never constructed from a raw string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical(String);

impl Canonical {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parse the scheme of a provider URI (for provider dispatch; not persisted).
pub fn parse_scheme(uri: &str) -> Result<Scheme, ProjectError> {
    file_scheme(&Url::parse(uri).map_err(|_| invalid_uri(uri))?)
}

fn file_scheme(url: &Url) -> Result<Scheme, ProjectError> {
    match url.scheme() {
        "file" => Ok(Scheme::File),
        other => Err(ProjectError::UnsupportedResourceScheme {
            scheme: other.to_owned(),
        }),
    }
}

/// Lexical canonicalization of a `file:` URI into the dedupe-key identity.
/// Pure: never touches the filesystem, never resolves symlinks.
///
/// Steps (see `data-model.md` "resource_canonical semantics"): parse to a fs
/// path, lexically resolve `.`/`..` and collapse separators, drop trailing
/// slash, fold path casing per platform, rebuild the `file:` URI (percent-
/// encoded, no fragment/query).
pub fn canonicalize(uri: &str) -> Result<Canonical, ProjectError> {
    let url = parse_resource_url(uri)?;
    file_scheme(&url)?;
    let path = url.to_file_path().map_err(|_| invalid_uri(uri))?;

    let normalized = normalize_lexical(&path);
    let folded = if IGNORE_PATH_CASING {
        PathBuf::from(normalized.to_string_lossy().to_ascii_lowercase())
    } else {
        normalized
    };

    file_uri(&folded).map(Canonical).ok_or_else(|| invalid_uri(uri))
}

/// Display-name derivation: the final path segment of a canonical folder.
pub fn basename(canonical: &Canonical) -> String {
    fs_path(canonical)
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default()
}

/// Build a `file:` URI from an absolute filesystem path (raw input capture).
/// Does not canonicalize — callers pass the result to [`canonicalize`] for the
/// identity form.
pub fn to_file_uri(path: &Path) -> Result<String, ProjectError> {
    file_uri(path).ok_or_else(|| invalid_uri(&path.to_string_lossy()))
}

/// Derive the absolute filesystem path from a `file:` canonical URI.
pub fn fs_path(canonical: &Canonical) -> Result<PathBuf, ProjectError> {
    uri_to_path(canonical.as_str())
}

/// Derive the absolute filesystem path from a raw `file:` URI string. Used by
/// the runtime provider, which receives already-resolved URIs as `&str` (a
/// [`Canonical`] cannot be constructed outside [`canonicalize`]).
pub fn uri_to_path(uri: &str) -> Result<PathBuf, ProjectError> {
    parse_resource_url(uri)?.to_file_path().map_err(|_| invalid_uri(uri))
}

fn invalid_uri(uri: &str) -> ProjectError {
    ProjectError::FolderCanonicalizeFailed { uri: uri.to_owned() }
}

fn parse_resource_url(uri: &str) -> Result<Url, ProjectError> {
    let drive_with_host = Cell::new(false);
    let url = Url::options()
        .syntax_violation_callback(Some(&|violation| {
            if violation == SyntaxViolation::FileWithHostAndWindowsDrive {
                drive_with_host.set(true);
            }
        }))
        .parse(uri)
        .map_err(|_| invalid_uri(uri))?;
    if drive_with_host.get() {
        // url 2.5.8 dropped the host when it recognized a drive letter during
        // parsing. Rebuild from the parsed path so normal UNC shares keep their
        // host, including paths that only resemble a drive after normalization.
        return Url::parse(&format!("file://{}", url.path())).map_err(|_| invalid_uri(uri));
    }
    Ok(url)
}

// Keep the url 2.5.8 file-path encoding for persisted resource_canonical keys
// and for file URIs that feed stored resource_uri values and URI comparisons.
const FILE_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

fn file_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let mut uri = String::from("file://");
    push_file_path(&mut uri, path)?;
    Some(uri)
}

#[cfg(not(windows))]
fn push_file_path(uri: &mut String, path: &Path) -> Option<()> {
    const UNIX_FILE_SEGMENT: &AsciiSet = &FILE_SEGMENT.add(b'\\');
    let mut segments = path.components().skip(1).peekable();
    if segments.peek().is_none() {
        uri.push('/');
    }
    for segment in segments {
        uri.push('/');
        uri.extend(percent_encode(
            segment.as_os_str().as_encoded_bytes(),
            UNIX_FILE_SEGMENT,
        ));
    }
    Some(())
}

#[cfg(windows)]
fn push_file_path(uri: &mut String, path: &Path) -> Option<()> {
    use std::path::Prefix;

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let disk = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            uri.push('/');
            uri.push(char::from(letter));
            uri.push(':');
            true
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            let host = url::Host::parse(server.to_str()?).ok()?;
            uri.push_str(&host.to_string());
            uri.push('/');
            uri.extend(percent_encode(share.to_str()?.as_bytes(), FILE_SEGMENT));
            false
        }
        _ => return None,
    };
    let mut segments = components
        .filter(|component| *component != Component::RootDir)
        .peekable();
    if disk && segments.peek().is_none() {
        uri.push('/');
    }
    for segment in segments {
        uri.push('/');
        uri.extend(percent_encode(segment.as_os_str().to_str()?.as_bytes(), FILE_SEGMENT));
    }
    Some(())
}

/// Lexically resolve `.` / `..` and collapse redundant separators without
/// touching the filesystem. `..` above the root is clamped (not an error here —
/// containment rejects escapes at the reference layer).
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                }
                // Otherwise ignore: cannot ascend above root / prefix.
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

#[cfg(test)]
#[path = "canonical_test.rs"]
mod canonical_test;
