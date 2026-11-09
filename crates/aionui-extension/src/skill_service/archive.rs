mod layout;

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::error::ExtensionError;

use super::{MAX_SKILL_IMPORT_FILE_BYTES, MAX_SKILL_IMPORT_TOTAL_BYTES};

// Issue #21, accepted 2026-11-09: the 20 bundled skill packages contain at most
// 111 files / 1,269,751 bytes; ZIP_DEFLATED level 9 measured at most 9.600:1
// per entry (3.074:1 per package). These limits allow substantial headroom.
// Count implicit output directories too, so deep names cannot bypass the inode
// budget. ZIP-wide bytes intentionally include all skills and ignored files.
const MAX_ZIP_ENTRIES: usize = 4096;
const MAX_ZIP_COMPRESSION_RATIO: u64 = 200;

pub(super) struct OversizedZipFile {
    pub relative_path: PathBuf,
    pub file_bytes: u64,
}

pub(super) fn extract_zip_archive(
    archive_path: &Path,
    destination: &Path,
) -> Result<Vec<OversizedZipFile>, ExtensionError> {
    let file = std::fs::File::open(archive_path)?;
    let raw_entry_reader = file.try_clone()?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
    enforce_entry_count(archive.len())?;
    layout::validate(&mut archive, raw_entry_reader)?;
    let mut output_paths = HashSet::new();
    let mut total_bytes = 0_u64;
    let mut oversized = Vec::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(zip_error)?;
        let entry_name = entry.name().map_err(zip_error)?.into_owned();
        reject_zip_symlink(&entry)?;
        let relative_path = safe_zip_entry_path(&entry_name)?;
        for path in relative_path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            output_paths.insert(path.to_path_buf());
            enforce_entry_count(output_paths.len())?;
        }
        let output_path = destination.join(&relative_path);

        if entry.is_dir() {
            std::fs::create_dir_all(&output_path)?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&output_path)?;
        let compressed_bytes = entry.compressed_size();
        match copy_entry(&mut entry, &mut output, &entry_name, compressed_bytes, &mut total_bytes) {
            Ok(()) => {}
            Err(ExtensionError::SkillImportFileTooLarge { file_bytes, .. }) => {
                drop(output);
                std::fs::remove_file(&output_path)?;
                oversized.push(OversizedZipFile {
                    relative_path,
                    file_bytes,
                });
            }
            Err(error) => return Err(error),
        }
    }

    Ok(oversized)
}

fn enforce_entry_count(entry_count: usize) -> Result<(), ExtensionError> {
    if entry_count > MAX_ZIP_ENTRIES {
        return Err(ExtensionError::SkillImportZipTooManyEntries {
            entry_count,
            limit_entries: MAX_ZIP_ENTRIES,
        });
    }
    Ok(())
}

fn copy_entry(
    input: &mut impl Read,
    output: &mut impl Write,
    name: &str,
    compressed_bytes: u64,
    total_bytes: &mut u64,
) -> Result<(), ExtensionError> {
    let ratio_limit_bytes = compressed_bytes.saturating_mul(MAX_ZIP_COMPRESSION_RATIO);
    let mut file_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        // Read at most one byte past the tightest budget, then reject BEFORE
        // writing it. Never use the declared uncompressed size as a budget.
        let remaining = (MAX_SKILL_IMPORT_FILE_BYTES - file_bytes)
            .min(MAX_SKILL_IMPORT_TOTAL_BYTES - *total_bytes)
            .min(ratio_limit_bytes - file_bytes);
        let read_len = remaining.saturating_add(1).min(buffer.len() as u64) as usize;
        let count = match input.read(&mut buffer[..read_len]) {
            Ok(0) => return Ok(()),
            Ok(count) => count,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(invalid_zip_io(err)),
        };
        let next_file_bytes = file_bytes + count as u64;
        let next_total_bytes = *total_bytes + count as u64;
        // Charge decoded bytes, including the 50 MiB + 1 probe, permanently.
        // Deleting a partial file never refunds budget: repeated skipped files
        // cannot buy more expansion work or hide earlier disk writes. This is
        // conservatively at least all bytes written, not header size accounting.
        *total_bytes = next_total_bytes;
        if next_total_bytes > MAX_SKILL_IMPORT_TOTAL_BYTES {
            return Err(ExtensionError::SkillImportZipTooLarge {
                total_bytes: next_total_bytes,
                limit_bytes: MAX_SKILL_IMPORT_TOTAL_BYTES,
            });
        }
        if next_file_bytes > ratio_limit_bytes {
            return Err(ExtensionError::SkillImportZipCompressionRatio {
                file_path: name.to_owned(),
                expanded_bytes: next_file_bytes,
                compressed_bytes,
                limit_ratio: MAX_ZIP_COMPRESSION_RATIO,
            });
        }
        if next_file_bytes > MAX_SKILL_IMPORT_FILE_BYTES {
            return Err(ExtensionError::SkillImportFileTooLarge {
                file_path: Some(name.to_owned()),
                file_bytes: next_file_bytes,
                limit_bytes: MAX_SKILL_IMPORT_FILE_BYTES,
            });
        }
        output.write_all(&buffer[..count])?;
        file_bytes = next_file_bytes;
    }
}

