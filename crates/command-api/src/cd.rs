//! Directory-move notice text verified against Claude Code 2.1.307's `WAt`.
//!
//! UI notices use the plain body; model notices use its system-reminder wrapper.
//! The runtime appends any gated-project notice and loaded memory separately.

/// Confirmation copy used by the interactive directory-change view.
pub const CONFIRM_PROMPT: &str = env!("HARNESS_CD_CONFIRM_PROMPT");

/// Native analytics name and the semantic source before analytics hashing.
pub const TELEMETRY_EVENT: &str = "tengu_cd_command";
/// Native source value for a slash-command move.
pub const TELEMETRY_SOURCE_CD_COMMAND: &str = "cd_command";

/// Entry point that initiated the confirmed move.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MoveSource {
    /// The interactive `/cd` command.
    CdCommand,
    /// A private host control request.
    User,
}

/// State after the move's final freshness check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveMessageOptions {
    /// Initiating entry point, used in the model notice.
    pub source: MoveSource,
    /// Whether the session has dequeued a request before the move.
    pub request_built: bool,
    /// Whether no user or assistant history has been established yet.
    pub fresh_session: bool,
}

impl Default for MoveMessageOptions {
    fn default() -> Self {
        Self {
            source: MoveSource::CdCommand,
            request_built: true,
            fresh_session: false,
        }
    }
}

/// Plain notice body after an interactive `/cd` in a session with prior history.
#[must_use]
pub fn result_message(new_cwd: &str) -> String {
    move_result_message(new_cwd, MoveMessageOptions::default())
}

/// Plain notice body for a host-initiated move in a session with prior history.
#[must_use]
pub fn user_result_message(new_cwd: &str) -> String {
    move_result_message(
        new_cwd,
        MoveMessageOptions {
            source: MoveSource::User,
            ..MoveMessageOptions::default()
        },
    )
}

/// Native base body for the confirmed session state.
/// `new_cwd` must already have the caller's display escaping applied.
#[must_use]
pub fn move_result_message(new_cwd: &str, options: MoveMessageOptions) -> String {
    if options.fresh_session {
        return String::new();
    }
    let source = match options.source {
        MoveSource::CdCommand => "via /cd",
        MoveSource::User => "by the user",
    };
    let mut message =
        format!("The session's working directory has changed to {new_cwd} ({source}). ");
    if options.request_built {
        message.push_str("The environment block at the start of this conversation still names the previous directory — that information is stale. ");
    }
    message.push_str(&format!("All tool calls and relative paths now resolve from {new_cwd}. Project settings (permission rules, hooks), project MCP servers, and project skills now come from {new_cwd}; its {}, if any, follows below. Environment variables set by the previous directory's settings stay in effect for this process — they cannot be unset — and the new directory's settings env is applied on top of them.", branding::MEMORY_FILE));
    message
}

/// Model-visible base notice, before optional gated-project and memory blocks.
#[must_use]
pub fn model_result_message(new_cwd: &str, options: MoveMessageOptions) -> String {
    let body = move_result_message(new_cwd, options);
    if body.is_empty() {
        body
    } else {
        format!("<system-reminder>\n{body}\n</system-reminder>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Project exactly the source notice's memory filename; all other oracle
    // bytes, including fresh-session and model-wrapper behavior, stay locked.
    fn product_notice(native: &str) -> String {
        if native.is_empty() {
            return native.to_owned();
        }
        let (_, suffix) = native.split_once("; its ").expect("memory sentence");
        let (filename, _) = suffix
            .split_once(", if any, follows below.")
            .expect("memory filename");
        assert_eq!(filename.rsplit_once('.'), Some(("CLAUDE", "md")));
        let source = format!("; its {filename}, if any, follows below.");
        assert_eq!(native.matches(&source).count(), 1);
        native.replacen(
            &source,
            &format!("; its {}, if any, follows below.", branding::MEMORY_FILE),
            1,
        )
    }

    #[test]
    fn move_notice_bytes_follow_native_307_replay() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../session/tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap();
        assert_eq!(fixture["version"], "2.1.307");
        for case in fixture["cd_move_cases"].as_array().unwrap() {
            if case["expected"].get("result").is_none() {
                continue;
            }
            let input = &case["input"];
            let source = if input["source"] == "cd_command" {
                MoveSource::CdCommand
            } else {
                MoveSource::User
            };
            let options = MoveMessageOptions {
                source,
                request_built: input["request_built"].as_bool().unwrap_or(true),
                fresh_session: case["expected"]["result"]["freshSession"]
                    .as_bool()
                    .unwrap(),
            };
            let target = input["target_dir"].as_str().unwrap();
            assert_eq!(
                move_result_message(target, options),
                product_notice(case["expected"]["base_message"].as_str().unwrap()),
                "{}",
                case["name"]
            );
            if input["project_grants_gated"].as_bool() != Some(true)
                && input.get("memory_context").is_none()
            {
                assert_eq!(
                    model_result_message(target, options),
                    product_notice(case["expected"]["result"]["modelMessage"].as_str().unwrap()),
                    "{}",
                    case["name"]
                );
            }
        }
    }

    #[test]
    fn default_sources_match_their_native_move_cases() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../session/tests/fixtures/workspace_session_2_1_307.json"
        ))
        .unwrap();
        let cases = fixture["cd_move_cases"].as_array().unwrap();
        assert_eq!(
            result_message("/workspace/B"),
            product_notice(
                cases
                    .iter()
                    .find(|case| case["name"] == "cd_used_session")
                    .unwrap()["expected"]["base_message"]
                    .as_str()
                    .unwrap()
            )
        );
        assert_eq!(
            user_result_message("/workspace/B"),
            product_notice(
                cases
                    .iter()
                    .find(|case| case["name"] == "remote_used_session")
                    .unwrap()["expected"]["base_message"]
                    .as_str()
                    .unwrap()
            )
        );
    }

    #[test]
    fn confirmation_names_the_product_memory_file() {
        assert_eq!(CONFIRM_PROMPT, format!("? This moves the session's working directory and write access there, and loads project configuration ({}, settings) from that location.", branding::MEMORY_FILE));
    }
}
