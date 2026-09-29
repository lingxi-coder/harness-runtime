//! `/fusion` argument parser.
//!
//! Not a builtin name — the handler is registered at the desktop composition
//! root so [`crate::builtin_support::names::BUILTIN_COMMAND_NAMES`] stays
//! locked at 108.

use crate::parser::ParsedSlashCommand;
use platform_api::{
    normalize_dimensions, normalize_dimensions_for, validate_verify_commands, FusionModelRef,
    FusionOrigin, FusionPanelMode, FusionPreset, FusionRequest, FUSION_MAX_PANEL, FUSION_MIN_PANEL,
    FUSION_SCHEMA_VERSION,
};

/// Usage line for empty / invalid invocations.
pub const FUSION_SLASH_USAGE: &str = "Usage: /fusion [--quality|--fast] [--same-provider|--cross-provider] [--models profile:model,...] [--dimensions dim,...] [--partial-ok|--no-partial] [--max-panel N] [--implement [--verify \"COMMAND\"]...] PROMPT\n   or: /fusion setup\n   or: /fusion clean\n   or: /fusion --retry-publication fu_RUN_ID";

/// Parsed `/fusion` flags plus the remaining prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionSlashArgs {
    /// Explicit panel preset. `None` = use the runtime Fusion default.
    pub preset: Option<FusionPreset>,
    /// `None` = use `slashCrossProviderDefault`.
    pub cross_provider: Option<bool>,
    /// Explicit panel models.
    pub models: Option<Vec<FusionModelRef>>,
    /// Scoring dimensions.
    pub dimensions: Option<Vec<String>>,
    /// Continue when some panels fail.
    pub partial_ok: Option<bool>,
    /// Panel cap override.
    pub max_panel: Option<u8>,
    /// `--implement`: the panels change the code in their own worktrees
    /// instead of answering read-only.
    pub implement: bool,
    /// Verification commands typed with `--verify`, in order. They replace
    /// `fusion.implement.verifyCommands` for this run and only exist together
    /// with `--implement`.
    pub verify_commands: Vec<String>,
    /// Non-empty task prompt.
    pub prompt: String,
}

/// Accumulated `/fusion` flag values as the token loop parses them out
/// (mirrors [`FusionSlashArgs`] minus the trailing prompt). Collected into
/// one struct so the per-token step function stays under the argument-count
/// lint instead of taking six separate `&mut Option<_>`.
#[derive(Default)]
struct FusionFlagAccum {
    preset: Option<FusionPreset>,
    cross_provider: Option<bool>,
    partial_ok: Option<bool>,
    models: Option<Vec<FusionModelRef>>,
    dimensions: Option<Vec<String>>,
    max_panel: Option<u8>,
    implement: bool,
    verify_commands: Vec<String>,
}

/// Outcome of consuming one token in the `/fusion` flag loop.
enum FlagStep {
    /// A flag (and, for `--models value` style, its following token) was
    /// consumed; resume the loop at `next_i`/`next_cursor`.
    Consumed { next_i: usize, next_cursor: usize },
    /// The current token is not a recognised flag: positional args (the
    /// prompt) begin here.
    Stop,
}

