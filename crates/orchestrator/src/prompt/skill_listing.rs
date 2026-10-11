//! `skill_listing` system-reminder formatter + provider seam.
//!
//! 1:1 with claude-code `src/tools/SkillTool/prompt.ts` (the
//! `formatCommandsWithinBudget` budgeter) and the `messages.ts:3728-3738`
//! renderer that wraps the listing in a `<system-reminder>` meta user message.
//!
//! IMPORTANT: the skill listing is NOT a system-prompt section. It is a
//! per-turn, TRANSIENT meta user message appended to the outgoing message
//! snapshot (never to `session.history` / JSONL), exactly like the OUTSTYLE.3
//! output-style reminder. See
//! [`crate::conversation::ConversationOrchestrator::skill_listing_reminder_message`].
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lingxi_core::types::utf16_json::{Utf16JsonBuilder, Utf16JsonProjection as Source};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Minimal per-skill view the formatter needs. The composition root builds
/// these from `command_api::SlashCommand` so the orchestrator crate need not
/// depend on `command-api`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillListingEntry {
    /// Skill name (TS `cmd.name`).
    pub name: Source,
    /// Short description (TS `cmd.description`).
    pub description: Source,
    /// Optional "when to use" guidance (TS `cmd.whenToUse`).
    pub when_to_use: Option<Source>,
    /// TS `cmd.source === 'bundled'` — bundled skills are never truncated.
    pub is_bundled: bool,
}

/// Supplies the model-invocable skill entries for the per-turn `skill_listing`
/// reminder. The production impl wraps the shared `CommandRegistry`; tests
/// inject a static fixture. Mirrors TS `getSkillToolCommands(cwd)`
/// (`commands.ts:565`).
#[async_trait]
pub trait SkillListingProvider: Send + Sync {
    /// Return the eligible skill entries (already filtered to model-invocable
    /// prompt skills). May be empty.
    async fn skill_entries(&self) -> Vec<SkillListingEntry>;

    /// Snapshot precommit suggestions using the same eligible skill catalog.
    async fn bash_precommit_skills(&self) -> tool_api::tool_trait::BashPrecommitSkills {
        tool_api::tool_trait::BashPrecommitSkills::default()
    }
}

/// A provider whose entries are loaded on demand from a host-owned source.
///
/// Composition roots use this to bridge their live command registry without
/// making the orchestrator depend on `command-api`. The closure is invoked for
/// every listing request, so registry/plugin mutations made after orchestrator
/// construction are visible to the next turn.
pub struct LazySkillListingProvider {
    loader:
        Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<SkillListingEntry>> + Send>> + Send + Sync>,
    precommit_loader: Option<PrecommitSkillsLoader>,
}

type PrecommitSkillsLoader = Arc<
    dyn Fn(
            Vec<SkillListingEntry>,
        )
            -> Pin<Box<dyn Future<Output = tool_api::tool_trait::BashPrecommitSkills> + Send>>
        + Send
        + Sync,
>;

