//! `bash_output_audience_note` — the one-line reminder appended after a Bash
//! result whose stdout is too long for the user's terminal to have shown.
//!
//! New in Claude Code 2.1.238. Oracle anatomy (offsets into
//! `~/.local/share/claude/versions/2.1.238`):
//!
//! * renderer @ **296736428**:
//!   ```js
//!   bash_output_audience_note:()=>Zy([kn({content:"Only you see that command's output — the user's terminal shows at most a few lines of it. If the user needs to read any of it, put it in your reply.",isMeta:!0})]),
//!   ```
//!   `Zy` maps `NT` (`<system-reminder>\n${e}\n</system-reminder>`) over the
//!   message, so the envelope is applied at the injection site, not here.
//! * gate `kpm(toolName, data, model)` @ **294267076**:
//!   ```js
//!   if(e!==Oi||typeof t!=="object"||t===null||!("stdout"in t)||typeof t.stdout!=="string"||Dn()||!CY(t.stdout))return!1;
//!   return IoT(Fo(r))
//!   ```
//!   — the tool must be Bash (`Oi`), the structured result must carry a string
//!   `stdout`, the session must be INTERACTIVE (`Dn()` @281039748 is
//!   `!isInteractive()`), the stdout must be "long" per `CY`, and the model
//!   must advertise the `bash_output_audience_note` capability
//!   (`IoT = JJr("bash_output_audience_note", V.CLAUDE_CODE_BASH_OUTPUT_AUDIENCE_NOTE, model)`).
//! * emission @ **294300924**, inside the post-tool-use dispatch tail:
//!   `if(kpm(e.name,re.data,VM(n))) S.push({message:gc({type:"bash_output_audience_note",toolUseID:t})})`
//!   — i.e. an attachment message that follows the `tool_result` line, not a
//!   transient per-turn reminder.
//!
//! `JJr` @ **294266?** returns the env value when it is defined, else the model
//! capability, else `false`. LingXi has no model-capability table, so
//! [`is_enabled`] is OFF unless `CLAUDE_CODE_BASH_OUTPUT_AUDIENCE_NOTE` is set.

/// The reminder body, byte-exact against @296736428 (U+2014 EM DASH).
pub const BASH_OUTPUT_AUDIENCE_NOTE: &str = "Only you see that command's output \u{2014} the user's terminal shows at most a few lines of it. If the user needs to read any of it, put it in your reply.";

/// `mNt = 3` @287626376 — the number of stdout lines the user's terminal is
/// assumed to show.
pub const TERMINAL_PREVIEW_LINES: usize = 3;

/// `Oi` — the tool this note is attached to.
pub const BASH_TOOL_NAME: &str = "Bash";

/// `CY(stdout)` @ **287626376**, called with the second argument omitted.
///
/// ```js
/// let r=e.trimEnd(),n=0,o=0;
/// for(let c=0;c<=mNt;c++){if(n=r.indexOf("\n",n),n===-1)break;o++,n++}
/// if(n!==-1&&n<r.length)return!0;
/// if(t===void 0)return!1;
/// ```
///
/// With no viewport width the whole wrapped-width branch is dead, so the
/// predicate reduces to "more than `TERMINAL_PREVIEW_LINES + 1` lines survive
/// after trimming trailing whitespace". Implemented as the literal loop so the
/// off-by-one stays visible.
///
/// (`String.prototype.trimEnd` also strips U+FEFF, which Rust's `trim_end`
/// does not; no realistic stdout is affected.)
#[must_use]
pub fn is_long_output(stdout: &str) -> bool {
    let trimmed = stdout.trim_end();
    let bytes = trimmed.as_bytes();
    let mut n: Option<usize> = Some(0);
    for _ in 0..=TERMINAL_PREVIEW_LINES {
        let from = n.expect("loop breaks on None");
        match bytes[from..].iter().position(|b| *b == b'\n') {
            Some(rel) => n = Some(from + rel + 1),
            None => {
                n = None;
                break;
            }
        }
    }
    matches!(n, Some(idx) if idx < bytes.len())
}

/// The `JJr("bash_output_audience_note", …)` arm reachable from the port.
#[must_use]
pub fn is_enabled() -> bool {
    let raw = std::env::var("CLAUDE_CODE_BASH_OUTPUT_AUDIENCE_NOTE").ok();
    lingxi_core::host::env::is_env_truthy(raw.as_deref())
}

/// `kpm(toolName, data, model)` — should the note follow this tool result?
///
/// `data` is the tool's structured `toolUseResult` payload (the orchestrator's
/// `tool_use_results` entry); `interactive` is `!Dn()`.
#[must_use]
pub fn should_attach(tool_name: &str, data: Option<&serde_json::Value>, interactive: bool) -> bool {
    if tool_name != BASH_TOOL_NAME || !interactive {
        return false;
    }
    let Some(stdout) = data.and_then(|d| d.get("stdout")).and_then(|v| v.as_str()) else {
        return false;
    };
    is_long_output(stdout) && is_enabled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn note_is_byte_exact_against_2_1_238() {
        assert_eq!(
            BASH_OUTPUT_AUDIENCE_NOTE,
            "Only you see that command's output — the user's terminal shows at most a few lines of it. If the user needs to read any of it, put it in your reply."
        );
        assert!(BASH_OUTPUT_AUDIENCE_NOTE.contains('\u{2014}'));
    }

    #[test]
    fn four_newlines_with_a_trailing_line_is_long() {
        assert!(is_long_output("a\nb\nc\nd\ne"));
    }

    #[test]
    fn four_lines_is_not_long() {
        assert!(!is_long_output("a\nb\nc\nd"));
    }

    /// `trimEnd` runs first, so trailing blank lines never push a short output
    /// over the threshold.
    #[test]
    fn trailing_whitespace_is_trimmed_before_counting() {
        assert!(!is_long_output("a\nb\nc\nd\n\n\n\n"));
        assert!(!is_long_output(""));
        assert!(!is_long_output("\n\n\n\n\n"));
    }

    #[test]
    fn only_bash_with_a_string_stdout_qualifies() {
        let long = json!({ "stdout": "1\n2\n3\n4\n5" });
        // Env gate is OFF by default, so even a qualifying result is skipped.
        assert!(!should_attach("Bash", Some(&long), true));
        // Wrong tool / non-interactive / missing stdout short-circuit earlier.
        assert!(!should_attach("Read", Some(&long), true));
        assert!(!should_attach("Bash", Some(&long), false));
        assert!(!should_attach("Bash", Some(&json!({ "stdout": 5 })), true));
        assert!(!should_attach("Bash", None, true));
    }
}
