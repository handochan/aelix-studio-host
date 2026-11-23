//! Resolve DOS namespace links before opening any filesystem object. Open the
//! checked NT device name itself so rebinding the DOS name cannot race the open.
use super::*;
use std::path::Prefix;
use windows_sys::Wdk::System::SystemServices::{FILE_FS_DEVICE_INFORMATION, FILE_REMOTE_DEVICE};
use windows_sys::Win32::System::Ioctl::{
    FILE_DEVICE_CD_ROM_FILE_SYSTEM, FILE_DEVICE_DISK_FILE_SYSTEM, FILE_DEVICE_VIRTUAL_DISK,
};

const MAX_SUBST: usize = 8;

fn denied() -> io::Error {
    io::ErrorKind::InvalidInput.into()
}

fn mapping(name: &OsStr) -> io::Result<String> {
    let wide: Vec<_> = name.encode_wide().chain([0]).collect();
    let mut buf = vec![0; 1024];
    loop {
        // QueryDosDevice returns a MULTI_SZ. Only its first string is the current mapping.
        let n = unsafe { QueryDosDeviceW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n > 0 {
            let end = buf[..n].iter().position(|v| *v == 0).ok_or_else(denied)?;
            return String::from_utf16(&buf[..end]).map_err(|_| denied());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || buf.len() >= 65536 {
            return Err(err);
        }
        buf.resize(buf.len() * 2, 0);
    }
}

fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())?
        .eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

fn numbered(s: &str, prefix: &str) -> bool {
    strip_ci(s, prefix).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn local_device(name: &str) -> bool {
    let Some(s) = strip_ci(name, r"\Device\") else {
        return false;
    };
    [
        "HarddiskVolume",
        "HarddiskVolumeShadowCopy",
        "CdRom",
        "Floppy",
        "Ramdisk",
    ]
    .iter()
    .any(|prefix| numbered(s, prefix))
        || s.split_once('\\')
            .is_some_and(|(disk, part)| numbered(disk, "Harddisk") && numbered(part, "Partition"))
}

pub(in crate::chat_attachment::platform) fn volume_name(path: &Path) -> Option<&str> {
    let Component::Prefix(prefix) = path.components().next()? else {
        return None;
    };
    let Prefix::Verbatim(name) = prefix.kind() else {
        return None;
    };
    let name = name.to_str()?;
    let uuid = strip_ci(name, "Volume{")?.strip_suffix('}')?;
    (uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then_some(name)
}

enum Mapping<'a> {
    Local(&'a str),
    Network(String),
    Subst(PathBuf),
}

fn classify(target: &str) -> io::Result<Mapping<'_>> {
    if local_device(target) {
        return Ok(Mapping::Local(target));
    }
    if let Some(dos) = target.strip_prefix(r"\??\") {
        // QueryDosDevice supplies an NT substitute. Preserve its component
        // spelling, including trailing dots; do not apply DOS trimming to it.
        if matches!(Path::new(dos).components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_)))
            && Path::new(dos).is_absolute()
        {
            let path = PathBuf::from(format!(r"\\?\{dos}"));
            if is_supported_path(&path) && path.components().skip(2).all(|c| matches!(c, Component::Normal(_))) {
                return Ok(Mapping::Subst(path));
            }
        }
        return Err(denied());
    }
    let tail = strip_ci(target, r"\Device\LanmanRedirector\")
        .or_else(|| strip_ci(target, r"\Device\Mup\"))
        .ok_or_else(denied)?;
    // The redirector can include its provider name and a per-logon drive ID.
    let mut share = strip_ci(tail, r";LanmanRedirector\").unwrap_or(tail);
    if share.starts_with(';') {
        let (id, rest) = share.split_once('\\').ok_or_else(denied)?;
        if id.len() < 4
            || !id.as_bytes()[1].is_ascii_alphabetic()
            || id.as_bytes()[2] != b':'
            || !id[3..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(denied());
        }
        share = rest;
    }
    let parts = share.trim_end_matches('\\');
    if parts.split('\\').count() < 2
        || parts
            .split('\\')
            .any(|p| p.is_empty() || matches!(p, "." | "..") || p.contains([':', '/', '\0']))
    {
        return Err(denied());
    }
    // LanmanRedirector is the targeted MUP provider. Keep the mapping's
    // original per-logon connection qualifier when addressing MUP directly.
    let native = if strip_ci(target, r"\Device\LanmanRedirector\").is_some() {
        format!(r"\Device\Mup\;LanmanRedirector\{tail}")
    } else {
        target.to_owned()
    };
    Ok(Mapping::Network(native))
}

fn device_root(native: &str, local: bool) -> io::Result<File> {
    let name = OsString::from(format!("{}\\", native.trim_end_matches('\\')));
    let file = nt_open(None, &name, DIR_ACCESS, FILE_OPEN, PROBE, ptr::null())?;
    if local {
        let mut info = FILE_FS_DEVICE_INFORMATION::default();
        let mut iosb = IO_STATUS_BLOCK::default();
        // SAFETY: query fixed-size device information on the pinned root.
        let status = unsafe {
            NtQueryVolumeInformationFile(
                file.as_raw_handle(),
                &mut iosb,
                (&raw mut info).cast(),
                size_of_val(&info) as u32,
                FileFsDeviceInformation,
            )
        };
        if status < 0 {
            return Err(io::Error::from_raw_os_error(
                unsafe { RtlNtStatusToDosError(status) } as i32
            ));
        }
        if !is_local_volume(info.DeviceType, info.Characteristics) {
            return Err(denied());
        }
    }
    if !file.metadata()?.is_dir() || is_surrogate(&file)? {
        return Err(denied());
    }
    Ok(file)
}

fn is_local_volume(kind: u32, characteristics: u32) -> bool {
    matches!(
        kind,
        FILE_DEVICE_DISK
            | FILE_DEVICE_CD_ROM
            | FILE_DEVICE_VIRTUAL_DISK
            | FILE_DEVICE_DISK_FILE_SYSTEM
            | FILE_DEVICE_CD_ROM_FILE_SYSTEM
    ) && characteristics & FILE_REMOTE_DEVICE == 0
}

pub(super) struct Start {
    pub(super) root: File,
    pub(super) prefix: PathBuf,
    pub(super) names: Vec<OsString>,
}

fn enter_subst(visited: &mut Vec<char>, letter: char) -> io::Result<()> {
    if visited.contains(&letter) || visited.len() >= MAX_SUBST {
        return Err(denied());
    }
    visited.push(letter);
    Ok(())
}

pub(super) fn resolve(input: &Path, mounted: bool) -> io::Result<Start> {
    let mut path = input.to_owned();
    let mut visited = Vec::new();
    loop {
        // Only a decoded mount-point substitute may introduce a volume GUID.
        let guid = volume_name(&path).filter(|_| mounted);
        if guid.is_none() && !is_supported_path(&path) {
            return Err(denied());
        }
        let abs = std::path::absolute(&path)?;
        let mut parts = abs.components();
        let Some(Component::Prefix(prefix)) = parts.next() else {
            return Err(denied());
        };
        if parts.next() != Some(Component::RootDir) {
            return Err(denied());
        }
        let names = parts
            .map(|part| match part {
                Component::Normal(name) => {
                    crate::chat_attachment::check_child_name(name)?;
                    Ok(name.to_owned())
                }
                _ => Err(denied()),
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut root_name = prefix.as_os_str().to_owned();
        root_name.push(r"\");
        let prefix = PathBuf::from(root_name);
        if let Some(volume) = guid {
            let native = mapping(volume.as_ref())?;
            if !local_device(&native) {
                return Err(denied());
            }
            return Ok(Start {
                root: device_root(&native, true)?,
                prefix,
                names,
            });
        }
        let letter = drive(&abs).ok_or_else(denied)?.to_ascii_uppercase();
        if !letter.is_ascii_alphabetic() {
            return Err(denied());
        }
        if visited.contains(&letter) {
            return Err(denied());
        }
        let native = mapping(format!("{letter}:").as_ref())?;
        match classify(&native)? {
            Mapping::Subst(mut underlying) => {
                enter_subst(&mut visited, letter)?;
                for name in names {
                    underlying.push(name);
                }
                path = underlying;
            }
            Mapping::Local(name) => {
                return Ok(Start {
                    root: device_root(name, true)?,
                    prefix,
                    names,
                });
            }
            Mapping::Network(share) => {
                return Ok(Start {
                    root: device_root(&share, false)?,
                    prefix,
                    names,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn subst_depth_and_cycles_are_bounded() {
        let mut seen = Vec::new();
        enter_subst(&mut seen, 'A').unwrap();
        assert!(enter_subst(&mut seen, 'A').is_err());
        for letter in 'B'..='H' {
            enter_subst(&mut seen, letter).unwrap();
        }
        assert!(enter_subst(&mut seen, 'I').is_err());
    }

    #[test]
    fn only_mount_substitutes_can_introduce_an_exact_volume_guid() {
        let parent = Path::new(r"C:\folder");
        let guid = r"\??\Volume{12345678-1234-1234-1234-123456789abc}\";
        let internal = link_target(guid, false, parent, true).unwrap();
        assert!(volume_name(&internal).is_some());
        assert!(!is_supported_path(&internal));
        assert!(link_target(guid, false, parent, false).is_err());
        assert!(link_target(r"\??\Volume{1234}\", false, parent, true).is_err());
        assert!(!local_device(r"\Device\Mup\server\share"));
    }

    #[test]
    fn allowed_dos_mappings_and_local_volume_types() {
        assert!(matches!(classify(r"\Device\HarddiskVolume12"), Ok(Mapping::Local(_))));
        assert!(matches!(
            classify(r"\Device\Harddisk0\Partition1"),
            Ok(Mapping::Local(_))
        ));
        assert!(matches!(
            classify(r"\Device\LanmanRedirector\;Z:000009ab\localhost\share"),
            Ok(Mapping::Network(_))
        ));
        assert!(matches!(classify(r"\Device\Mup\server\share"), Ok(Mapping::Network(_))));
        assert!(matches!(classify(r"\??\C:\sub"), Ok(Mapping::Subst(_))));
        for bad in [
            r"\Device\HarddiskVolume1\hidden",
            r"\Device\NamedPipe",
            r"\??\UNC\server\share",
            r"\Device\Mup\;WebDavRedirector\host\share",
        ] {
            assert!(classify(bad).is_err());
        }
        assert!(is_local_volume(FILE_DEVICE_DISK, 0));
        assert!(!is_local_volume(FILE_DEVICE_DISK, FILE_REMOTE_DEVICE));
        assert!(!is_local_volume(0x14, 0)); // FILE_DEVICE_NETWORK_FILE_SYSTEM
    }
}
