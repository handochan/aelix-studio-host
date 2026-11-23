//! Capability-aware partition of message attachments into native media
//! blocks vs path-text files.
//!
//! Applied at agent dispatch time — the only layer that knows the target
//! agent's prompt capabilities. The persisted/broadcast message keeps the
//! full `[[AION_FILES]]` form (UI chips and history are untouched); only
//! the agent-bound copy is rewritten. Mirrors the aionrs precedent in
//! `manager/aionrs/content.rs`: strip the trailing marker block when it
//! matches `files` exactly, then rebuild.

use std::fs::Metadata;
use std::io::Read;
use std::path::Path;

#[cfg(test)]
use aionui_common::chat_attachment::ChatAttachment;
use aionui_common::chat_attachment::{PromptAttachment, inline_media_mime};

use aionui_common::constants::AIONUI_FILES_MARKER;
use tracing::warn;

use crate::AgentError;
use crate::types::{PromptMediaCaps, SendMessageData};

/// Max bytes for a single attachment sent as an inline base64 content block.
/// Above this the attachment degrades to a path (Claude's hard per-image API
/// limit, and a sane ceiling for one wire frame).
pub const MAX_MEDIA_BLOCK_BYTES: u64 = aionui_common::chat_attachment::MAX_INLINE_FILE_BYTES;
const MAX_PROMPT_MEDIA_BYTES: u64 = 50 * 1024 * 1024;

/// Coarse media classification for prompt content blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
}

/// An attachment that should be delivered as a native content block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAttachment {
    /// Absolute path on the local filesystem.
    pub path: String,
    /// Full mime type (e.g. `image/png`, `audio/mpeg`).
    pub mime: String,
    pub kind: MediaKind,
    /// Bytes reserved from this request at classification, also enforced during the read.
    pub byte_limit: u64,
    file_index: usize,
}

/// Result of [`partition_media`].
#[derive(Debug)]
pub struct MediaPartition {
    /// Agent-bound content: the user text with the `[[AION_FILES]]` block
    /// re-appended containing only the non-media paths. Byte-identical to the
    /// input when nothing partitions to media.
    pub content: String,
    /// Attachments that stay as path text / resource links, in order.
    pub path_files: Vec<String>,
    /// Attachments to send as native blocks, in order.
    pub media: Vec<MediaAttachment>,
}

/// Split `files` into native-block media vs path attachments, honoring the
/// agent's declared capabilities, and rewrite `content`'s trailing marker
/// block to list only the path attachments.
///
/// Degradation rules (attachment stays a path): capability not declared,
/// non-media mime, SVG (vision APIs reject it), file missing/unreadable, or
/// larger than [`MAX_MEDIA_BLOCK_BYTES`]. With `caps == default()` the input
/// passes through byte-identical.
pub fn partition_media(content: &str, files: &[String], caps: PromptMediaCaps) -> MediaPartition {
    partition_with_metadata(content, files, caps, &[])
}

pub(crate) async fn prepare_prompt_media(
    data: &SendMessageData,
    caps: PromptMediaCaps,
) -> Result<MediaPartition, AgentError> {
    if data.attachments.is_empty() {
        return Ok(partition_media(&data.content, &data.files, caps));
    }
    let content = data.content.clone();
    let files = data.files.clone();
    let attachments = data.attachments.clone();
    tokio::task::spawn_blocking(move || {
        let mut metadata = Vec::with_capacity(attachments.len());
        for attachment in &attachments {
            metadata.push(match attachment {
                PromptAttachment::Inline(_) => match attachment.pinned() {
                    Some(file) => FileStat::Verified(file.metadata().map_err(attachment_unavailable)?),
                    None => FileStat::PathOnly,
                },
                PromptAttachment::PathOnly => FileStat::PathOnly,
                PromptAttachment::Project => FileStat::ByPath,
            });
        }
        let partition = partition_with_metadata(&content, &files, caps, &metadata);
        let mut selected = partition.media.iter().map(|media| media.file_index).peekable();
        for (index, source) in attachments.iter().enumerate() {
            if selected.next_if_eq(&index).is_none() {
                source.release();
            }
        }
        Ok(partition)
    })
    .await
    .map_err(attachment_unavailable)?
}

fn attachment_unavailable(_: impl std::error::Error) -> AgentError {
    AgentError::bad_request("The message attachment changed or is unavailable.")
}

enum FileStat {
    Verified(Metadata),
    PathOnly,
    ByPath,
}