/// Parse and apply the flag at `tokens[i]`, advancing `cursor` past its
/// matching span in `raw`. Same per-flag behavior as the original inline
/// match — this is a straight extraction, not a rewrite.
fn consume_fusion_flag(
    tokens: &[String],
    raw: &str,
    i: usize,
    cursor: usize,
    acc: &mut FusionFlagAccum,
) -> Result<FlagStep, String> {
    let tok = tokens[i].as_str();
    let consumed_one = |cursor: usize| FlagStep::Consumed {
        next_i: i + 1,
        next_cursor: advance_past_token(raw, cursor, tok),
    };
    match tok {
        "--quality" => {
            set_preset(&mut acc.preset, FusionPreset::Quality)?;
            Ok(consumed_one(cursor))
        }
        "--fast" => {
            set_preset(&mut acc.preset, FusionPreset::Fast)?;
            Ok(consumed_one(cursor))
        }
        "--same-provider" => {
            set_cross(&mut acc.cross_provider, false)?;
            Ok(consumed_one(cursor))
        }
        "--cross-provider" => {
            set_cross(&mut acc.cross_provider, true)?;
            Ok(consumed_one(cursor))
        }
        "--partial-ok" => {
            set_once(&mut acc.partial_ok, true, "--partial-ok/--no-partial")?;
            Ok(consumed_one(cursor))
        }
        "--no-partial" => {
            set_once(&mut acc.partial_ok, false, "--partial-ok/--no-partial")?;
            Ok(consumed_one(cursor))
        }
        flag if flag.starts_with("--models=") => {
            set_once(
                &mut acc.models,
                parse_models(&flag["--models=".len()..])?,
                "--models",
            )?;
            Ok(consumed_one(cursor))
        }
        "--models" => {
            let next_cursor = advance_past_token(raw, cursor, tok);
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| "--models requires a value".to_string())?;
            set_once(&mut acc.models, parse_models(value)?, "--models")?;
            Ok(FlagStep::Consumed {
                next_i: i + 2,
                next_cursor: advance_past_token(raw, next_cursor, value),
            })
        }
        flag if flag.starts_with("--dimensions=") => {
            set_once(
                &mut acc.dimensions,
                parse_dimensions(&flag["--dimensions=".len()..])?,
                "--dimensions",
            )?;
            Ok(consumed_one(cursor))
        }
        "--dimensions" => {
            let next_cursor = advance_past_token(raw, cursor, tok);
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| "--dimensions requires a value".to_string())?;
            set_once(
                &mut acc.dimensions,
                parse_dimensions(value)?,
                "--dimensions",
            )?;
            Ok(FlagStep::Consumed {
                next_i: i + 2,
                next_cursor: advance_past_token(raw, next_cursor, value),
            })
        }
        flag if flag.starts_with("--max-panel=") => {
            set_once(
                &mut acc.max_panel,
                parse_max_panel(&flag["--max-panel=".len()..])?,
                "--max-panel",
            )?;
            Ok(consumed_one(cursor))
        }
        "--max-panel" => {
            let next_cursor = advance_past_token(raw, cursor, tok);
            let value = tokens
                .get(i + 1)
                .ok_or_else(|| "--max-panel requires a value".to_string())?;
            set_once(&mut acc.max_panel, parse_max_panel(value)?, "--max-panel")?;
            Ok(FlagStep::Consumed {
                next_i: i + 2,
                next_cursor: advance_past_token(raw, next_cursor, value),
            })
        }
        "--implement" => {
            if acc.implement {
                return Err("--implement specified more than once".into());
            }
            acc.implement = true;
            Ok(consumed_one(cursor))
        }
        flag if flag.starts_with("--verify=") => {
            acc.verify_commands
                .push(flag["--verify=".len()..].to_string());
            Ok(consumed_one(cursor))
        }
        "--verify" => {
            let next_cursor = advance_past_token(raw, cursor, tok);
            let value = tokens
                .get(i + 1)
                .filter(|value| !value.starts_with("--"))
                .ok_or_else(|| "--verify requires a quoted command".to_string())?;
            acc.verify_commands.push(value.clone());
            Ok(FlagStep::Consumed {
                next_i: i + 2,
                next_cursor: advance_past_token(raw, next_cursor, value),
            })
        }
        flag if flag.starts_with("--") => {
            Err(format!("unknown flag `{flag}`\n{FUSION_SLASH_USAGE}"))
        }
        _ => Ok(FlagStep::Stop),
    }
}

