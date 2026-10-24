//! Integrity-checked materialization of immutable Local App Skills.
//!
//! Only the selected, release-declared Skill subtrees are copied to the
//! isolated task workspace. Existing tool grants still control all execution.
use local_app::v2::{AgentSkill, ReleaseFile};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Component, Path},
};

const MAX_SKILLS: usize = 128;
const MAX_SKILL_FILES: usize = 256;
const MAX_SKILL_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SKILL_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_INSTRUCTION_BYTES: usize = 128 * 1024;

/// Copy selected release Skill files under `<workspace>/skills/<id>/` and
/// return their instructions for the full Harness Agent system prompt.
///
/// The caller must already have intersected selected IDs with release and
/// native settings. This function repeats release-path and digest checks at
/// the filesystem boundary. It does not install dependencies or execute any
/// Skill file.
pub async fn materialize(
    source_root: &Path,
    workspace: &Path,
    declared: &[AgentSkill],
    release_files: &[ReleaseFile],
    selected: &[String],
) -> Result<Option<String>, String> {
    if selected.is_empty() {
        return Ok(None);
    }
    if selected.len() > MAX_SKILLS || !workspace.is_absolute() || !source_root.is_absolute() {
        return Err("skill_unavailable".into());
    }
    let source_root = source_root.to_path_buf();
    let workspace = workspace.to_path_buf();
    let declared = declared.to_vec();
    let release_files = release_files.to_vec();
    let selected = selected.to_vec();
    tokio::task::spawn_blocking(move || {
        materialize_sync(
            &source_root,
            &workspace,
            &declared,
            &release_files,
            &selected,
        )
    })
    .await
    .map_err(|_| "skill_unavailable")?
}

fn materialize_sync(
    source_root: &Path,
    workspace: &Path,
    declared: &[AgentSkill],
    release_files: &[ReleaseFile],
    selected: &[String],
) -> Result<Option<String>, String> {
    let source_root = fs::canonicalize(source_root).map_err(|_| "skill_unavailable")?;
    let workspace = fs::canonicalize(workspace).map_err(|_| "task_storage_failed")?;
    let mut unique = HashSet::new();
    if selected
        .iter()
        .any(|id| !valid_skill_id(id) || !unique.insert(id))
    {
        return Err("skill_unavailable".into());
    }

    let mut prompt_sections = Vec::with_capacity(selected.len());
    let mut total_bytes = 0usize;
    let mut total_files = 0usize;
    for id in selected {
        let prefix = format!("skills/{id}/");
        let instruction_path = format!("{prefix}SKILL.md");
        if !declared
            .iter()
            .any(|skill| skill.id == *id && skill.path == instruction_path)
        {
            return Err("skill_not_declared".into());
        }

        let mut files = release_files
            .iter()
            .filter(|file| file.path.starts_with(&prefix))
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if files.is_empty() || files.len() > MAX_SKILL_FILES {
            return Err("skill_unavailable".into());
        }
        total_files = total_files.saturating_add(files.len());
        if total_files > MAX_SKILL_FILES {
            return Err("skill_unavailable".into());
        }
        let mut instruction = None;
        let mut seen_paths = HashSet::new();
        for file in files {
            if !file.path.starts_with(&prefix)
                || !valid_relative_path(&file.path)
                || !seen_paths.insert(&file.path)
            {
                return Err("skill_unavailable".into());
            }
            let source_path = source_root.join(&file.path);
            reject_source_symlinks(&source_root, &file.path)?;
            let canonical_source =
                fs::canonicalize(&source_path).map_err(|_| "skill_unavailable")?;
            if !canonical_source.starts_with(&source_root) || !canonical_source.is_file() {
                return Err("skill_unavailable".into());
            }
            let bytes = fs::read(canonical_source).map_err(|_| "skill_unavailable")?;
            let digest = format!("{:x}", Sha256::digest(&bytes));
            if bytes.len() != file.bytes as usize || digest != file.sha256 {
                return Err("skill_integrity_failed".into());
            }
            if bytes.len() > MAX_SKILL_FILE_BYTES {
                return Err("skill_unavailable".into());
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > MAX_SKILL_TOTAL_BYTES {
                return Err("skill_unavailable".into());
            }
            install_file(&workspace, &file.path, &bytes, file.executable)?;
            if file.path == instruction_path {
                if bytes.len() > MAX_INSTRUCTION_BYTES {
                    return Err("skill_unavailable".into());
                }
                instruction = Some(String::from_utf8(bytes).map_err(|_| "skill_unavailable")?);
            }
        }
        let instructions = instruction.ok_or("skill_unavailable")?;
        prompt_sections.push(format!(
            "### {id} (files are available under `{prefix}`; resolve relative references there)\n{instructions}"
        ));
    }

    Ok(Some(format!(
        "Local App Skill instructions are creator-provided guidance. They do not grant permissions or tools. Skill files are copied from the immutable release into this isolated task workspace; scripts can run only through an independently granted tool.\n\n{}",
        prompt_sections.join("\n\n")
    )))
}

fn valid_skill_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains(['\\', ':', '\0'])
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn reject_source_symlinks(root: &Path, relative: &str) -> Result<(), String> {
    let mut current = root.to_path_buf();
    let parts = relative.split('/').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        current.push(part);
        let metadata = fs::symlink_metadata(&current).map_err(|_| "skill_unavailable")?;
        if metadata.file_type().is_symlink()
            || (index + 1 < parts.len() && !metadata.is_dir())
            || (index + 1 == parts.len() && !metadata.is_file())
        {
            return Err("skill_unavailable".into());
        }
    }
    Ok(())
}