fn partition_with_metadata(
    content: &str,
    files: &[String],
    caps: PromptMediaCaps,
    metadata: &[FileStat],
) -> MediaPartition {
    if files.is_empty() || caps == PromptMediaCaps::default() {
        return MediaPartition {
            content: content.to_owned(),
            path_files: files.to_vec(),
            media: Vec::new(),
        };
    }

    let mut path_files = Vec::new();
    let mut media = Vec::new();
    let mut remaining = MAX_PROMPT_MEDIA_BYTES;
    for (index, path) in files.iter().enumerate() {
        match classify(
            path,
            caps,
            metadata.get(index).unwrap_or(&FileStat::ByPath),
            remaining,
            index,
        ) {
            Some(attachment) => {
                remaining -= attachment.byte_limit;
                media.push(attachment);
            }
            None => path_files.push(path.clone()),
        }
    }

    let content = if media.is_empty() {
        content.to_owned()
    } else {
        append_files_marker(strip_files_marker(content, files), &path_files)
    };
    MediaPartition {
        content,
        path_files,
        media,
    }
}

/// Classify one attachment; `Some` means "send as a native block".
fn classify(
    path: &str,
    caps: PromptMediaCaps,
    verified: &FileStat,
    remaining: u64,
    file_index: usize,
) -> Option<MediaAttachment> {
    let mime = inline_media_mime(path)?;
    let kind = match mime.type_().as_str() {
        // SVG is source text, not a raster image — vision APIs reject it.
        "image" if caps.image && mime.subtype() != "svg" => MediaKind::Image,
        "audio" if caps.audio => MediaKind::Audio,
        _ => return None,
    };
    let metadata = match verified {
        FileStat::Verified(stat) => Some(stat.clone()),
        FileStat::PathOnly => return None,
        FileStat::ByPath => std::fs::metadata(path).ok(),
    };
    match metadata {
        Some(meta) if meta.is_file() && meta.len() <= MAX_MEDIA_BLOCK_BYTES.min(remaining) => Some(MediaAttachment {
            path: path.to_owned(),
            byte_limit: meta.len(),
            file_index,
            mime: mime.essence_str().to_owned(),
            kind,
        }),
        Some(meta) if meta.is_file() => {
            warn!(
                bytes = meta.len(),
                "media attachment exceeds block size limit; sending as path"
            );
            None
        }
        _ => {
            warn!(
                code = "ATTACHMENT_UNAVAILABLE",
                "media attachment unreadable; sending as path"
            );
            None
        }
    }
}

/// Strip the trailing `[[AION_FILES]]` block iff its path list matches
/// `files` exactly (same validation as aionrs `strip_attachment_metadata`);
/// otherwise return `content` unchanged.
fn strip_files_marker<'a>(content: &'a str, files: &[String]) -> &'a str {
    let Some((user_text, metadata)) = content.rsplit_once(AIONUI_FILES_MARKER) else {
        return content;
    };
    let metadata_files = metadata.lines().map(str::trim).filter(|line| !line.is_empty());
    if metadata_files.eq(files.iter().map(String::as_str)) {
        user_text.strip_suffix("\n\n").unwrap_or(user_text)
    } else {
        content
    }
}

/// Re-append the marker block in the exact `resolve_chat_message` format.
fn append_files_marker(content: &str, paths: &[String]) -> String {
    if paths.is_empty() {
        content.to_owned()
    } else {
        format!("{content}\n\n{AIONUI_FILES_MARKER}\n{}", paths.join("\n"))
    }
}

/// The agent-bound text with the `[[AION_FILES]]` block listing EVERY
/// attachment — media included — regardless of what [`partition_media`] moved
/// to a native block.
///
/// For callers whose only path-delivery channel is the text itself (the ACP
/// `session/prompt` path emits no resource links: a non-media attachment rides
/// solely as a marker line). A native media block carries bytes, not a path, so
/// dropping the media path from the marker leaves such an agent able to see the
/// image but unable to open the file.
///
/// Not the same as returning `content` untouched: when `content` carries no
/// marker at all, this appends one for the full list, so a caller can never
/// lose the non-media paths [`partition_media`] would have re-appended. When
/// `content` does carry the exact marker for `files`, the result is
/// byte-identical to `content`.
pub fn content_with_all_paths(content: &str, files: &[String]) -> String {
    append_files_marker(strip_files_marker(content, files), files)
}

