//! Marketplace `source:"command"` installer (oracle plugin command source).
//!
//! The catalog entry's `command` is a shell snippet that must print exactly
//! one absolute plugin directory. `mode:"link"` uses that directory in place;
//! omitted/`copy` copies it into the cache. The command is not run until the
//! user has consented to that exact command (and link vs copy).

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::marketplace::{MarketplaceCommandMode, MarketplaceExternalSource};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_STDOUT_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const STDERR_MESSAGE_CHARS: usize = 500;
const LINK_MODE_WINDOWS_ERROR: &str =
    "This plugin source uses mode \"link\", which is not supported on Windows yet; the marketplace can use mode \"copy\" instead.";

/// Install a `source:"command"` plugin into `dest`.
///
/// `consented` must be true for the current command/mode; otherwise the
/// command is not started (oracle `ht`).
pub fn materialize_command_plugin_source(
    source: &MarketplaceExternalSource,
    dest: &Path,
    consented: bool,
) -> Result<PathBuf, String> {
    let MarketplaceExternalSource::Command {
        command,
        timeout,
        mode,
    } = source
    else {
        return Err("plugin source is not a command source".to_string());
    };
    if !consented {
        let shown = command_display(command, *mode);
        return Err(format!(
            "This plugin is installed by running a command on this machine (`{shown}`) that has not been reviewed yet, so it was not run."
        ));
    }
    if matches!(mode, Some(MarketplaceCommandMode::Link)) && cfg!(windows) {
        return Err(LINK_MODE_WINDOWS_ERROR.to_string());
    }

    let producer = run_command_source(command, timeout.unwrap_or(DEFAULT_TIMEOUT_SECS))?;
    if dest.exists() {
        let _ = std::fs::remove_dir_all(dest);
    }
    std::fs::create_dir_all(dest).map_err(|e| format!("failed to create plugin cache dir: {e}"))?;
    match mode {
        Some(MarketplaceCommandMode::Link) => link_plugin_directory(&producer, dest)?,
        Some(MarketplaceCommandMode::Copy) | None => copy_plugin_directory(&producer, dest)?,
    }
    Ok(dest.to_path_buf())
}

fn command_display(command: &str, mode: Option<MarketplaceCommandMode>) -> String {
    let truncated: String = command.chars().take(200).collect();
    if matches!(mode, Some(MarketplaceCommandMode::Link)) {
        format!("{truncated} [mode: link]")
    } else {
        truncated
    }
}

