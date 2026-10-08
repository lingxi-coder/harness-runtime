//! Host implementation of the Mod `fs.ancestors` operation.

use hooks::mods::ModError;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_path(cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn worktree_roots(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let worktree = git_path(cwd, &["rev-parse", "--show-toplevel"])?;
    let common_dir = git_path(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common_dir = std::fs::canonicalize(common_dir).ok()?;
    if common_dir.file_name()? != ".git" {
        return None;
    }
    let main = common_dir.parent()?.to_path_buf();
    (main != worktree).then_some((main, worktree))
}

fn is_other_worktree_ancestor(directory: &Path, roots: Option<&(PathBuf, PathBuf)>) -> bool {
    roots.is_some_and(|(main, worktree)| {
        directory.starts_with(main) && !directory.starts_with(worktree)
    })
}

fn resolve_mod_ancestor_path(raw: &str, cwd: &Path) -> Result<PathBuf, ModError> {
    if raw.starts_with("//") || raw.starts_with("\\\\") {
        return Err(ModError::Hook(
            "network file paths are not supported".into(),
        ));
    }
    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

fn lexical_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub(super) async fn run(
    input: Value,
    original_cwd: PathBuf,
    include_external: bool,
    excluder: Option<memory::lingxi_md::LingxiMdExcluder>,
) -> Result<Value, ModError> {
    let names = input
        .get("names")
        .and_then(Value::as_array)
        .ok_or_else(|| ModError::Hook("takes names, a list of relative .md file names".into()))?;
    let path_arg = |key: &str| -> Result<Option<PathBuf>, ModError> {
        match input.get(key) {
            None => Ok(None),
            Some(Value::String(path)) if !path.is_empty() => Ok(Some(lexical_path(
                &resolve_mod_ancestor_path(path, &original_cwd)?,
            ))),
            _ => Err(ModError::Hook(format!("takes {key}, a path, when given"))),
        }
    };
    let of = path_arg("of")?;
    let below = path_arg("below")?;
    let mut names_checked = Vec::with_capacity(names.len());
    for name in names {
        let name = name
            .as_str()
            .ok_or_else(|| ModError::Hook(format!("takes names, each a .md file name ({name})")))?;
        if !name.to_ascii_lowercase().ends_with(".md") {
            return Err(ModError::Hook(format!(
                "takes names, each a .md file name ({})",
                json!(name)
            )));
        }
        let drive_absolute = name.as_bytes().get(1) == Some(&b':')
            && name
                .as_bytes()
                .get(2)
                .is_some_and(|separator| matches!(*separator, b'/' | b'\\'));
        if name.starts_with('/')
            || name.starts_with('\\')
            || drive_absolute
            || name.split(['/', '\\']).any(|component| component == "..")
        {
            return Err(ModError::Hook(format!(
                "takes names, each relative with no \"..\" ({name})"
            )));
        }
        names_checked.push(name.to_owned());
    }
    let start = match of.as_deref() {
        Some(path) if path != original_cwd => path.parent().unwrap_or(path).to_path_buf(),
        _ => original_cwd.clone(),
    };
    tokio::task::spawn_blocking(move || {
        if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|value| !value.is_empty()) {
            return Ok(Value::Array(Vec::new()));
        }
        let home = dirs::home_dir();
        let roots = worktree_roots(&original_cwd);
        let mut processed = HashSet::new();
        processed.insert(memory::lingxi_md::hierarchy::managed_path().join("LINGXI.md"));
        if let Some(home) = home.as_deref() {
            processed.insert(memory::lingxi_md::hierarchy::user_config_dir(home).join("LINGXI.md"));
        }
        let mut directories = Vec::new();
        let mut cursor = start.as_path();
        loop {
            let Some(parent) = cursor.parent() else { break };
            if parent == cursor {
                break;
            }
            if below
                .as_ref()
                .is_some_and(|below| cursor == below || !cursor.starts_with(below))
            {
                break;
            }
            directories.push(cursor.to_path_buf());
            cursor = parent;
        }
        directories.reverse();
        let mut found = Vec::new();
        for directory in directories {
            if is_other_worktree_ancestor(&directory, roots.as_ref()) {
                continue;
            }
            for name in &names_checked {
                let path = directory.join(name);
                let expanded = memory::lingxi_md::loader::expand_memory_file_with_excluder(
                    &path,
                    &mut processed,
                    include_external,
                    &original_cwd,
                    home.as_deref(),
                    0,
                    memory::lingxi_md::LingxiMdTier::Project,
                    excluder.as_ref(),
                );
                if expanded.is_empty() {
                    continue;
                }
                let parts = expanded
                    .iter()
                    .map(|entry| {
                        json!({
                            "path": entry.path.to_string_lossy(),
                            "content": entry.body.clone(),
                        })
                    })
                    .collect::<Vec<_>>();
                let content = expanded
                    .iter()
                    .map(|entry| entry.body.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                found.push(json!({
                    "dir":directory.to_string_lossy(),
                    "name":name,
                    "content":content,
                    "parts":parts,
                }));
            }
        }
        Ok::<Value, ModError>(Value::Array(found))
    })
    .await
    .map_err(|error| ModError::Unavailable(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn walks_root_first_and_preserves_included_parts() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("AGENTS.md"), "project\n@./extra.md\n").unwrap();
        std::fs::write(project.join("extra.md"), "extra\n").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested\n").unwrap();

        let found = run(
            json!({
                "names": ["AGENTS.md"],
                "of": "nested/source.rs",
                "below": temp.path(),
            }),
            project.clone(),
            false,
            None,
        )
        .await
        .unwrap();
        let entries = found.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["dir"], project.to_string_lossy().as_ref());
        assert_eq!(entries[1]["dir"], nested.to_string_lossy().as_ref());
        assert_eq!(entries[0]["name"], "AGENTS.md");
        assert_eq!(entries[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(
            entries[0]["parts"][0]["path"],
            project.join("AGENTS.md").to_string_lossy().as_ref()
        );
        assert_eq!(
            entries[0]["parts"][1]["path"],
            project.join("extra.md").to_string_lossy().as_ref()
        );
        assert_eq!(
            entries[0]["content"],
            format!(
                "{}\n\n{}",
                entries[0]["parts"][0]["content"].as_str().unwrap(),
                entries[0]["parts"][1]["content"].as_str().unwrap()
            )
        );
    }

    #[tokio::test]
    async fn below_is_exclusive_and_unrelated_below_returns_empty() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(project.join("AGENTS.md"), "project").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "nested").unwrap();
        let input = json!({"names":["AGENTS.md"],"of":"nested/source.rs","below":project});
        let found = run(input, project.clone(), false, None).await.unwrap();
        assert_eq!(found.as_array().unwrap().len(), 1);
        assert_eq!(found[0]["dir"], nested.to_string_lossy().as_ref());
        let unrelated = run(
            json!({"names":["AGENTS.md"],"below":temp.path().join("other")}),
            project,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(unrelated, json!([]));
    }

    #[tokio::test]
    async fn of_resolving_to_cwd_keeps_cwd_in_walk() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "here").unwrap();
        let found = run(
            json!({"names":["AGENTS.md"], "of":"."}),
            temp.path().to_path_buf(),
            false,
            None,
        )
        .await
        .unwrap();
        assert!(found
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["dir"] == temp.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn follows_project_memory_excludes() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("AGENTS.md");
        std::fs::write(&file, "excluded").unwrap();
        let excluder =
            memory::lingxi_md::LingxiMdExcluder::new(&[file.to_string_lossy().into_owned()]);
        let result = run(
            json!({"names":["AGENTS.md"]}),
            temp.path().to_path_buf(),
            false,
            Some(excluder),
        )
        .await
        .unwrap();
        assert_eq!(result, json!([]));
    }

    #[tokio::test]
    async fn rejects_invalid_names_and_network_paths() {
        let temp = tempfile::tempdir().unwrap();
        for names in [
            json!(["../AGENTS.md"]),
            json!(["/AGENTS.md"]),
            json!(["AGENTS.txt"]),
        ] {
            assert!(run(
                json!({"names":names}),
                temp.path().to_path_buf(),
                false,
                None
            )
            .await
            .is_err());
        }
        assert!(run(
            json!({"names":["AGENTS.md"],"of":"//server/path"}),
            temp.path().to_path_buf(),
            false,
            None
        )
        .await
        .is_err());
        assert!(run(
            json!({"names":["AGENTS.md"],"below":""}),
            temp.path().to_path_buf(),
            false,
            None
        )
        .await
        .is_err());
    }

    #[test]
    fn skips_main_checkout_ancestors_of_a_linked_worktree() {
        let roots = (
            PathBuf::from("/repo"),
            PathBuf::from("/repo/.worktrees/mods"),
        );
        assert!(is_other_worktree_ancestor(Path::new("/repo"), Some(&roots)));
        assert!(is_other_worktree_ancestor(
            Path::new("/repo/.worktrees"),
            Some(&roots)
        ));
        assert!(!is_other_worktree_ancestor(
            Path::new("/repo/.worktrees/mods"),
            Some(&roots)
        ));
        assert!(!is_other_worktree_ancestor(Path::new("/tmp"), Some(&roots)));
    }
}