/// Parse `/fusion` tokens. Unknown flags and empty prompts are errors.
///
/// # Errors
///
/// Returns [`FUSION_SLASH_USAGE`] or a more specific mutex / parse error.
pub fn parse_fusion_slash(args: &ParsedSlashCommand) -> Result<FusionSlashArgs, String> {
    let mut acc = FusionFlagAccum::default();
    let tokens = &args.positional_args;
    let raw = args.raw_args.as_str();
    // `tokens` is shell-quote output: unquoted `*`/`?` globs are dropped,
    // `#` truncates the remainder as a comment, `( ) | & ; < >` are operators.
    // That is fine for recognising leading `--flag` words (always plain
    // ASCII), but the prompt itself must come back from `raw_args` verbatim
    // — so we only use `tokens` to find where the flags end, then recover
    // everything after that point directly from the untokenized string,
    // preserving punctuation and newlines the tokenizer would have mangled.
    let mut cursor = 0usize;
    let mut i = 0;
    while i < tokens.len() {
        match consume_fusion_flag(tokens, raw, i, cursor, &mut acc)? {
            FlagStep::Consumed {
                next_i,
                next_cursor,
            } => {
                i = next_i;
                cursor = next_cursor;
            }
            FlagStep::Stop => break,
        }
    }
    let prompt = raw[cursor.min(raw.len())..].trim().to_string();
    if prompt.is_empty() {
        return Err(FUSION_SLASH_USAGE.to_string());
    }
    if !acc.verify_commands.is_empty() && !acc.implement {
        return Err("--verify requires --implement".into());
    }
    let mode = if acc.implement {
        FusionPanelMode::Implement
    } else {
        FusionPanelMode::Analysis
    };
    let verify_commands =
        validate_verify_commands(mode, acc.verify_commands).map_err(|error| error.to_string())?;
    Ok(FusionSlashArgs {
        preset: acc.preset,
        cross_provider: acc.cross_provider,
        models: acc.models,
        dimensions: acc.dimensions,
        partial_ok: acc.partial_ok,
        max_panel: acc.max_panel,
        implement: acc.implement,
        verify_commands,
        prompt,
    })
}

/// Advance `cursor` past the next occurrence of `token` in `raw`, searching
/// from `cursor` onward.
///
/// `token` is a value pulled from the shell-quote tokenizer's output. In the
/// common case it is a literal substring of the raw text at this position
/// (surrounded by whatever quotes/backslashes the tokenizer stripped, but
/// not split by them), so a plain [`str::find`] locates it and we additionally
/// skip one trailing closing quote so it doesn't leak into the recovered
/// prompt. That `find` is also what lets an unquoted `*`/`?` glob the
/// tokenizer drops, or a standalone operator character (`| & ; ( ) < >`)
/// that produces no token at all, re-synchronise on the FOLLOWING token
/// instead of desyncing the cursor.
///
/// `find` misses only when the token text itself is no longer a literal
/// substring at this position — e.g. `--models="a, b"` (the `=`-form embeds
/// whitespace inside a quoted span that a raw `find` for the whole token
/// can't match around) or a backslash-escaped space (`a,\ b`). For exactly
/// that case we fall back to [`advance_past_quoted_word`], which walks the
/// RAW text honoring `'`/`"` quoting and `\` escapes the same way
/// [`crate::parser`]'s tokenizer does, so a quoted or escaped span is
/// skipped in full rather than stopping at the first whitespace inside it.
fn advance_past_token(raw: &str, cursor: usize, token: &str) -> usize {
    let cursor = cursor.min(raw.len());
    let rest = &raw[cursor..];
    if let Some(rel) = rest.find(token) {
        let mut end = cursor + rel + token.len();
        // If the raw text quoted this token (e.g. `--models "a,b"`), the
        // shell-quote tokenizer already stripped the surrounding quotes from
        // `token` before handing it to us, so `token` matches only the
        // INSIDE of the raw quoted span — skip the closing quote too, or it
        // leaks as the first character of the recovered prompt.
        let opened_with_quote =
            rel > 0 && matches!(rest.as_bytes().get(rel - 1), Some(b'"' | b'\''));
        if opened_with_quote {
            let opening = rest.as_bytes()[rel - 1];
            if raw.as_bytes().get(end) == Some(&opening) {
                end += 1;
            }
        }
        return end;
    }
    advance_past_quoted_word(raw, cursor)
}

/// Fallback for [`advance_past_token`]: advance `cursor` past the next shell
/// word in `raw`, starting from `cursor` (leading whitespace is skipped
/// first), honoring `'`/`"` quoting and `\` escapes so a whitespace-bearing
/// quoted or escaped span is skipped in full instead of splicing its tail
/// onto the front of the recovered prompt.
fn advance_past_quoted_word(raw: &str, cursor: usize) -> usize {
    let cursor = cursor.min(raw.len());
    let rest = &raw[cursor..];
    let trimmed_start = rest
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(rest.len());
    let start = cursor + trimmed_start;

    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut end = start;
    for c in raw[start..].chars() {
        if escaped {
            escaped = false;
            end += c.len_utf8();
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if q == '"' && c == '\\' {
                escaped = true;
            }
            end += c.len_utf8();
            continue;
        }
        if c == '\\' {
            escaped = true;
            end += c.len_utf8();
            continue;
        }
        if c == '"' || c == '\'' {
            quote = Some(c);
            end += c.len_utf8();
            continue;
        }
        if c.is_whitespace() {
            break;
        }
        end += c.len_utf8();
    }
    end
}

