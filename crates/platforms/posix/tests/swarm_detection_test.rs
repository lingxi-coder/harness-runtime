//! Verifies the detection logic picks the right backend given env probes.

use platform_posix::swarm::detection::{pick_backend, BackendChoice, TerminalEnv};

#[test]
fn inside_tmux_picks_tmux() {
    let env = TerminalEnv {
        inside_tmux: true,
        iterm_app: false,
        tmux_available: true,
        it2_available: true,
    };
    assert!(matches!(pick_backend(&env), BackendChoice::Tmux));
}

#[test]
fn iterm_with_it2_no_tmux_picks_iterm() {
    let env = TerminalEnv {
        inside_tmux: false,
        iterm_app: true,
        tmux_available: false,
        it2_available: true,
    };
    assert!(matches!(pick_backend(&env), BackendChoice::ITerm));
}

#[test]
fn outside_tmux_with_tmux_uses_inprocess() {
    let env = TerminalEnv {
        inside_tmux: false,
        iterm_app: false,
        tmux_available: true,
        it2_available: false,
    };
    assert!(matches!(pick_backend(&env), BackendChoice::InProcess));
}

#[test]
fn iterm_plus_tmux_prefers_iterm() {
    // claude-code registry.ts:173-200 — iTerm wins over external tmux when
    // it2/osascript is reachable.
    let env = TerminalEnv {
        inside_tmux: false,
        iterm_app: true,
        tmux_available: true,
        it2_available: true,
    };
    assert!(matches!(pick_backend(&env), BackendChoice::ITerm));
}

#[test]
fn nothing_available_picks_inprocess() {
    let env = TerminalEnv {
        inside_tmux: false,
        iterm_app: false,
        tmux_available: false,
        it2_available: false,
    };
    assert!(matches!(pick_backend(&env), BackendChoice::InProcess));
}

#[test]
fn explicit_modes_do_not_silently_fall_back() {
    use platform_posix::swarm::detection::{select_backend, TeammateMode};
    let env = TerminalEnv {
        inside_tmux: false,
        iterm_app: false,
        tmux_available: false,
        it2_available: false,
    };
    assert!(select_backend(&env, TeammateMode::Tmux, true, false).is_err());
    assert_eq!(select_backend(&env, TeammateMode::ITerm2, true, false), Err("teammateMode is set to \"iterm2\" but this session is not running inside iTerm2. Launch LingXi from iTerm2, or change teammateMode in settings."));
    assert_eq!(
        select_backend(&env, TeammateMode::ITerm2, false, false),
        Ok(BackendChoice::InProcess)
    );
}

#[test]
fn auto_falls_back_when_iterm_cli_is_missing() {
    use platform_posix::swarm::detection::{select_backend, TeammateMode};
    let env = TerminalEnv {
        inside_tmux: false,
        iterm_app: true,
        tmux_available: false,
        it2_available: false,
    };
    assert_eq!(pick_backend(&env), BackendChoice::InProcess);
    assert_eq!(
        select_backend(&env, TeammateMode::Tmux, true, false),
        Err("iTerm2 detected but it2 CLI not installed. Install it2 with: pip install it2")
    );
    assert_eq!(select_backend(&env, TeammateMode::ITerm2, true, false), Err("teammateMode is set to \"iterm2\" but the it2 CLI is not reachable. Install it with `pip install it2` and enable the Python API in iTerm2 (Preferences > General > Magic > Enable Python API)."));
}

#[test]
fn explicit_iterm_overrides_tmux_and_preference() {
    use platform_posix::swarm::detection::{select_backend, TeammateMode};
    let env = TerminalEnv {
        inside_tmux: true,
        iterm_app: true,
        tmux_available: true,
        it2_available: true,
    };
    assert_eq!(
        select_backend(&env, TeammateMode::ITerm2, true, true),
        Ok(BackendChoice::ITerm)
    );
    assert_eq!(
        select_backend(&env, TeammateMode::Auto, true, false),
        Ok(BackendChoice::Tmux)
    );
    let outside = TerminalEnv {
        inside_tmux: false,
        ..env
    };
    assert_eq!(
        select_backend(&outside, TeammateMode::Auto, true, true),
        Ok(BackendChoice::Tmux)
    );
}