fn run_command_source(command: &str, timeout_secs: u64) -> Result<PathBuf, String> {
    let shown: String = command.chars().take(200).collect();
    tracing::info!("Plugin command source: running `{shown}` (timeout {timeout_secs}s)");

    let mut child = plugin_source_command(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Plugin source command `{shown}` could not be started: {e}"))?;

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_buf = thread::spawn(move || read_capped(&mut stdout, MAX_STDOUT_BYTES));
    let stderr_buf = thread::spawn(move || read_capped(&mut stderr, MAX_STDERR_BYTES));

    let timeout = Duration::from_secs(timeout_secs);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Plugin source command `{shown}` did not finish within {timeout_secs}s and was stopped."
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                return Err(format!(
                    "Plugin source command `{shown}` could not be started: {e}"
                ));
            }
        }
    };

    let (stdout, stdout_overflow) = stdout_buf
        .join()
        .unwrap_or_else(|_| Ok((Vec::new(), false)))
        .unwrap_or_else(|_| (Vec::new(), false));
    let (stderr, _) = stderr_buf
        .join()
        .unwrap_or_else(|_| Ok((Vec::new(), false)))
        .unwrap_or_else(|_| (Vec::new(), false));
    if stdout_overflow {
        return Err(format!(
            "Plugin source command `{shown}` printed more than {} KB and was stopped; it must print a single absolute path.",
            MAX_STDOUT_BYTES / 1024
        ));
    }
    let stderr_text = String::from_utf8_lossy(&stderr);
    let stderr_note = truncate_chars(stderr_text.trim(), STDERR_MESSAGE_CHARS);
    let stderr_suffix = if stderr_note.is_empty() {
        String::new()
    } else {
        format!(": {stderr_note}")
    };

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(format!(
            "Plugin source command `{shown}` exited with code {code}{stderr_suffix}"
        ));
    }

    let lines: Vec<&str> = std::str::from_utf8(&stdout)
        .map_err(|_| {
            format!("Plugin source command `{shown}` printed nothing; it must print the absolute path of the plugin directory.")
        })?
        .split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return Err(format!(
            "Plugin source command `{shown}` printed nothing; it must print the absolute path of the plugin directory."
        ));
    }
    if lines.len() > 1 {
        return Err(format!(
            "Plugin source command `{shown}` printed {} lines; it must print exactly one absolute path.",
            lines.len()
        ));
    }
    let printed = lines[0];
    if !Path::new(printed).is_absolute() {
        let shown_path: String = printed.chars().take(200).collect();
        return Err(format!(
            "Plugin source command `{shown}` printed `{shown_path}`, which is not an absolute path."
        ));
    }
    if is_network_path(printed) {
        let shown_path: String = printed.chars().take(200).collect();
        return Err(format!(
            "Plugin source command `{shown}` printed `{shown_path}`, a network path (UNC or automount), which is not supported as a plugin directory."
        ));
    }
    let resolved = std::fs::canonicalize(printed).map_err(|e| {
        let shown_path: String = printed.chars().take(200).collect();
        format!(
            "Plugin source command `{shown}` printed `{shown_path}`, but that path could not be resolved ({e})."
        )
    })?;
    if is_network_path(&resolved.to_string_lossy()) {
        return Err(format!(
            "Plugin source command `{shown}` printed a path that resolves to a network location, which is not supported as a plugin directory."
        ));
    }
    let entries = std::fs::read_dir(&resolved).map_err(|e| {
        let shown_path: String = printed.chars().take(200).collect();
        if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) {
            format!(
                "Plugin source command `{shown}` printed `{shown_path}`, which is not a directory."
            )
        } else {
            format!(
                "Plugin source command `{shown}` printed `{shown_path}`, which could not be read as a directory ({e})."
            )
        }
    })?;
    let has_plugin_content = entries
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_str().is_some_and(is_plugin_content_entry));
    if !has_plugin_content {
        let shown_path: String = printed.chars().take(200).collect();
        return Err(format!(
            "Plugin source command `{shown}` printed `{shown_path}`, but that directory has no plugin content (expected {}/ or a commands/, skills/, agents/, hooks/, themes/, output-styles/, monitors/, workflows/, SKILL.md, .mcp.json, or .lsp.json at the top level). Nothing was installed.",
            branding::PLUGIN_MANIFEST_DIR
        ));
    }
    tracing::info!(
        "Plugin command source: resolved plugin directory {}",
        resolved.display()
    );
    Ok(resolved)
}

fn plugin_source_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

