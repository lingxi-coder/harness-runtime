//! Shared task-output names and session layout. Hosts supply their temp root;
//! mobile keeps its explicitly different root.

use std::path::{Path, PathBuf};

/// Sanitize the project cwd to the existing session directory component.
#[must_use]
pub fn project_component(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Session output root, shared by the registry, search and sandbox policies.
#[must_use]
pub fn session_output_dir(temp_root: &Path, cwd: &Path, session_id: &str) -> PathBuf {
    temp_root
        .join(project_component(cwd))
        .join(session_id)
        .join("tasks")
}

/// Stable filename for a task. Callers validate ids before filesystem use.
#[must_use]
pub fn output_filename(task_id: &str) -> String {
    format!("{task_id}.output")
}

/// Stable path within a caller-owned output root.
#[must_use]
pub fn output_path(root: &Path, task_id: &str) -> PathBuf {
    root.join(output_filename(task_id))
}

/// Compatibility root for unbound shell runners; registry-bound tasks never use it.
#[must_use]
pub fn legacy_output_dir() -> PathBuf {
    std::env::temp_dir().join("lingxi-task-output")
}

/// Compatibility output path shared by unbound runners and their callers.
#[must_use]
pub fn legacy_output_path(task_id: &str) -> PathBuf {
    legacy_output_dir().join(format!("{task_id}.out"))
}

/// 2.1.263 `gMe`: only a direct output child and a 1..20 ASCII id qualify.
/// This is a display parser, not the stricter registry id validator.
#[must_use]
pub fn output_id<'a>(root: &Path, path: &'a Path) -> Option<&'a str> {
    let root = root.to_str()?.trim_end_matches(std::path::MAIN_SEPARATOR);
    let filename = path
        .to_str()?
        .strip_prefix(root)?
        .strip_prefix(std::path::MAIN_SEPARATOR)?;
    let id = filename.strip_suffix(".output")?;
    (!id.is_empty()
        && id.len() <= 20
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'))
    .then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_layout_and_direct_child_parser() {
        let root = session_output_dir(
            Path::new("/tmp/user"),
            Path::new("/work/my project"),
            "session",
        );
        assert_eq!(root, Path::new("/tmp/user/-work-my-project/session/tasks"));
        let path = output_path(&root, "agent_1");
        assert_eq!(output_id(&root, &path), Some("agent_1"));
        for invalid in [
            ".output",
            "../agent.output",
            "nested/agent.output",
            "./agent.output",
            "//agent.output",
            "a.b.output",
            "abcdefghijklmnopqrstu.output",
            "中.output",
            "agent.out",
        ] {
            assert_eq!(output_id(&root, &root.join(invalid)), None, "{invalid}");
        }
        assert_eq!(
            output_id(&root, Path::new("/different/agent_1.output")),
            None
        );
    }
}

