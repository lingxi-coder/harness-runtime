//! Verifies the color map literals match claude-code `TmuxBackend.ts:60-69` and
//! that the argv constructed by `TmuxBackend` matches what would be passed to
//! `tmux` on the shell.

use platform_posix::swarm::tmux::{
    agent_color_to_tmux, build_select_pane_color_argv, build_send_keys_argv,
    build_set_pane_border_argv, build_set_pane_border_format_argv, build_split_window_argv,
    AgentColor, SwarmConstants,
};

#[test]
fn color_map_matches_claude_code() {
    assert_eq!(agent_color_to_tmux(AgentColor::Red), "red");
    assert_eq!(agent_color_to_tmux(AgentColor::Blue), "blue");
    assert_eq!(agent_color_to_tmux(AgentColor::Green), "green");
    assert_eq!(agent_color_to_tmux(AgentColor::Yellow), "yellow");
    assert_eq!(agent_color_to_tmux(AgentColor::Cyan), "cyan");
    assert_eq!(agent_color_to_tmux(AgentColor::Purple), "magenta");
    assert_eq!(agent_color_to_tmux(AgentColor::Orange), "colour208");
    assert_eq!(agent_color_to_tmux(AgentColor::Pink), "colour205");
}

#[test]
fn swarm_constants_match_claude_code() {
    assert_eq!(SwarmConstants::SESSION_NAME, "claude-swarm");
    assert_eq!(SwarmConstants::VIEW_WINDOW_NAME, "swarm-view");
    assert_eq!(SwarmConstants::TMUX_COMMAND, "tmux");
    assert!(SwarmConstants::socket_name_for_pid(1234).contains("claude-swarm-1234"));
    // Current-pid socket name uses the live pid.
    assert!(SwarmConstants::current_socket_name().starts_with("claude-swarm-"));
}

#[test]
fn split_window_argv_inside_tmux_horizontal() {
    // Splitting horizontally with a 70% size, returning the new pane id.
    let argv = build_split_window_argv("%0", true, Some("70%"));
    assert_eq!(
        argv,
        vec![
            "split-window".to_string(),
            "-t".to_string(),
            "%0".to_string(),
            "-h".to_string(),
            "-l".to_string(),
            "70%".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
        ]
    );
}

#[test]
fn split_window_argv_vertical_no_size() {
    // Vertical (`-v`), no `-l` size — used by claude-code when caller omits sizing.
    let argv = build_split_window_argv("%3", false, None);
    assert_eq!(
        argv,
        vec![
            "split-window".to_string(),
            "-t".to_string(),
            "%3".to_string(),
            "-v".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
        ]
    );
}

#[test]
fn select_pane_color_argv_uses_dash_capital_p() {
    let argv = build_select_pane_color_argv("%1", "magenta");
    assert_eq!(
        argv,
        vec![
            "select-pane".to_string(),
            "-t".to_string(),
            "%1".to_string(),
            "-P".to_string(),
            "bg=default,fg=magenta".to_string(),
        ]
    );
}

#[test]
fn set_pane_border_argv_uses_lower_p() {
    let argv = build_set_pane_border_argv("%2", "colour208");
    assert_eq!(
        argv,
        vec![
            "set-option".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            "%2".to_string(),
            "pane-border-style".to_string(),
            "fg=colour208".to_string(),
        ]
    );
}

#[test]
fn set_pane_border_format_argv_shape() {
    let argv = build_set_pane_border_format_argv("%5", "#{pane_index} agent-1");
    assert_eq!(
        argv,
        vec![
            "set-option".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            "%5".to_string(),
            "pane-border-format".to_string(),
            "#{pane_index} agent-1".to_string(),
        ]
    );
}

#[test]
fn send_keys_argv_appends_enter() {
    let argv = build_send_keys_argv("%6", "echo hello");
    assert_eq!(
        argv,
        vec![
            "send-keys".to_string(),
            "-t".to_string(),
            "%6".to_string(),
            "echo hello".to_string(),
            "Enter".to_string(),
        ]
    );
}