fn read_capped(stream: &mut Option<impl Read>, cap: usize) -> io::Result<(Vec<u8>, bool)> {
    let Some(stream) = stream else {
        return Ok((Vec::new(), false));
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok((buf, false)),
            Ok(n) => {
                if buf.len() + n > cap {
                    buf.extend_from_slice(&chunk[..cap.saturating_sub(buf.len())]);
                    return Ok((buf, true));
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn is_network_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.starts_with("//")
        || normalized.starts_with("/net/")
        || normalized.starts_with("/Network/Servers/")
}

fn is_plugin_content_entry(name: &str) -> bool {
    matches!(
        name,
        "commands"
            | "skills"
            | "agents"
            | "hooks"
            | "themes"
            | "output-styles"
            | "monitors"
            | "workflows"
            | "SKILL.md"
            | ".mcp.json"
            | ".lsp.json"
    ) || name == branding::PLUGIN_MANIFEST_DIR
}

fn copy_plugin_directory(src: &Path, dest: &Path) -> Result<(), String> {
    copy_tree(src, dest, true).map_err(|e| format!("failed to copy plugin directory: {e}"))
}

fn copy_tree(src: &Path, dest: &Path, skip_root_git: bool) -> io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip_root_git && name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dest.join(&name);
        let ft = entry.file_type()?;
        if ft.is_dir() {
            copy_tree(&from, &to, false)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn link_plugin_directory(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("failed to create link farm: {e}"))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| format!("failed to read command output directory: {e}"))?
    {
        let entry = entry.map_err(|e| format!("failed to read command output directory: {e}"))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = std::fs::canonicalize(entry.path())
            .map_err(|e| format!("A top-level entry of the plugin directory its command produced could not be resolved ({e}); refusing to link it."))?;
        let to = dest.join(&name);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&from, &to).map_err(|e| {
                format!(
                    "failed to link plugin entry {}: {e}",
                    name.to_string_lossy()
                )
            })?;
        }
        #[cfg(not(unix))]
        {
            let _ = (from, to);
            return Err(LINK_MODE_WINDOWS_ERROR.to_string());
        }
    }
    let marker = dest.join(".lingxi-plugin-link");
    std::fs::write(&marker, serde_json::json!({ "target": src }).to_string())
        .map_err(|e| format!("failed to write link-farm marker: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_plugin(dir: &Path, name: &str) {
        fs::create_dir_all(dir.join(".lingxi-plugin")).unwrap();
        fs::write(
            dir.join(".lingxi-plugin").join("plugin.json"),
            format!(r#"{{"name":"{name}","version":"1.0.0"}}"#),
        )
        .unwrap();
    }

    #[test]
    fn refuses_to_run_without_consent() {
        let tmp = tempfile::tempdir().unwrap();
        let source = MarketplaceExternalSource::Command {
            command: "echo /tmp/plugin".into(),
            timeout: Some(5),
            mode: None,
        };
        let err = materialize_command_plugin_source(&source, &tmp.path().join("out"), false)
            .expect_err("must not run");
        assert!(err.contains("has not been reviewed yet"), "got: {err}");
        assert!(!tmp.path().join("out").exists());
    }

    #[test]
    fn copies_command_output_directory_when_consented() {
        let tmp = tempfile::tempdir().unwrap();
        let producer = tmp.path().join("produced");
        write_plugin(&producer, "from-cmd");
        let command = format!("printf '%s\\n' '{}'", producer.display());
        let source = MarketplaceExternalSource::Command {
            command,
            timeout: Some(5),
            mode: None,
        };
        let dest = tmp.path().join("cache");
        let out = materialize_command_plugin_source(&source, &dest, true).expect("copy");
        assert_eq!(out, dest);
        assert!(dest.join(".lingxi-plugin").join("plugin.json").is_file());
        assert!(producer
            .join(".lingxi-plugin")
            .join("plugin.json")
            .is_file());
    }

    #[cfg(unix)]
    #[test]
    fn link_mode_farms_top_level_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let producer = tmp.path().join("produced");
        write_plugin(&producer, "linked");
        fs::create_dir_all(producer.join("commands")).unwrap();
        fs::write(producer.join("commands").join("hi.md"), "hi").unwrap();
        let command = format!("printf '%s\\n' '{}'", producer.display());
        let source = MarketplaceExternalSource::Command {
            command,
            timeout: Some(5),
            mode: Some(MarketplaceCommandMode::Link),
        };
        let dest = tmp.path().join("farm");
        materialize_command_plugin_source(&source, &dest, true).expect("link");
        assert!(dest.join(".lingxi-plugin-link").is_file());
        assert!(dest.join(".lingxi-plugin").is_symlink());
        assert!(dest.join("commands").is_symlink());
    }

    #[test]
    fn relative_stdout_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let source = MarketplaceExternalSource::Command {
            command: "printf '%s\\n' relative/plugin".into(),
            timeout: Some(5),
            mode: None,
        };
        let err = materialize_command_plugin_source(&source, &tmp.path().join("out"), true)
            .expect_err("relative");
        assert!(err.contains("not an absolute path"), "got: {err}");
    }
}