fn glob_literal(path: &str) -> String {
    let mut escaped = String::new();
    for c in path.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '{' | '}' | '!' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// Search exclusions for session output directories (2.1.263 `jSn`).
/// A direct task output file remains searchable; traversing its directory does not.
#[must_use]
pub fn search_exclusions(output_dir: &Path, search_root: &Path) -> Vec<String> {
    let Some(session_dir) = output_dir.parent() else {
        return vec![];
    };
    let Some(project_dir) = session_dir.parent() else {
        return vec![];
    };
    let Some(temp_root) = project_dir.parent() else {
        return vec![];
    };
    let Some(session) = session_dir.file_name().and_then(|s| s.to_str()) else {
        return vec![];
    };
    // The mobile layout is an accepted separate root, not a desktop temp tree.
    let mobile = output_dir.file_name().and_then(|s| s.to_str()) != Some("tasks");
    let roots = [
        Some(output_dir.to_path_buf()),
        std::fs::canonicalize(output_dir).ok(),
    ];
    if roots
        .into_iter()
        .flatten()
        .any(|root| search_root.starts_with(&root))
    {
        return vec!["!**".into()];
    }
    let mut result = vec![];
    if mobile {
        if let Ok(relative) = output_dir.strip_prefix(search_root) {
            result.push(format!(
                "!/{}/**",
                glob_literal(&relative.to_string_lossy().replace('\\', "/"))
            ));
        }
        return result;
    }
    let mut sessions = vec![
        "????????-????-????-????-????????????".to_owned(),
        "session_*".into(),
        "cse_*".into(),
    ];
    if session
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        sessions.push(session.into());
    }
    let roots = [
        Some(temp_root.to_path_buf()),
        std::fs::canonicalize(temp_root).ok(),
    ];
    for root in roots.into_iter().flatten() {
        if let Ok(relative) = root.strip_prefix(search_root) {
            let prefix = glob_literal(&relative.to_string_lossy().replace('\\', "/"));
            let prefix = if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            };
            for session in &sessions {
                result.push(format!("!/{prefix}*/{session}/tasks/**"));
            }
        } else if let Ok(relative) = search_root.strip_prefix(&root) {
            let parts: Vec<_> = relative.components().collect();
            if parts.len() == 1 {
                for session in &sessions {
                    result.push(format!("!/{session}/tasks/**"));
                }
            } else if parts.len() >= 2 {
                let candidate = parts[1].as_os_str().to_string_lossy();
                let recognized = candidate == session
                    || uuid::Uuid::parse_str(&candidate).is_ok()
                    || candidate.starts_with("session_")
                    || candidate.starts_with("cse_");
                if recognized {
                    if parts.len() == 2 {
                        result.push("!/tasks/**".into());
                    } else if parts[2].as_os_str() == "tasks" {
                        return vec!["!**".into()];
                    }
                }
            }
        }
    }
    result.sort();
    result.dedup();
    result
}

#[cfg(test)]
mod search_tests {
    use super::*;
    #[test]
    fn exclusions_follow_session_tree_without_hiding_ordinary_tasks_dirs() {
        let root = Path::new("/tmp/claude-1/project/session_current/tasks");
        assert!(search_exclusions(root, Path::new("/tmp"))
            .iter()
            .any(|p| p == "!/claude-1/*/session_current/tasks/**"));
        assert!(search_exclusions(root, Path::new("/tmp/claude-1/project"))
            .iter()
            .any(|p| p == "!/session_current/tasks/**"));
        assert_eq!(search_exclusions(root, root), vec!["!**"]);
        assert_eq!(
            search_exclusions(
                root,
                Path::new("/tmp/claude-1/project/00000000-0000-0000-0000-000000000000/tasks")
            ),
            vec!["!**"]
        );
        assert!(search_exclusions(root, Path::new("/work/tasks")).is_empty());
    }
}

/// Incremental UTF-8 decoding for one stdout or stderr stream. Keep separate
/// instances for the two streams and apply stderr framing after decoding.
#[derive(Debug, Default, Clone)]
pub struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    /// Decode a stream chunk, retaining incomplete code points until the next
    /// chunk. Set `eof` only when that stream ends; its incomplete tail becomes
    /// a replacement character then, never during foreground/background handoff.
    pub fn decode(&mut self, chunk: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(chunk);
        let mut output = String::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            match std::str::from_utf8(&self.pending[offset..]) {
                Ok(text) => {
                    output.push_str(text);
                    offset = self.pending.len();
                }
                Err(error) => {
                    let valid_end = offset + error.valid_up_to();
                    output.push_str(&String::from_utf8_lossy(&self.pending[offset..valid_end]));
                    offset = valid_end;
                    match error.error_len() {
                        Some(length) => {
                            output.push('\u{fffd}');
                            offset += length;
                        }
                        None if eof => {
                            output.push('\u{fffd}');
                            offset = self.pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..offset);
        output
    }
}

/// N7e/VAe: completed Bash output copies retain at most 64 MiB. This is
/// independent of the live task writer's 5 Gi UTF-16-unit cap.
pub const MAX_PERSISTED_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