/// Read a media attachment's bytes, degrading to `None` (caller falls back to
/// the path form) when the file vanished or grew past the limit between
/// classification and read.
pub async fn read_media_bytes(attachment: &MediaAttachment) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let bounded = async {
        let file = tokio::fs::File::open(Path::new(&attachment.path)).await?;
        let mut bytes = Vec::new();
        file.take(attachment.byte_limit + 1).read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    };
    match bounded.await {
        Ok(bytes) if bytes.len() as u64 <= attachment.byte_limit => Some(bytes),
        Ok(_) => None,
        Err(_) => {
            warn!(
                code = "ATTACHMENT_UNAVAILABLE",
                "media attachment read failed; sending as path"
            );
            None
        }
    }
}

pub(crate) async fn read_prompt_media(
    media: &MediaAttachment,
    attachments: &[PromptAttachment],
) -> Result<Option<Vec<u8>>, AgentError> {
    let attachment = match attachments.get(media.file_index) {
        Some(source @ PromptAttachment::Inline(_)) => {
            let Some(file) = source.pinned() else {
                return Ok(None);
            };
            file
        }
        Some(PromptAttachment::PathOnly) => return Ok(None),
        Some(PromptAttachment::Project) => return Ok(read_media_bytes(media).await),
        None if attachments.is_empty() => return Ok(read_media_bytes(media).await),
        None => {
            return Err(AgentError::bad_request(
                "The message attachment changed or is unavailable.",
            ));
        }
    };
    let limit = media.byte_limit;
    let bytes = tokio::task::spawn_blocking(move || {
        attachment.with_file(|file| {
            let size = file.metadata()?.len();
            if size > limit {
                return Ok(None);
            }
            let mut bytes = Vec::with_capacity(size as usize);
            file.take(limit + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limit {
                return Ok(None);
            }
            Ok(Some(bytes))
        })
    })
    .await
    .map_err(attachment_unavailable)?
    .map_err(attachment_unavailable)?;
    if bytes.is_none() {
        attachments[media.file_index].release();
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS_IMAGE: PromptMediaCaps = PromptMediaCaps {
        image: true,
        audio: false,
    };
    const CAPS_ALL: PromptMediaCaps = PromptMediaCaps {
        image: true,
        audio: true,
    };

    fn inline(content: &str, paths: &[&str]) -> String {
        format!("{content}\n\n{AIONUI_FILES_MARKER}\n{}", paths.join("\n"))
    }

    fn temp_file(name: &str, bytes: &[u8]) -> String {
        let dir = std::env::temp_dir().join("aionui-media-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn cumulative_inline_budget_keeps_overflow_as_paths_and_bounds_the_actual_read() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image.png");
        std::fs::File::create(&image)
            .unwrap()
            .set_len(MAX_MEDIA_BLOCK_BYTES)
            .unwrap();
        let path = image.to_str().unwrap().to_owned();
        let tiny = dir.path().join("one.png");
        std::fs::write(&tiny, [1]).unwrap();
        let tiny = tiny.to_str().unwrap().to_owned();
        let below = dir.path().join("below.png");
        std::fs::File::create(&below)
            .unwrap()
            .set_len(MAX_MEDIA_BLOCK_BYTES - 1)
            .unwrap();
        let files = [vec![path.clone(); 9], vec![below.to_str().unwrap().to_owned()]].concat();
        let partition = partition_media("", &files, CAPS_IMAGE);
        assert_eq!(partition.media.len(), 10);
        assert!(partition.path_files.is_empty());
        assert_eq!(
            partition.media.iter().map(|m| m.byte_limit).sum::<u64>(),
            MAX_PROMPT_MEDIA_BYTES - 1
        );
        let files = [vec![path.clone(); 10], vec![tiny.clone(), path.clone()]].concat();
        let partition = partition_media("", &files, CAPS_IMAGE);
        assert_eq!(partition.media.len(), 10);
        assert_eq!(
            partition.media.iter().map(|m| m.byte_limit).sum::<u64>(),
            MAX_PROMPT_MEDIA_BYTES
        );
        assert_eq!(partition.path_files, [tiny.clone(), path.clone()]);
        assert!(partition.content.contains(&tiny));
        let files = [vec![path.clone(); 9], vec![tiny.clone(), path.clone(), tiny.clone()]].concat();
        let partition = partition_media("", &files, CAPS_IMAGE);
        assert_eq!(partition.media.len(), 11);
        assert_eq!(partition.path_files, [path]);
        let reserved = partition.media.last().unwrap();
        assert_eq!(reserved.byte_limit, 1);
        std::fs::write(&tiny, [1, 2]).unwrap();
        assert!(read_media_bytes(reserved).await.is_none());
    }

    #[tokio::test]
    async fn overflow_releases_only_that_occurrences_handle_across_turn_clones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.png");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_MEDIA_BLOCK_BYTES)
            .unwrap();
        let source = ChatAttachment::open(&path, None).unwrap();
        let files = vec![source.path().to_owned(); 11];
        let candidate = PromptAttachment::inline(source);
        let data = SendMessageData {
            content: String::new(),
            msg_id: "budget".into(),
            turn_id: None,
            files,
            attachments: (0..11).map(|_| candidate.repeat()).collect(),
            inject_skills: vec![],
        };
        let replay = data.clone();
        let selected = prepare_prompt_media(&data, CAPS_IMAGE).await.unwrap();
        assert_eq!(selected.media.len(), 10);
        assert!(replay.attachments[..10].iter().all(|a| a.pinned().is_some()));
        assert!(replay.attachments[10].pinned().is_none());
        assert_eq!(
            prepare_prompt_media(&replay, CAPS_IMAGE)
                .await
                .unwrap()
                .path_files
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn growth_after_classification_falls_back_and_releases_the_pin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("growing.png");
        std::fs::write(&path, b"small").unwrap();
        let file = ChatAttachment::open(&path, None).unwrap();
        let data = SendMessageData {
            content: String::new(),
            msg_id: "growth".into(),
            turn_id: None,
            files: vec![file.path().into()],
            attachments: vec![PromptAttachment::inline(file)],
            inject_skills: vec![],
        };
        let replay = data.clone();
        let selected = prepare_prompt_media(&data, CAPS_IMAGE).await.unwrap();
        std::fs::write(&path, b"larger").unwrap();
        assert!(
            read_prompt_media(&selected.media[0], &data.attachments)
                .await
                .unwrap()
                .is_none()
        );
        assert!(replay.attachments[0].pinned().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_canonical_paths_recheck_the_reference_at_the_media_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.png");
        let alias = dir.path().join("alias.png");
        std::fs::write(&path, b"original").unwrap();
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let local = ChatAttachment::open(&path, None).unwrap();
        let upload = ChatAttachment::open(&alias, Some(dir.path())).unwrap();
        assert_eq!(local.path(), upload.path());
        let data = SendMessageData {
            content: "duplicate".into(),
            msg_id: "id".into(),
            turn_id: None,
            files: vec![local.path().into(), upload.path().into()],
            attachments: vec![PromptAttachment::inline(local), PromptAttachment::inline(upload)],
            inject_skills: vec![],
        };
        let partition = prepare_prompt_media(&data, CAPS_IMAGE).await.unwrap();
        std::fs::remove_file(alias).unwrap();
        assert_eq!(
            read_prompt_media(&partition.media[0], &data.attachments)
                .await
                .unwrap()
                .unwrap(),
            b"original"
        );
        assert!(matches!(
            read_prompt_media(&partition.media[1], &data.attachments).await,
            Err(AgentError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn provenance_rejects_a_swap_between_media_classification_and_read() {
        for upload in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("managed");
            aionui_common::chat_attachment::ManagedUploadRoot::open(&root).unwrap();
            let path = root.join("attachment.png");
            std::fs::write(&path, b"selected bytes").unwrap();
            let source = ChatAttachment::open(&path, upload.then_some(root.as_path())).unwrap();
            let data = SendMessageData {
                content: "image attachment".into(),
                msg_id: "swap-fixture".into(),
                turn_id: None,
                files: vec![source.path().to_owned()],
                attachments: vec![PromptAttachment::inline(source)],
                inject_skills: vec![],
            };
            let partition = prepare_prompt_media(&data, CAPS_IMAGE).await.unwrap();
            let media = &partition.media[0];
            assert_eq!(
                read_prompt_media(media, &data.attachments).await.unwrap().unwrap(),
                b"selected bytes"
            );

            std::fs::rename(&path, dir.path().join("original.png")).unwrap();
            std::fs::write(&path, b"replacement").unwrap();
            assert!(matches!(
                read_prompt_media(media, &data.attachments).await,
                Err(AgentError::BadRequest(reason)) if reason == "The message attachment changed or is unavailable."
            ));
            assert!(matches!(
                prepare_prompt_media(&data, PromptMediaCaps::default()).await,
                Err(AgentError::BadRequest(reason)) if reason == "The message attachment changed or is unavailable."
            ));
        }
    }

    #[test]
    fn no_caps_is_byte_identical_passthrough() {
        let img = temp_file("a.png", b"png");
        let content = inline("hello", &[&img]);
        let part = partition_media(&content, std::slice::from_ref(&img), PromptMediaCaps::default());
        assert_eq!(part.content, content);
        assert_eq!(part.path_files, vec![img]);
        assert!(part.media.is_empty());
    }

    #[test]
    fn image_partitions_to_media_and_marker_is_removed() {
        let img = temp_file("b.png", b"png");
        let content = inline("look at this", &[&img]);
        let part = partition_media(&content, std::slice::from_ref(&img), CAPS_IMAGE);
        assert_eq!(part.content, "look at this");
        assert!(part.path_files.is_empty());
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].mime, "image/png");
        assert_eq!(part.media[0].kind, MediaKind::Image);
    }

    #[test]
    fn mixed_files_keep_non_media_in_marker() {
        let img = temp_file("c.jpg", b"jpg");
        let doc = temp_file("c.pdf", b"pdf");
        let content = inline("mix", &[&img, &doc]);
        let part = partition_media(&content, &[img.clone(), doc.clone()], CAPS_IMAGE);
        assert_eq!(part.content, inline("mix", &[&doc]));
        assert_eq!(part.path_files, vec![doc]);
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].path, img);
    }

    #[test]
    fn audio_needs_audio_cap() {
        let mp3 = temp_file("d.mp3", b"mp3");
        let content = inline("song", &[&mp3]);
        let no_audio = partition_media(&content, std::slice::from_ref(&mp3), CAPS_IMAGE);
        assert!(no_audio.media.is_empty());
        assert_eq!(no_audio.content, content);
        let with_audio = partition_media(&content, std::slice::from_ref(&mp3), CAPS_ALL);
        assert_eq!(with_audio.media.len(), 1);
        assert_eq!(with_audio.media[0].kind, MediaKind::Audio);
        assert_eq!(with_audio.media[0].mime, "audio/mpeg");
    }

    #[test]
    fn svg_and_missing_and_oversized_stay_paths() {
        let svg = temp_file("e.svg", b"<svg/>");
        let missing = "/nonexistent/aionui-media-test.png".to_owned();
        let big = temp_file("f.png", &vec![0u8; (MAX_MEDIA_BLOCK_BYTES + 1) as usize]);
        let files = vec![svg, missing, big];
        let content = inline("all degrade", &[&files[0], &files[1], &files[2]]);
        let part = partition_media(&content, &files, CAPS_ALL);
        assert!(part.media.is_empty());
        assert_eq!(part.path_files, files);
        assert_eq!(part.content, content);
    }

    #[test]
    fn all_paths_content_keeps_media_paths_in_the_marker() {
        let img = temp_file("h.png", b"png");
        let doc = temp_file("h.pdf", b"pdf");
        let files = vec![img.clone(), doc.clone()];
        let content = inline("mix", &[&img, &doc]);
        // partition drops the image path from the marker...
        let part = partition_media(&content, &files, CAPS_IMAGE);
        assert_eq!(part.content, inline("mix", &[&doc]));
        // ...while the all-paths form keeps both, byte-identical to the input.
        assert_eq!(content_with_all_paths(&content, &files), content);
    }

    #[test]
    fn all_paths_content_appends_a_marker_when_none_present() {
        // The trap this helper exists for: with no marker in `content`, falling
        // back to the raw text would lose EVERY path, and partition would have
        // appended a marker for the non-media ones. Rebuild the full list.
        let img = temp_file("i.png", b"png");
        let doc = temp_file("i.pdf", b"pdf");
        let files = vec![img.clone(), doc.clone()];
        assert_eq!(content_with_all_paths("bare", &files), inline("bare", &[&img, &doc]));
    }

    #[test]
    fn all_paths_content_is_a_noop_without_files() {
        assert_eq!(content_with_all_paths("just text", &[]), "just text");
    }

    #[test]
    fn content_without_marker_still_partitions() {
        let img = temp_file("g.webp", b"webp");
        let part = partition_media("bare", std::slice::from_ref(&img), CAPS_IMAGE);
        assert_eq!(part.content, "bare");
        assert_eq!(part.media.len(), 1);
        assert_eq!(part.media[0].mime, "image/webp");
    }
}