/// Build a [`FusionRequest`] from parsed slash args and parent session identity.
#[must_use]
pub fn fusion_request_from_slash(
    parsed: FusionSlashArgs,
    parent_profile: String,
    parent_model: String,
    slash_cross_provider_default: bool,
    default_preset: FusionPreset,
    default_partial_ok: bool,
) -> FusionRequest {
    let mode = if parsed.implement {
        FusionPanelMode::Implement
    } else {
        FusionPanelMode::Analysis
    };
    let dimensions = parsed.dimensions.unwrap_or_else(|| {
        // An empty list normalizes to the mode's own defaults.
        normalize_dimensions_for(mode, Vec::new()).unwrap_or_default()
    });
    FusionRequest {
        schema_version: FUSION_SCHEMA_VERSION,
        origin: FusionOrigin::Slash,
        prompt: parsed.prompt,
        preset: parsed.preset.unwrap_or(default_preset),
        models: parsed.models,
        dimensions,
        partial_ok: parsed.partial_ok.unwrap_or(default_partial_ok),
        max_panel: parsed.max_panel,
        cross_provider: parsed
            .cross_provider
            .unwrap_or(slash_cross_provider_default),
        parent_profile,
        parent_model,
        mode,
        verify_commands: parsed.verify_commands,
    }
}

fn set_preset(slot: &mut Option<FusionPreset>, value: FusionPreset) -> Result<(), String> {
    if let Some(existing) = *slot {
        if existing == value {
            let flag = match value {
                FusionPreset::Quality => "--quality",
                FusionPreset::Fast => "--fast",
            };
            return Err(format!("{flag} specified more than once"));
        }
        return Err("--quality and --fast are mutually exclusive".into());
    }
    *slot = Some(value);
    Ok(())
}

fn set_cross(slot: &mut Option<bool>, value: bool) -> Result<(), String> {
    if let Some(existing) = *slot {
        if existing == value {
            let flag = if value {
                "--cross-provider"
            } else {
                "--same-provider"
            };
            return Err(format!("{flag} specified more than once"));
        }
        return Err("--same-provider and --cross-provider are mutually exclusive".into());
    }
    *slot = Some(value);
    Ok(())
}

fn set_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{flag} specified more than once"));
    }
    *slot = Some(value);
    Ok(())
}

fn parse_csv(raw: &str, flag: &str) -> Result<Vec<String>, String> {
    let values = raw
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if values.is_empty() || values.iter().any(String::is_empty) {
        return Err(format!("{flag} must not contain empty entries"));
    }
    Ok(values)
}

/// Splits `raw` on commas, then parses each entry through the single
/// `platform_api::parse_fusion_model_ref` the Agent tool also routes through,
/// so a malformed entry (e.g. `openai:` — a colon with an empty model) is
/// rejected identically from every entrypoint.
fn parse_models(raw: &str) -> Result<Vec<FusionModelRef>, String> {
    let mut out = Vec::new();
    for item in parse_csv(raw, "--models")? {
        out.push(platform_api::parse_fusion_model_ref(&item).map_err(|error| error.to_string())?);
    }
    if out.len() < usize::from(FUSION_MIN_PANEL) {
        return Err("explicit --models must contain at least 2 entries".into());
    }
    Ok(out)
}

fn parse_dimensions(raw: &str) -> Result<Vec<String>, String> {
    normalize_dimensions(parse_csv(raw, "--dimensions")?).map_err(|error| error.to_string())
}