impl LazySkillListingProvider {
    /// Wrap an async loader as a [`SkillListingProvider`].
    pub fn new<F, Fut>(loader: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<SkillListingEntry>> + Send + 'static,
    {
        let loader = Arc::new(
            move || -> Pin<Box<dyn Future<Output = Vec<SkillListingEntry>> + Send>> {
                Box::pin(loader())
            },
        );
        Self {
            loader,
            precommit_loader: None,
        }
    }

    /// Attach host provenance/settings resolution without changing listing rows.
    pub fn with_bash_precommit_skills<F, Fut>(mut self, loader: F) -> Self
    where
        F: Fn(Vec<SkillListingEntry>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = tool_api::tool_trait::BashPrecommitSkills> + Send + 'static,
    {
        self.precommit_loader = Some(Arc::new(move |entries| Box::pin(loader(entries))));
        self
    }
}

#[async_trait]
impl SkillListingProvider for LazySkillListingProvider {
    async fn skill_entries(&self) -> Vec<SkillListingEntry> {
        (self.loader)().await
    }

    async fn bash_precommit_skills(&self) -> tool_api::tool_trait::BashPrecommitSkills {
        let Some(loader) = self.precommit_loader.as_ref() else {
            return tool_api::tool_trait::BashPrecommitSkills::default();
        };
        loader(self.skill_entries().await).await
    }
}

// prompt.ts:20-29
const SKILL_BUDGET_CONTEXT_PERCENT: f64 = 0.01;
const CHARS_PER_TOKEN: usize = 4;
const DEFAULT_CHAR_BUDGET: usize = 8_000;
const MIN_DESC_LENGTH: usize = 20;

/// prompt.ts:31-41. Honors the `SLASH_COMMAND_TOOL_CHAR_BUDGET` override 1:1.
fn char_budget(context_window_tokens: Option<usize>) -> usize {
    if let Some(v) = std::env::var("SLASH_COMMAND_TOOL_CHAR_BUDGET")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n != 0)
    {
        return v;
    }
    match context_window_tokens {
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        Some(t) => ((t * CHARS_PER_TOKEN) as f64 * SKILL_BUDGET_CONTEXT_PERCENT).floor() as usize,
        None => DEFAULT_CHAR_BUDGET,
    }
}

/// Native 2.1.293 retains the full description when the listing fits its
/// overall budget. Guidance remains part of the same source string.
fn skill_description(e: &SkillListingEntry) -> Source {
    match &e.when_to_use {
        Some(w) if !units(w).is_empty() => concatenate(&[&e.description, &" - ".into(), w]),
        _ => e.description.clone(),
    }
}

fn full_line(e: &SkillListingEntry) -> Source {
    concatenate(&[&"- ".into(), &e.name, &": ".into(), &skill_description(e)])
}

fn units(source: &Source) -> Vec<u16> {
    source.string_units("").expect("skill listing text")
}

fn from_units(units: Vec<u16>) -> Source {
    let mut builder = Utf16JsonBuilder::new();
    let root = builder.push_string_units(units);
    builder.finish(root).expect("skill listing source text")
}

fn concatenate(parts: &[&Source]) -> Source {
    from_units(parts.iter().flat_map(|part| units(part)).collect())
}

fn join_lines(lines: &[Source]) -> Source {
    let mut output = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            output.push(u16::from(b'\n'));
        }
        output.extend(units(line));
    }
    from_units(output)
}

// Rust's Unicode routines need scalar text. The measurement projection keeps
// every UTF-16 offset (one replacement scalar per isolated unit); output is
// always sliced from the original units. Bun assigns isolated surrogates zero
// display columns, unlike the replacement character used only for segmentation.
fn measurement_text(units: &[u16]) -> (String, usize) {
    let mut isolated = 0;
    let text = char::decode_utf16(units.iter().copied())
        .map(|value| {
            value.unwrap_or_else(|_| {
                isolated += 1;
                '\u{fffd}'
            })
        })
        .collect();
    (text, isolated)
}

fn units_width(units: &[u16]) -> usize {
    let (text, isolated) = measurement_text(units);
    UnicodeWidthStr::width(text.as_str()).saturating_sub(isolated)
}

fn string_width(source: &Source) -> usize {
    units_width(&units(source))
}

fn truncate_to_width(source: &Source, max_width: usize) -> Source {
    let original = units(source);
    if units_width(&original) <= max_width {
        return source.clone();
    }
    let (measurement, _) = measurement_text(&original);
    let mut used = 0;
    let mut end = 0;
    for grapheme in measurement.graphemes(true) {
        let next = end + grapheme.encode_utf16().count();
        let width = units_width(&original[end..next]);
        if used + width > max_width {
            break;
        }
        used += width;
        end = next;
    }
    from_units(original[..end].to_vec())
}