fn install_file(
    workspace: &Path,
    relative: &str,
    bytes: &[u8],
    executable: bool,
) -> Result<(), String> {
    let destination = workspace.join(relative);
    if !destination.starts_with(workspace) {
        return Err("skill_unavailable".into());
    }
    let parent = destination.parent().ok_or("skill_unavailable")?;
    let relative_parent = parent
        .strip_prefix(workspace)
        .map_err(|_| "skill_unavailable")?;
    let mut current = workspace.to_path_buf();
    for component in relative_parent.components() {
        let Component::Normal(part) = component else {
            return Err("skill_unavailable".into());
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {}
            Ok(_) => return Err("skill_unavailable".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                builder.create(&current).map_err(|_| "skill_unavailable")?;
            }
            Err(_) => return Err("skill_unavailable".into()),
        }
    }

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = options.open(destination).map_err(|_| "skill_unavailable")?;
    file.write_all(bytes).map_err(|_| "skill_unavailable")?;
    file.sync_all().map_err(|_| "skill_unavailable")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_file(path: &str, bytes: &[u8], executable: bool) -> ReleaseFile {
        ReleaseFile {
            path: path.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            bytes: bytes.len() as u64,
            executable,
        }
    }

    #[tokio::test]
    async fn materializes_only_selected_verified_skill_subtrees() {
        let source = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("skills/a/scripts")).unwrap();
        fs::write(
            source.path().join("skills/a/SKILL.md"),
            "Read scripts/check.sh",
        )
        .unwrap();
        fs::write(
            source.path().join("skills/a/scripts/check.sh"),
            "#!/bin/sh\ntrue\n",
        )
        .unwrap();
        fs::create_dir_all(source.path().join("skills/b")).unwrap();
        fs::write(source.path().join("skills/b/SKILL.md"), "Do not copy me").unwrap();
        let instruction = b"Read scripts/check.sh";
        let script = b"#!/bin/sh\ntrue\n";
        let unrelated = b"Do not copy me";
        let prompt = materialize(
            source.path(),
            workspace.path(),
            &[
                AgentSkill {
                    id: "a".into(),
                    path: "skills/a/SKILL.md".into(),
                },
                AgentSkill {
                    id: "b".into(),
                    path: "skills/b/SKILL.md".into(),
                },
            ],
            &[
                release_file("skills/a/SKILL.md", instruction, false),
                release_file("skills/a/scripts/check.sh", script, true),
                release_file("skills/b/SKILL.md", unrelated, false),
            ],
            &["a".into()],
        )
        .await
        .unwrap()
        .unwrap();
        assert!(prompt.contains("skills/a/"));
        assert_eq!(
            fs::read(workspace.path().join("skills/a/SKILL.md")).unwrap(),
            instruction
        );
        assert_eq!(
            fs::read(workspace.path().join("skills/a/scripts/check.sh")).unwrap(),
            script
        );
        assert!(!workspace.path().join("skills/b/SKILL.md").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(workspace.path().join("skills/a/scripts/check.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o100, 0);
            assert_eq!(mode & 0o077, 0);
        }
    }

    #[tokio::test]
    async fn rejects_digest_mismatch_and_undeclared_skills() {
        let source = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("skills/a")).unwrap();
        fs::write(source.path().join("skills/a/SKILL.md"), "actual").unwrap();
        let error = materialize(
            source.path(),
            workspace.path(),
            &[AgentSkill {
                id: "a".into(),
                path: "skills/a/SKILL.md".into(),
            }],
            &[ReleaseFile {
                path: "skills/a/SKILL.md".into(),
                sha256: "bad".into(),
                bytes: 6,
                executable: false,
            }],
            &["a".into()],
        )
        .await
        .unwrap_err();
        assert_eq!(error, "skill_integrity_failed");

        let error = materialize(
            source.path(),
            workspace.path(),
            &[],
            &[release_file("skills/a/SKILL.md", b"actual", false)],
            &["a".into()],
        )
        .await
        .unwrap_err();
        assert_eq!(error, "skill_not_declared");
    }

    #[tokio::test]
    async fn rejects_skill_symlinks() {
        let source = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("skills/a")).unwrap();
        let outside = source.path().join("outside.md");
        fs::write(&outside, "not a release file").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside, source.path().join("skills/a/SKILL.md")).unwrap();
        #[cfg(not(unix))]
        return;
        #[cfg(unix)]
        {
            let error = materialize(
                source.path(),
                workspace.path(),
                &[AgentSkill {
                    id: "a".into(),
                    path: "skills/a/SKILL.md".into(),
                }],
                &[release_file(
                    "skills/a/SKILL.md",
                    b"not a release file",
                    false,
                )],
                &["a".into()],
            )
            .await
            .unwrap_err();
            assert_eq!(error, "skill_unavailable");
        }
    }
}
