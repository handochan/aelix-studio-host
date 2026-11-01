//! Retirement applies only to the host-owned builtin tree/catalog, never custom skills.
use std::path::Path;

pub const RETIRED_BUILTINS: &[&str] = &["moltbook", "aionui-webui-public"];

pub(crate) fn is_retired(name: &str) -> bool {
    RETIRED_BUILTINS.contains(&name)
}

pub(crate) async fn remove_materialized(root: &Path) -> Result<(), std::io::Error> {
    for name in RETIRED_BUILTINS {
        for path in [root.join(name), root.join("auto-inject").join(name)] {
            match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) => {
                    if metadata.is_dir() && !metadata.file_type().is_symlink() {
                        tokio::fs::remove_dir_all(&path).await?;
                    } else {
                        tokio::fs::remove_file(&path).await?;
                    }
                    tracing::info!(skill = name, "retired host builtin removed");
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// A failed refresh must not re-enable old agent-facing installer instructions.
/// Inspect only the host-owned tree; imported custom skills are never scanned.
pub(crate) async fn has_legacy_installer_instructions(root: &Path) -> Result<bool, std::io::Error> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let mut entries = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                directories.push(entry.path());
            }
            if kind.is_file() && entry.file_name() == "SKILL.md" {
                let text = tokio::fs::read_to_string(entry.path()).await?.to_ascii_lowercase();
                if text.contains("d.officecli.ai/install.")
                    || text.contains("raw.githubusercontent.com/iofficeai/officecli/main/install.")
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
