//! Retirement applies only to host-owned builtins, never imported custom skills.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const RETIRED_BUILTINS: &[&str] = &["moltbook", "aionui-webui-public"];
static DEGRADED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
fn roots() -> &'static Mutex<HashSet<PathBuf>> {
    DEGRADED.get_or_init(|| Mutex::new(HashSet::new()))
}
pub(crate) fn mark_degraded(root: &Path) {
    roots().lock().unwrap().insert(root.to_path_buf());
}
pub(crate) fn clear_degraded(root: &Path) {
    roots().lock().unwrap().remove(root);
}
pub(crate) fn is_degraded(root: &Path) -> bool {
    roots().lock().unwrap().contains(root)
}
pub(crate) fn is_retired(name: &str) -> bool {
    RETIRED_BUILTINS.contains(&name)
}
pub(crate) fn office_family(name: &str) -> bool {
    name == "officecli" || name.starts_with("officecli-") || matches!(name, "morph-ppt" | "morph-ppt-3d")
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

pub(crate) fn forbidden_text(root: &Path, path: &Path, text: &str) -> bool {
    // Scope deployment wording by the owning skill's path, not its display name
    // or a reference filename. Legacy flat markdown skills are supported too.
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = relative.components().filter_map(|part| match part {
        std::path::Component::Normal(name) => name.to_str(),
        _ => None,
    });
    let first = parts.next().unwrap_or("");
    let skill = if first.eq_ignore_ascii_case("auto-inject") {
        parts.next().unwrap_or("")
    } else {
        first
    };
    let webui_setup =
        skill.eq_ignore_ascii_case("aionui-webui-setup") || skill.eq_ignore_ascii_case("aionui-webui-setup.md");
    let text = text.to_ascii_lowercase();
    text.contains("d.officecli.ai/install.")
        || text.contains("raw.githubusercontent.com/iofficeai/officecli/main/install.")
        || text.contains("remote-internet-access-guide")
        || text.contains("curl ifconfig.me")
        || text.contains("<公网ip>")
        || (webui_setup
            && [
                "多用户、24/7",
                "需要多用户访问或 24/7 运行",
                "需要 24/7 运行",
                "部署在云服务器上",
                "云服务器（linux/macos）",
                "检查云服务器安全组规则",
                "生产环境建议配置 https",
                "使用 nginx 等反向代理",
            ]
            .iter()
            .any(|phrase| text.contains(phrase)))
}

async fn safe_tree(root: &Path, path: &Path) -> Result<bool, std::io::Error> {
    if path.is_file() {
        return Ok(!forbidden_text(root, path, &tokio::fs::read_to_string(path).await?));
    }
    let mut directories = vec![path.to_path_buf()];
    while let Some(dir) = directories.pop() {
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                directories.push(entry.path());
            }
            if (kind.is_file() || kind.is_symlink())
                && entry.path().extension().is_some_and(|e| e == "md")
                && forbidden_text(root, &entry.path(), &tokio::fs::read_to_string(entry.path()).await?)
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Central read/use gate: stale unsafe files are never returned even if cleanup fails.
pub(crate) async fn available(root: &Path, name: &str, path: &Path) -> bool {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let directory_name = path.file_name().and_then(|n| n.to_str()).unwrap_or(name);
    let blocked = is_retired(name)
        || is_retired(directory_name)
        || (is_degraded(root)
            && (office_family(name)
                || office_family(directory_name)
                || path.file_stem().and_then(|n| n.to_str()).is_some_and(office_family)))
        || !safe_tree(root, &path).await.unwrap_or(false);
    if blocked {
        tracing::warn!(
            skill = name,
            reason = "unsafe_or_stale_builtin",
            "builtin skill hidden; restore the bundled skills to use it"
        );
    }
    !blocked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_webui_deployment_guidance_is_hidden_but_current_private_guidance_is_safe() {
        for old in [
            "服务器部署：多用户、24/7 运行",
            "需要多用户访问或 24/7 运行",
            "部署在云服务器上",
            "云服务器（Linux/macOS）",
            "检查云服务器安全组规则",
            "生产环境建议配置 HTTPS",
            "使用 Nginx 等反向代理",
        ] {
            assert!(
                forbidden_text(Path::new(""), Path::new("aionui-webui-setup/SKILL.md"), old),
                "stale guidance must be hidden: {old}"
            );
            assert!(
                !forbidden_text(
                    Path::new(""),
                    Path::new("other-skill/references/aionui-webui-setup.md"),
                    old,
                ),
                "deployment wording must stay available outside webui-setup: {old}"
            );
        }
        assert!(!forbidden_text(
            Path::new(""),
            Path::new("aionui-webui-setup/SKILL.md"),
            include_str!("../../aionui-app/assets/builtin-skills/aionui-webui-setup/SKILL.md")
        ));
        assert!(!forbidden_text(
            Path::new(""),
            Path::new("aionui-webui-setup/SKILL.md"),
            include_str!("../../aionui-app/assets/builtin-skills/aionui-webui-setup/references/aionui-webui.md")
        ));
    }
    #[test]
    fn original_forbidden_instructions_still_apply_to_every_builtin() {
        for text in [
            "https://d.officecli.ai/install.sh",
            "https://raw.githubusercontent.com/iOfficeAI/OfficeCLI/main/install.ps1",
            "Remote-Internet-Access-Guide",
            "curl ifconfig.me",
            "http://<公网IP>:25808",
        ] {
            for skill in ["other-skill", "aionui-webui-setup"] {
                assert!(forbidden_text(Path::new(""), &Path::new(skill).join("SKILL.md"), text));
            }
        }
    }

    #[tokio::test]
    async fn deployment_wording_only_hides_its_webui_owner_in_list_read_and_use_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::resolve_skill_paths(tmp.path(), tmp.path());
        let root = &paths.builtin_skills_dir;
        for name in ["other-skill", "aionui-webui-setup"] {
            let dir = root.join(name);
            tokio::fs::create_dir_all(dir.join("references")).await.unwrap();
            tokio::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: Fixture\n---\nSafe entry point"),
            )
            .await
            .unwrap();
            // A reference filename cannot make another skill count as webui-setup.
            tokio::fs::write(
                dir.join("references/aionui-webui-setup.md"),
                "部署在云服务器上\n需要 24/7 运行\n使用 Nginx 等反向代理",
            )
            .await
            .unwrap();
        }
        assert!(available(root, "aionui-webui-setup", &root.join("other-skill")).await);
        let list = crate::list_available_skills(&paths).await.unwrap();
        assert!(list.iter().any(|skill| skill.name == "other-skill"));
        assert!(!list.iter().any(|skill| skill.name == "aionui-webui-setup"));
        for prefix in ["", "./"] {
            assert!(
                !crate::read_builtin_skill(&paths, &format!("{prefix}other-skill/references/aionui-webui-setup.md"))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                crate::read_builtin_skill(
                    &paths,
                    &format!("{prefix}aionui-webui-setup/references/aionui-webui-setup.md")
                )
                .await
                .unwrap()
                .is_empty()
            );
        }
        assert_eq!(
            crate::materialize_skills_for_agent(&paths, "conv-other", &["other-skill".into()])
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            crate::materialize_skills_for_agent(&paths, "conv-webui", &["aionui-webui-setup".into()])
                .await
                .unwrap()
                .is_empty()
        );
        // Cover auto-inject and the legacy flat-file layout without extra I/O.
        assert!(forbidden_text(
            root,
            &root.join("auto-inject/aionui-webui-setup/references/guide.md"),
            "部署在云服务器上"
        ));
        assert!(forbidden_text(
            root,
            &root.join("aionui-webui-setup.md"),
            "部署在云服务器上"
        ));
    }
}