fn parse_max_panel(raw: &str) -> Result<u8, String> {
    let n: u8 = raw
        .parse()
        .map_err(|_| format!("--max-panel `{raw}` is not an integer"))?;
    if !(FUSION_MIN_PANEL..=FUSION_MAX_PANEL).contains(&n) {
        return Err(format!(
            "--max-panel must be {FUSION_MIN_PANEL}..={FUSION_MAX_PANEL}"
        ));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_slash_command;

    fn parse(line: &str) -> Result<FusionSlashArgs, String> {
        let parsed = parse_slash_command(line).expect("slash");
        parse_fusion_slash(&parsed)
    }

    #[test]
    fn quoted_flag_value_does_not_leak_its_closing_quote_into_the_prompt() {
        let args =
            parse("/fusion --models \"openai:gpt-5,anthropic:opus\" review the plan").unwrap();
        assert_eq!(args.prompt, "review the plan");
    }

    #[test]
    fn empty_prompt_is_usage() {
        assert_eq!(parse("/fusion").unwrap_err(), FUSION_SLASH_USAGE);
        assert_eq!(parse("/fusion --quality").unwrap_err(), FUSION_SLASH_USAGE);
    }

    #[test]
    fn quality_and_fast_are_mutex() {
        let err = parse("/fusion --quality --fast review this").unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn same_and_cross_are_mutex() {
        let err = parse("/fusion --same-provider --cross-provider review this").unwrap_err();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn omitted_preset_and_cross_provider_are_deferred_to_runtime_settings() {
        let args = parse("/fusion review the plan").unwrap();
        assert_eq!(args.preset, None);
        assert_eq!(args.cross_provider, None);
        assert_eq!(args.prompt, "review the plan");
    }

    #[test]
    fn same_provider_and_fast() {
        let args = parse("/fusion --fast --same-provider check locking").unwrap();
        assert_eq!(args.preset, Some(FusionPreset::Fast));
        assert_eq!(args.cross_provider, Some(false));
        assert_eq!(args.prompt, "check locking");
    }

    #[test]
    fn models_and_dimensions() {
        let args = parse(
            "/fusion --models anthropic:claude-sonnet-5,openai:gpt-5.6-terra --dimensions coverage,reasoning review",
        )
        .unwrap();
        assert_eq!(args.models.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            args.dimensions.as_deref(),
            Some(["coverage".to_string(), "reasoning".to_string()].as_slice())
        );
    }

    #[test]
    fn unknown_flag_rejected() {
        let err = parse("/fusion --nope review").unwrap_err();
        assert!(err.contains("unknown flag"), "{err}");
    }

    #[test]
    fn request_uses_slash_cross_default() {
        let parsed = parse("/fusion do the thing").unwrap();
        let req = fusion_request_from_slash(
            parsed,
            "anthropic".into(),
            "claude-sonnet-5".into(),
            true,
            FusionPreset::Fast,
            true,
        );
        assert!(req.cross_provider);
        assert_eq!(req.preset, FusionPreset::Fast);
        assert_eq!(req.origin, FusionOrigin::Slash);
    }

    #[test]
    fn a_colon_with_an_empty_model_is_rejected_with_the_same_message_as_the_agent_tool() {
        // `platform_api::parse_fusion_model_ref` is the single implementation
        // both `/fusion` and the Agent tool (`tools/agent/src/agent.rs`'s
        // `parse_fusion_models`) route a `--models`/`models[]` entry through,
        // so an invalid entry rejects identically from either entrypoint.
        let err = parse("/fusion --models openai:,anthropic:opus review").unwrap_err();
        assert!(
            err.contains("invalid fusion request: invalid fusion models entry `openai:`"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_models_flag_is_rejected() {
        let err = parse("/fusion --models a,b --models c,d review").unwrap_err();
        assert!(err.contains("specified more than once"), "{err}");
    }

    #[test]
    fn duplicate_same_provider_flag_is_rejected() {
        let err = parse("/fusion --same-provider --same-provider review").unwrap_err();
        assert!(err.contains("specified more than once"), "{err}");
    }

    #[test]
    fn empty_csv_entries_are_rejected() {
        assert!(parse("/fusion --models a,,b review")
            .unwrap_err()
            .contains("empty entries"));
        assert!(parse("/fusion --dimensions coverage, review")
            .unwrap_err()
            .contains("empty entries"));
        assert!(parse("/fusion --dimensions= review")
            .unwrap_err()
            .contains("empty entries"));
    }

    #[test]
    fn dimensions_are_validated_and_deduplicated_before_spawn() {
        let args = parse("/fusion --dimensions coverage,coverage,reasoning review").unwrap();
        assert_eq!(
            args.dimensions,
            Some(vec!["coverage".to_string(), "reasoning".to_string()])
        );
        assert!(parse("/fusion --dimensions Not-Snake review").is_err());
    }

    #[test]
    fn max_panel_accepts_only_the_locked_range() {
        assert_eq!(
            parse("/fusion --max-panel 2 review").unwrap().max_panel,
            Some(2)
        );
        assert_eq!(
            parse("/fusion --max-panel=8 review").unwrap().max_panel,
            Some(8)
        );
        assert!(parse("/fusion --max-panel 1 review").is_err());
        assert!(parse("/fusion --max-panel 9 review").is_err());
    }

    #[test]
    fn prompt_recovers_verbatim_text_shell_quote_would_mangle() {
        // Bare `*` `?` are dropped as globs, `#` truncates as a comment and
        // `(` `)` `|` etc. are operators in the shell-quote tokenizer that
        // `positional_args` goes through — the prompt must NOT be rebuilt
        // from those tokens, it must come back from `raw_args` verbatim.
        let args = parse("/fusion what does foo(bar) do?").unwrap();
        assert_eq!(args.prompt, "what does foo(bar) do?");

        let args = parse("/fusion review PR #123 for races").unwrap();
        assert_eq!(args.prompt, "review PR #123 for races");

        let args = parse("/fusion find a * b").unwrap();
        assert_eq!(args.prompt, "find a * b");
    }

    #[test]
    fn prompt_preserves_newlines_after_leading_flags() {
        let args = parse("/fusion --quality line one\nline two").unwrap();
        assert_eq!(args.prompt, "line one\nline two");
    }

    #[test]
    fn flags_before_punctuation_heavy_prompt_are_still_consumed() {
        let args =
            parse("/fusion --fast --same-provider --dimensions coverage what about foo(bar)?")
                .unwrap();
        assert_eq!(args.preset, Some(FusionPreset::Fast));
        assert_eq!(args.cross_provider, Some(false));
        assert_eq!(args.prompt, "what about foo(bar)?");
    }

    #[test]
    fn request_uses_partial_default_when_flag_omitted() {
        let parsed = parse("/fusion do the thing").unwrap();
        let req = fusion_request_from_slash(
            parsed,
            "anthropic".into(),
            "claude-sonnet-5".into(),
            true,
            FusionPreset::Quality,
            false,
        );
        assert!(!req.partial_ok);
    }

    #[test]
    fn quoted_equals_flag_value_with_internal_whitespace_does_not_leak_into_prompt() {
        // `--models="a, b"` tokenizes to one token `--models=a, b` with the
        // quotes stripped, so `advance_past_token`'s literal `find` for that
        // whole token fails against the still-quoted raw text and used to
        // fall back to a plain whitespace boundary, landing INSIDE the
        // quoted span and splicing its tail (plus the stray closing quote)
        // onto the front of the prompt.
        let args = parse("/fusion --models=\"openai:gpt-5, anthropic:opus\" summarize this design")
            .unwrap();
        assert_eq!(args.prompt, "summarize this design");
        assert_eq!(args.models.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn escaped_space_in_equals_flag_value_does_not_leak_into_prompt() {
        let args =
            parse("/fusion --models=openai:gpt-5,\\ anthropic:opus review the plan").unwrap();
        assert_eq!(args.prompt, "review the plan");
        assert_eq!(args.models.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn quoted_equals_dimensions_with_internal_whitespace_does_not_leak_into_prompt() {
        let args = parse("/fusion --dimensions=\"coverage, reasoning\" review the plan").unwrap();
        assert_eq!(args.prompt, "review the plan");
        assert_eq!(
            args.dimensions.as_deref(),
            Some(["coverage".to_string(), "reasoning".to_string()].as_slice())
        );
    }

    #[test]
    fn unquoted_glob_word_between_flags_does_not_desync_the_cursor() {
        // `*.rs` is an unquoted glob: `crate::parser`'s tokenizer
        // drops it entirely (it never becomes a `positional_args` token,
        // per `flush_word`), so the flag loop's token list skips straight
        // from `--fast` to `--max-panel`. A cursor-advance strategy that
        // walks exactly one raw shell word per consumed token desyncs here
        // — it lands on `*.rs` instead of `--max-panel` and never recovers.
        // `advance_past_token`'s literal `find` for the LITERAL flag text
        // scans forward past the dropped glob and resynchronises.
        let args = parse("/fusion --fast *.rs --max-panel 4 review the plan").unwrap();
        assert_eq!(args.prompt, "review the plan");
        assert_eq!(args.preset, Some(FusionPreset::Fast));
        assert_eq!(args.max_panel, Some(4));
    }

    #[test]
    fn standalone_shell_operator_between_flags_does_not_desync_the_cursor() {
        // A bare `|` is a shell operator: the tokenizer emits no token at
        // all for it (parser.rs's operator handling), so — like the glob
        // case above — the token list skips straight from `--fast` to
        // `--max-panel` while the raw text still has the operator sitting
        // between them.
        let args = parse("/fusion --fast | --max-panel 4 review the plan").unwrap();
        assert_eq!(args.prompt, "review the plan");
        assert_eq!(args.preset, Some(FusionPreset::Fast));
        assert_eq!(args.max_panel, Some(4));
    }

    #[test]
    fn implement_and_verify_flags_are_parsed_and_the_prompt_is_recovered() {
        let args = parse(
            "/fusion --implement --verify \"cargo check --locked\" --verify 'cargo test -p foo && echo ok' fix the flaky retry",
        )
        .unwrap();
        assert!(args.implement);
        assert_eq!(
            args.verify_commands,
            vec![
                "cargo check --locked".to_string(),
                "cargo test -p foo && echo ok".to_string()
            ]
        );
        assert_eq!(args.prompt, "fix the flaky retry");

        let equals = parse("/fusion --implement --verify=\"cargo check\" do it").unwrap();
        assert_eq!(equals.verify_commands, vec!["cargo check".to_string()]);
        assert_eq!(equals.prompt, "do it");

        let plain = parse("/fusion --implement refactor the parser").unwrap();
        assert!(plain.implement);
        assert!(plain.verify_commands.is_empty());
        assert_eq!(plain.prompt, "refactor the parser");
        assert!(!parse("/fusion review this").unwrap().implement);
    }

    #[test]
    fn verify_needs_implement_and_a_command() {
        let err = parse("/fusion --verify \"cargo check\" review").unwrap_err();
        assert!(err.contains("--verify requires --implement"), "{err}");
        let err = parse("/fusion --implement --verify").unwrap_err();
        assert!(err.contains("--verify requires a quoted command"), "{err}");
        let err = parse("/fusion --implement --verify --quality review").unwrap_err();
        assert!(err.contains("--verify requires a quoted command"), "{err}");
        let err = parse("/fusion --implement --implement review").unwrap_err();
        assert!(err.contains("specified more than once"), "{err}");
    }

    #[test]
    fn implement_requests_carry_the_commands_and_default_to_implement_dimensions() {
        let request = |line: &str| {
            fusion_request_from_slash(
                parse(line).unwrap(),
                "anthropic".into(),
                "claude-sonnet-5".into(),
                true,
                FusionPreset::Quality,
                true,
            )
        };
        let implement = request("/fusion --implement --verify \"cargo check\" do it");
        assert_eq!(implement.mode, FusionPanelMode::Implement);
        assert_eq!(implement.verify_commands, vec!["cargo check".to_string()]);
        assert_eq!(
            implement.dimensions,
            platform_api::DEFAULT_IMPLEMENT_FUSION_DIMENSIONS
        );
        let custom = request("/fusion --implement --dimensions scope,speed do it");
        assert_eq!(
            custom.dimensions,
            vec!["scope".to_string(), "speed".to_string()]
        );

        let analysis = request("/fusion review it");
        assert_eq!(analysis.mode, FusionPanelMode::Analysis);
        assert!(analysis.verify_commands.is_empty());
        assert_eq!(analysis.dimensions, platform_api::DEFAULT_FUSION_DIMENSIONS);
    }
}
