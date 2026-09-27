//! Detect the terminal environment to choose a `SwarmBackend`.
//!
//! Source: Claude Code 2.1.263, src_160738762.js and src_172372449.js.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Snapshot of the host terminal at startup.
#[allow(clippy::struct_excessive_bools)] // each flag answers an independent probe
#[derive(Debug, Clone)]
pub struct TerminalEnv {
    /// `$TMUX` env var is set (we're inside a tmux session).
    pub inside_tmux: bool,
    /// iTerm2 terminal program, session ID, or terminal hint is present.
    pub iterm_app: bool,
    /// `tmux -V` succeeds.
    pub tmux_available: bool,
    /// `it2` CLI is reachable.
    pub it2_available: bool,
}

/// Which concrete backend to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    /// Real `tmux` backend (inside or outside a user session).
    Tmux,
    /// iTerm2 via `it2`.
    ITerm,
    /// No-pane fallback.
    InProcess,
}

/// Probe terminal state and backend availability at session construction.
#[must_use]
pub fn detect_terminal_env() -> TerminalEnv {
    let inside_tmux = std::env::var("TMUX").is_ok_and(|value| !value.is_empty());
    let iterm_app = is_inside_iterm();

    let tmux_available = tmux_available();
    let it2_available = iterm_app && probe_it2();

    TerminalEnv {
        inside_tmux,
        iterm_app,
        tmux_available,
        it2_available,
    }
}

/// Latest upstream checks the command exit status, not a minimum version.
#[must_use]
pub fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Recognize native iTerm2 even when TERM_PROGRAM was not forwarded.
#[must_use]
pub fn is_inside_iterm() -> bool {
    std::env::var("TERM_PROGRAM").as_deref() == Ok("iTerm.app")
        || std::env::var("ITERM_SESSION_ID").is_ok_and(|value| !value.is_empty())
        || std::env::var("terminal").as_deref() == Ok("iTerm.app")
}

static IT2_COMMAND: OnceLock<String> = OnceLock::new();

pub(crate) fn it2_command() -> &'static str {
    IT2_COMMAND.get().map_or("it2", String::as_str)
}

/// Probe API connectivity, resolving login-shell PATH with the upstream
/// two-second limit. A reachable executable alone does not enable iTerm2.
pub(crate) fn probe_it2() -> bool {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let resolved = Command::new(shell)
        .args(["-lc", "command -v it2"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()
        .and_then(|mut child| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return child.wait_with_output().ok(),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                }
            }
        })
        .filter(|output| output.status.success())
        .and_then(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .next_back()
                .map(str::to_owned)
        });
    let mut command = resolved.as_deref().unwrap_or("it2");
    let mut result = Command::new(command).args(["session", "list"]).output();
    if resolved.is_some()
        && (result
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            || result
                .as_ref()
                .is_ok_and(|output| output.status.code() == Some(127)))
    {
        command = "it2";
        result = Command::new(command).args(["session", "list"]).output();
    }
    if result.is_ok_and(|output| output.status.success()) {
        let _ = IT2_COMMAND.set(command.to_owned());
        return true;
    }
    false
}

/// User-selected teammate execution mode (Claude Code 2.1.263).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TeammateMode {
    /// Use panes only when already in tmux or iTerm2.
    #[default]
    Auto,
    /// Always run within the host process.
    InProcess,
    /// Require an available pane backend.
    Tmux,
    /// Require native iTerm2 and its it2 CLI.
    ITerm2,
}

/// Select execution mode before constructing a pane backend. Noninteractive
/// sessions always use in-process execution, including explicit pane modes.
/// Source: 2.1.263 src_172372449.js, l4e / Ift.
pub fn select_backend(
    env: &TerminalEnv,
    mode: TeammateMode,
    interactive: bool,
    prefer_tmux: bool,
) -> Result<BackendChoice, &'static str> {
    if !interactive || mode == TeammateMode::InProcess {
        return Ok(BackendChoice::InProcess);
    }
    if mode == TeammateMode::Auto && !env.inside_tmux && !env.iterm_app {
        return Ok(BackendChoice::InProcess);
    }
    let detected = detect_pane_backend(env, mode, prefer_tmux);
    if mode == TeammateMode::Auto {
        return Ok(detected.unwrap_or(BackendChoice::InProcess));
    }
    detected
}

fn detect_pane_backend(
    env: &TerminalEnv,
    mode: TeammateMode,
    prefer_tmux: bool,
) -> Result<BackendChoice, &'static str> {
    if mode == TeammateMode::ITerm2 {
        if !env.iterm_app {
            return Err("teammateMode is set to \"iterm2\" but this session is not running inside iTerm2. Launch LingXi from iTerm2, or change teammateMode in settings.");
        }
        if !env.it2_available {
            return Err("teammateMode is set to \"iterm2\" but the it2 CLI is not reachable. Install it with `pip install it2` and enable the Python API in iTerm2 (Preferences > General > Magic > Enable Python API).");
        }
        return Ok(BackendChoice::ITerm);
    }
    if env.inside_tmux {
        return Ok(BackendChoice::Tmux);
    }
    if env.iterm_app {
        if !prefer_tmux && env.it2_available {
            return Ok(BackendChoice::ITerm);
        }
        if env.tmux_available {
            return Ok(BackendChoice::Tmux);
        }
        return Err("iTerm2 detected but it2 CLI not installed. Install it2 with: pip install it2");
    }
    if env.tmux_available {
        return Ok(BackendChoice::Tmux);
    }
    Err(NO_PANE_BACKEND)
}

#[cfg(target_os = "macos")]
const NO_PANE_BACKEND: &str = "To use agent swarms, install tmux:\n  brew install tmux\nThen start a tmux session with: tmux new-session -s lingxi";
#[cfg(not(target_os = "macos"))]
const NO_PANE_BACKEND: &str = "To use agent swarms, install tmux:\n  sudo apt install tmux    # Ubuntu/Debian\n  sudo dnf install tmux    # Fedora/RHEL\nThen start a tmux session with: tmux new-session -s lingxi";

/// Select the default interactive execution mode.
#[must_use]
pub fn pick_backend(env: &TerminalEnv) -> BackendChoice {
    select_backend(env, TeammateMode::Auto, true, false)
        .expect("auto mode always supports in-process execution")
}