/// prompt.ts:70-171. Returns the listing body (no `<system-reminder>` wrapper);
/// empty string when there are no entries.
///
#[must_use]
pub fn format_within_budget(
    entries: &[SkillListingEntry],
    context_window_tokens: Option<usize>,
) -> Source {
    if entries.is_empty() {
        return "".into();
    }
    let budget = char_budget(context_window_tokens);

    let full: Vec<Source> = entries.iter().map(full_line).collect();
    let join_overhead = full.len().saturating_sub(1); // newlines between lines
    let full_total: usize = full.iter().map(|l| string_width(l)).sum::<usize>() + join_overhead;
    if full_total <= budget {
        return join_lines(&full);
    }

    // Over budget: bundled skills stay full; the rest share the remaining budget
    // evenly across their descriptions, falling back to names-only (prompt.ts:92-170).
    let bundled_chars: usize = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.is_bundled)
        .map(|(i, _)| string_width(&full[i]) + 1) // +1 newline
        .sum();
    let rest: Vec<usize> = (0..entries.len())
        .filter(|&i| !entries[i].is_bundled)
        .collect();
    if rest.is_empty() {
        return join_lines(&full);
    }
    let remaining = budget.saturating_sub(bundled_chars);
    let rest_name_overhead: usize = rest
        .iter()
        .map(|&i| string_width(&entries[i].name) + "- ".len() + ": ".len())
        .sum::<usize>()
        + rest.len().saturating_sub(1); // newlines among rest
    let max_desc_len = remaining.saturating_sub(rest_name_overhead) / rest.len();

    join_lines(
        &entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                if e.is_bundled {
                    full[i].clone()
                } else if max_desc_len < MIN_DESC_LENGTH {
                    concatenate(&[&"- ".into(), &e.name]) // names-only fallback (prompt.ts:137-141)
                } else {
                    let d = skill_description(e);
                    let d = truncate_to_width(&d, max_desc_len);
                    concatenate(&[&"- ".into(), &e.name, &": ".into(), &d])
                }
            })
            .collect::<Vec<_>>(),
    )
}

