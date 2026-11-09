use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
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

pub(super) fn extract_zip_archive(archive_path: &Path, destination: &Path) -> Result<(), ExtensionError> {
    let file = std::fs::File::open(archive_path)?;
    let raw_entry_reader = file.try_clone()?;
    let mut archive = zip::ZipArchive::new(file).map_err(zip_error)?;
    enforce_entry_count(archive.len())?;
    if archive.has_duplicate_names() {
        enforce_raw_entry_count(raw_entry_reader, archive.central_directory_start())?;
    }
    let mut output_paths = HashSet::new();
    let mut total_bytes = 0_u64;

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
        copy_entry(&mut entry, &mut output, &entry_name, compressed_bytes, &mut total_bytes)?;
    }

    Ok(())
}

fn enforce_raw_entry_count(file: std::fs::File, directory_start: u64) -> Result<(), ExtensionError> {
    // zip 9's len() counts unique names. Preserve supported duplicate entries,
    // but bound their raw count too. The library has already validated this
    // central directory; only walk its length-delimited records, without data
    // allocation/decompression, and stop immediately on the 4097th record.
    let mut reader = io::BufReader::new(file);
    reader.seek(SeekFrom::Start(directory_start)).map_err(invalid_zip_io)?;
    let mut count = 0;
    loop {
        let mut signature = [0; 4];
        reader.read_exact(&mut signature).map_err(invalid_zip_io)?;
        if signature != *b"PK\x01\x02" {
            return Ok(());
        }
        count += 1;
        enforce_entry_count(count)?;
        let mut header = [0; 42];
        reader.read_exact(&mut header).map_err(invalid_zip_io)?;
        let skip: i64 = [24, 26, 28]
            .into_iter()
            .map(|offset| i64::from(u16::from_le_bytes([header[offset], header[offset + 1]])))
            .sum();
        reader.seek(SeekFrom::Current(skip)).map_err(invalid_zip_io)?;
    }
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
        if next_file_bytes > MAX_SKILL_IMPORT_FILE_BYTES {
            return Err(ExtensionError::SkillImportFileTooLarge {
                file_path: Some(name.to_owned()),
                file_bytes: next_file_bytes,
                limit_bytes: MAX_SKILL_IMPORT_FILE_BYTES,
            });
        }
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
        output.write_all(&buffer[..count])?;
        file_bytes = next_file_bytes;
        *total_bytes = next_total_bytes;
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
        assert_eq!(total, output.0);
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
        assert_eq!(total, initial);
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
                assert_eq!(total, 0);
            }
        }
    }
}
