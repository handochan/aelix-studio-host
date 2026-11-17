//! Validate compressed member extents before trusting them as ratio denominators.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use crate::error::ExtensionError;

use super::{enforce_entry_count, invalid_zip_io, zip_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct EntrySpan {
    header_start: u64,
    data_start: u64,
    data_end: u64,
}

pub(super) fn validate(archive: &mut zip::ZipArchive<File>, mut file: File) -> Result<(), ExtensionError> {
    let directory_start = archive.central_directory_start();
    let file_len = file.metadata()?.len();
    let mut spans = read_spans(&mut file, directory_start, archive.offset(), archive.len(), file_len)?;
    spans.sort_by_key(|span| span.header_start);
    for pair in spans.windows(2) {
        if pair[0].data_end > pair[1].header_start {
            return Err(invalid("compressed data overlaps another local header or member"));
        }
    }

    // Cross-check the manual raw-CD walk against the library's effective
    // metadata, including ZIP64/SFX offsets. by_index_raw never decompresses.
    let spans: HashSet<_> = spans.into_iter().collect();
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(zip_error)?;
        let data_start = entry.data_start().ok_or_else(|| invalid("missing member data start"))?;
        let span = EntrySpan {
            header_start: entry.header_start(),
            data_start,
            data_end: add(data_start, entry.compressed_size())?,
        };
        if !spans.contains(&span) {
            return Err(invalid("central directory disagrees with effective member extent"));
        }
    }
    Ok(())
}

fn read_spans(
    file: &mut (impl Read + Seek),
    directory_start: u64,
    archive_offset: u64,
    minimum_count: usize,
    file_len: u64,
) -> Result<Vec<EntrySpan>, ExtensionError> {
    file.seek(SeekFrom::Start(directory_start)).map_err(invalid_zip_io)?;
    let mut spans = Vec::new();
    loop {
        let mut signature = [0; 4];
        file.read_exact(&mut signature).map_err(invalid_zip_io)?;
        if signature != *b"PK\x01\x02" {
            // zip 9 deduplicates names; a shortened raw walk must never pass
            // just because an unexpected signature ended it early.
            if spans.len() < minimum_count {
                return Err(invalid("central directory ended before the expected entry count"));
            }
            return Ok(spans);
        }
        enforce_entry_count(spans.len() + 1)?;
        let mut header = [0; 42];
        file.read_exact(&mut header).map_err(invalid_zip_io)?;
        let name_len = u16_at(&header, 24);
        let extra_len = u16_at(&header, 26);
        let comment_len = u16_at(&header, 28);
        file.seek(SeekFrom::Current(i64::from(name_len)))
            .map_err(invalid_zip_io)?;
        let mut extra = vec![0; usize::from(extra_len)];
        file.read_exact(&mut extra).map_err(invalid_zip_io)?;
        file.seek(SeekFrom::Current(i64::from(comment_len)))
            .map_err(invalid_zip_io)?;
        let next_central_header = file.stream_position().map_err(invalid_zip_io)?;
        let (compressed_size, relative_header) = member_fields(&header, &extra)?;
        let header_start = add(archive_offset, relative_header)?;

        if add(header_start, 30)? > directory_start || directory_start > file_len {
            return Err(invalid("local header lies outside the archive data region"));
        }
        file.seek(SeekFrom::Start(header_start)).map_err(invalid_zip_io)?;
        let mut local = [0; 30];
        file.read_exact(&mut local).map_err(invalid_zip_io)?;
        if local[..4] != *b"PK\x03\x04" {
            return Err(invalid("invalid local header signature"));
        }
        let data_start = add(
            add(header_start, 30)?,
            u64::from(u16_at(&local, 26)) + u64::from(u16_at(&local, 28)),
        )?;
        let data_end = add(data_start, compressed_size)?;
        if data_start > directory_start || data_end > directory_start || data_end > file_len {
            return Err(invalid("compressed member range exceeds the archive data region"));
        }
        spans.push(EntrySpan {
            header_start,
            data_start,
            data_end,
        });
        file.seek(SeekFrom::Start(next_central_header))
            .map_err(invalid_zip_io)?;
    }
}

fn member_fields(header: &[u8; 42], extra: &[u8]) -> Result<(u64, u64), ExtensionError> {
    let compressed = u32_at(header, 16);
    let uncompressed = u32_at(header, 20);
    let offset = u32_at(header, 38);
    if compressed != u32::MAX && offset != u32::MAX {
        return Ok((u64::from(compressed), u64::from(offset)));
    }
    // APPNOTE 4.5.3 / zip 9's zip64_extended_information.rs: only fields
    // requested by a sentinel are present, in size/size/header order.
    let mut rest = extra;
    while rest.len() >= 4 {
        let id = u16_at(rest, 0);
        let len = usize::from(u16_at(rest, 2));
        let value = rest
            .get(4..4 + len)
            .ok_or_else(|| invalid("truncated ZIP64 extra field"))?;
        if id == 1 {
            let mut cursor = std::io::Cursor::new(value);
            if uncompressed == u32::MAX {
                read_u64(&mut cursor)?;
            }
            let compressed = if compressed == u32::MAX {
                read_u64(&mut cursor)?
            } else {
                u64::from(compressed)
            };
            let offset = if offset == u32::MAX {
                read_u64(&mut cursor)?
            } else {
                u64::from(offset)
            };
            return Ok((compressed, offset));
        }
        rest = &rest[4 + len..];
    }
    Err(invalid("missing ZIP64 member extent"))
}

fn read_u64(reader: &mut impl Read) -> Result<u64, ExtensionError> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes).map_err(invalid_zip_io)?;
    Ok(u64::from_le_bytes(bytes))
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn add(left: u64, right: u64) -> Result<u64, ExtensionError> {
    left.checked_add(right).ok_or_else(|| invalid("member extent overflow"))
}

fn invalid(reason: &str) -> ExtensionError {
    ExtensionError::SkillImportInvalidZip(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_entry_walk_fails_closed_on_early_end() {
        let mut reader = std::io::Cursor::new(b"PK\x05\x06");
        let error = read_spans(&mut reader, 0, 0, 1, 4).unwrap_err();
        assert!(matches!(error, ExtensionError::SkillImportInvalidZip(ref message)
            if message == "central directory ended before the expected entry count"));
    }
}