fn safe_zip_entry_path(name: &str) -> Result<PathBuf, ExtensionError> {
    if name.is_empty() || name.contains('\\') {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let path = Path::new(name);
    if path.is_absolute() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    let mut safe_path = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => safe_path.push(part),
            Component::CurDir => {}
            _ => return Err(ExtensionError::PathTraversal(name.to_string())),
        }
    }

    if safe_path.as_os_str().is_empty() {
        return Err(ExtensionError::PathTraversal(name.to_string()));
    }

    Ok(safe_path)
}

fn reject_zip_symlink<R: io::Read>(entry: &zip::read::ZipFile<'_, R>) -> Result<(), ExtensionError> {
    if let Some(mode) = entry.unix_mode()
        && mode & 0o170000 == 0o120000
    {
        return Err(ExtensionError::SkillImportSymlinkEntry(
            entry.name().map_err(zip_error)?.into_owned(),
        ));
    }
    Ok(())
}

fn zip_error(err: zip::result::ZipError) -> ExtensionError {
    ExtensionError::SkillImportInvalidZip(err.to_string())
}

fn invalid_zip_io(err: io::Error) -> ExtensionError {
    ExtensionError::SkillImportInvalidZip(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct CountingWriter(u64);

    impl Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn file_limit_never_writes_the_overflow_byte() {
        let mut input = io::repeat(0).take(MAX_SKILL_IMPORT_FILE_BYTES + 1);
        let mut output = CountingWriter::default();
        let mut total = 0;
        let error = copy_entry(&mut input, &mut output, "data", MAX_SKILL_IMPORT_FILE_BYTES, &mut total).unwrap_err();
        assert!(
            matches!(error, ExtensionError::SkillImportFileTooLarge { file_bytes, .. }
            if file_bytes == MAX_SKILL_IMPORT_FILE_BYTES + 1)
        );
        assert_eq!(output.0, MAX_SKILL_IMPORT_FILE_BYTES);
        assert_eq!(total, MAX_SKILL_IMPORT_FILE_BYTES + 1);
        assert_eq!(input.limit(), 0);
    }

    #[test]
    fn zip_total_limit_never_writes_the_overflow_chunk() {
        let mut input = io::repeat(0).take(100);
        let mut output = CountingWriter::default();
        let initial = MAX_SKILL_IMPORT_TOTAL_BYTES - 8;
        let mut total = initial;
        let error = copy_entry(&mut input, &mut output, "data", 100, &mut total).unwrap_err();
        assert!(
            matches!(error, ExtensionError::SkillImportZipTooLarge { total_bytes, .. }
            if total_bytes == MAX_SKILL_IMPORT_TOTAL_BYTES + 1)
        );
        assert_eq!(output.0, 0);
        assert_eq!(total, MAX_SKILL_IMPORT_TOTAL_BYTES + 1);
        assert_eq!(input.limit(), 91);
    }

    #[test]
    fn compression_ratio_boundary_and_overflow() {
        for (size, succeeds) in [(200, true), (201, false)] {
            let mut input = io::repeat(0).take(size);
            let mut output = CountingWriter::default();
            let mut total = 0;
            let result = copy_entry(&mut input, &mut output, "data", 1, &mut total);
            if succeeds {
                result.unwrap();
                assert_eq!(output.0, 200);
                assert_eq!(total, 200);
            } else {
                assert!(matches!(
                    result,
                    Err(ExtensionError::SkillImportZipCompressionRatio {
                        expanded_bytes: 201,
                        compressed_bytes: 1,
                        limit_ratio: 200,
                        ..
                    })
                ));
                assert_eq!(output.0, 0);
                assert_eq!(total, 201);
            }
        }
    }

    #[test]
    fn extractor_removes_partial_oversized_file_before_continuing() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("partial.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .compression_level(Some(6));
        writer.start_file("bad/data.bin", options).unwrap();
        let mut state = 1_u32;
        let mut block = [0_u8; 4096];
        for byte in &mut block {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let mut remaining = MAX_SKILL_IMPORT_FILE_BYTES + 1;
        while remaining > 0 {
            let count = remaining.min(block.len() as u64) as usize;
            writer.write_all(&block[..count]).unwrap();
            remaining -= count as u64;
        }
        writer.start_file("good/keep", options).unwrap();
        writer.write_all(b"retained").unwrap();
        writer.finish().unwrap();
        assert!(std::fs::metadata(&archive).unwrap().len() < 2 * 1024 * 1024);
        let destination = tmp.path().join("extracted");

        let oversized = extract_zip_archive(&archive, &destination).unwrap();

        assert_eq!(oversized.len(), 1);
        assert_eq!(oversized[0].relative_path, Path::new("bad/data.bin"));
        assert_eq!(oversized[0].file_bytes, MAX_SKILL_IMPORT_FILE_BYTES + 1);
        assert!(!destination.join("bad/data.bin").exists());
        assert_eq!(std::fs::read(destination.join("good/keep")).unwrap(), b"retained");
    }
}