/// messages.ts:3728-3738 renderer + `wrapInSystemReminder` (`messages.ts:3097`).
/// Returns the full meta-message body (including the `<system-reminder>`
/// envelope), or `None` when the listing is empty.
#[must_use]
pub fn render_reminder(
    entries: &[SkillListingEntry],
    context_window_tokens: Option<usize>,
) -> Option<Source> {
    let body = format_within_budget(entries, context_window_tokens);
    if units(&body).is_empty() {
        return None;
    }
    Some(concatenate(&[
        &"<system-reminder>\nThe following skills are available for use with the Skill tool:\n\n"
            .into(),
        &body,
        &"\n</system-reminder>".into(),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes the two tests that mutate the process-global
    // SLASH_COMMAND_TOOL_CHAR_BUDGET env var. Poison is benign — recover.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn entry(name: &str, desc: &str, when: Option<&str>, bundled: bool) -> SkillListingEntry {
        SkillListingEntry {
            name: name.into(),
            description: desc.into(),
            when_to_use: when.map(Into::into),
            is_bundled: bundled,
        }
    }

    #[tokio::test]
    async fn lazy_provider_invokes_loader_for_each_listing_request() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_loader = calls.clone();
        let provider = LazySkillListingProvider::new(move || {
            calls_for_loader.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { vec![entry("live", "loaded from the current source", None, false)] }
        });

        assert_eq!(
            provider.skill_entries().await[0].name.string_at(""),
            Some("live")
        );
        assert_eq!(
            provider.skill_entries().await[0].name.string_at(""),
            Some("live")
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn empty_entries_render_none() {
        assert_eq!(render_reminder(&[], Some(200_000)), None);
        assert_eq!(format_within_budget(&[], None).string_at(""), Some(""));
    }

    #[test]
    fn renders_byte_exact_system_reminder() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        let entries = vec![
            entry("debug", "Debug a failing test", None, false),
            entry(
                "loop",
                "Run a prompt on a loop",
                Some("for repeated tasks"),
                false,
            ),
        ];
        let got = render_reminder(&entries, Some(200_000)).expect("some");
        let want = "<system-reminder>\n\
            The following skills are available for use with the Skill tool:\n\n\
            - debug: Debug a failing test\n\
            - loop: Run a prompt on a loop - for repeated tasks\n\
            </system-reminder>";
        assert_eq!(got.string_at(""), Some(want));
    }

    #[test]
    fn when_to_use_is_appended_to_description() {
        let e = entry("x", "Base desc", Some("when X happens"), false);
        assert_eq!(
            skill_description(&e).string_at(""),
            Some("Base desc - when X happens")
        );
    }

    #[test]
    fn listing_preserves_source_name_description_and_guidance() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        let entry = SkillListingEntry {
            name: from_units(vec![0xd800]),
            description: from_units(vec![0xdc00]),
            when_to_use: Some(from_units(vec![0xd801])),
            is_bundled: false,
        };
        let actual = format_within_budget(&[entry], None);
        assert_eq!(
            units(&actual),
            vec![45, 32, 0xd800, 58, 32, 0xdc00, 32, 45, 32, 0xd801]
        );
    }

    #[test]
    fn width_truncation_preserves_zero_width_isolated_units_and_whole_graphemes() {
        let mut original = vec![0xd800, 0x301];
        original.extend("中😀".encode_utf16());
        let source = from_units(original);
        assert_eq!(string_width(&source), 4);
        assert_eq!(
            units(&truncate_to_width(&source, 2)),
            vec![0xd800, 0x301, 0x4e2d]
        );
        // A real replacement character retains its own display column.
        assert_eq!(string_width(&"\u{fffd}\u{301}中😀".into()), 5);
    }

    #[test]
    fn in_budget_long_description_matches_native_293_raw_listing_line() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        let entries = [entry("native-width-probe", &"a".repeat(1200), None, false)];
        let actual = format_within_budget(&entries, None);
        // This is a direct slice of the captured Native request, with no
        // JSON decoding, reserialization, whitespace changes, or trailing LF.
        let native = include_bytes!("../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.293/skill-description-width/control-ascii-1200/skill-listing-line.raw.txt");
        assert_eq!(actual.string_at("").unwrap().as_bytes(), native);
    }

    #[test]
    fn in_budget_description_retains_all_cjk_text() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        let long = "中".repeat(200);
        let actual = format_within_budget(&[entry("han", &long, None, false)], None);
        assert_eq!(
            units(&actual),
            format!("- han: {long}").encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn in_budget_description_retains_all_emoji_graphemes() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        let long = "👨‍👩‍👧‍👦".repeat(200);
        let actual = format_within_budget(&[entry("emoji", &long, None, false)], None);
        assert_eq!(
            units(&actual),
            format!("- emoji: {long}")
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn width_truncation_respects_zero_and_one_column_budgets() {
        assert_eq!(truncate_to_width(&"wide".into(), 0).string_at(""), Some(""));
        assert_eq!(
            truncate_to_width(&"wide".into(), 1).string_at(""),
            Some("w")
        );
    }

    #[test]
    fn tiny_budget_falls_back_to_names_only_for_non_bundled() {
        // A tiny budget forces the truncation path; non-bundled entries collapse
        // to "- name" while bundled entries keep their full line.
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        let entries = vec![
            entry("bundled_one", "Bundled stays full", None, true),
            entry(
                "user_a",
                "Some user skill description that is long",
                None,
                false,
            ),
            entry(
                "user_b",
                "Another user skill description that is long",
                None,
                false,
            ),
        ];
        let body = format_within_budget(&entries, Some(1_000)); // budget = 40 chars
        assert!(body
            .string_at("")
            .unwrap()
            .contains("- bundled_one: Bundled stays full"));
        assert!(body.string_at("").unwrap().contains("- user_a"));
        assert!(body.string_at("").unwrap().contains("- user_b"));
        // Non-bundled descriptions are dropped under a tiny budget.
        assert!(!body
            .string_at("")
            .unwrap()
            .contains("Some user skill description"));
    }

    #[test]
    fn budget_counts_cjk_descriptions_by_display_width() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SLASH_COMMAND_TOOL_CHAR_BUDGET", "30");
        let entries = vec![entry("han", "中".repeat(20).as_str(), None, false)];
        let body = format_within_budget(&entries, Some(200_000));
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");

        assert_ne!(body, full_line(&entries[0]));
        assert_eq!(body.string_at("").unwrap(), "- han: 中中中中中中中中中中中");
        assert!(string_width(&body) <= 30);
    }

    #[test]
    fn slash_command_tool_char_budget_env_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SLASH_COMMAND_TOOL_CHAR_BUDGET", "5");
        let b = char_budget(Some(200_000));
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        assert_eq!(b, 5);
    }

    #[test]
    fn default_budget_matches_one_percent_of_window() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
        assert_eq!(char_budget(Some(200_000)), 8_000); // 200k * 4 * 0.01
        assert_eq!(char_budget(None), DEFAULT_CHAR_BUDGET);
    }
}
