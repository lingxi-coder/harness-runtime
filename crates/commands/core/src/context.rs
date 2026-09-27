//! `/context` — render the context-usage panel.
//!
//! 1:1 (flat-template) port of the claude-code `type: 'local'`
//! `contextNonInteractive` variant
//! (`src/commands/context/context-noninteractive.ts`). The TS
//! `formatContextAsMarkdownTable(data)` renders a `## Context Usage` panel
//! whose header is:
//!   `**Model:** {model}  ` (two trailing spaces = markdown hard break)
//!   `**Tokens:** {used} / {max} ({pct}%)`
//! followed by the rich per-category / MCP / agent / memory-file / skill
//! markdown tables.
//!
//! `LingXi` delivers the header, token-usage line, and the category rows from
//! the additive [`OrchestratorHandle::context_usage_snapshot`] surface.
//!
//! The percentage matches the TS `Math.round((total / max) * 100)`.

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::OrchestratorHandle;
use std::sync::Arc;

/// `/context` handler — renders the context-usage header panel.
#[derive(Clone)]
pub struct ContextHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ContextHandler {
    /// Construct a `ContextHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ContextHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let snap = self.handle.get_status_snapshot().await;
        let usage = self.handle.context_usage_snapshot().await;
        CommandResult::Done {
            display: Some(render_context(&snap.model, &usage)),
        }
    }
    fn name(&self) -> &str {
        "context"
    }
    fn description(&self) -> &str {
        "Show current context usage"
    }
}

/// Render the locked `## Context Usage` header panel.
///
/// Matches the TS `formatContextAsMarkdownTable` header lines (the rich
/// sub-tables are deferred). The `**Model:**` line carries the markdown
/// hard-break (two trailing spaces) exactly as the TS source does.
#[must_use]
fn render_context(model: &str, usage: &platform_api::ContextUsageSnapshot) -> String {
    let used = usage.live_context_tokens;
    let max = usage.max_context_tokens;
    let pct = percentage(used, max);
    let used = format_tokens(used);
    let max = format_tokens(max);
    let mut out =
        format!("## Context Usage\n\n**Model:** {model}  \n**Tokens:** {used} / {max} ({pct}%)\n");
    if !usage.breakdown.is_empty() {
        out.push_str("\n| Category | Tokens | Percentage |\n");
        out.push_str("|---|---:|---:|\n");
        for row in &usage.breakdown {
            let row_pct = percentage(row.tokens, usage.max_context_tokens);
            out.push_str(&format!(
                "| {} | {} | {row_pct}% |\n",
                category_label(row.kind),
                format_tokens(row.tokens)
            ));
        }
    }
    if let Some(warning) = usage.overflow_warning(
        std::env::var_os("DISABLE_COMPACT").is_some_and(|value| !value.is_empty()),
    ) {
        out.push_str("\n⚠ ");
        out.push_str(&warning);
        out.push('\n');
    }
    out
}

fn category_label(kind: platform_api::ContextUsageCategoryKind) -> &'static str {
    use platform_api::ContextUsageCategoryKind as Kind;
    match kind {
        Kind::SystemPrompt => "System prompt",
        Kind::SystemTools => "System tools",
        Kind::McpTools => "MCP tools",
        Kind::MemoryFiles => "Memory files",
        Kind::Skills => "Skills",
        Kind::Messages => "Messages",
        Kind::AutocompactBuffer => "Autocompact buffer",
        Kind::FreeSpace => "Free space",
    }
}

/// Compact token count, 1:1 with the TS `formatTokens` → `formatNumber`
/// (`Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 })`
/// then `.toLowerCase()` then strip a trailing `.0`): `50_000` → `"50k"`,
/// `12_345` → `"12.3k"`, `1_500_000` → `"1.5m"`, and anything under `1_000`
/// verbatim (`0` → `"0"`).
///
/// The f64 division is exact for the token magnitudes seen here (well within
/// f64's 2^53 integer range); the cast lints are allowed accordingly.
#[must_use]
#[allow(clippy::cast_precision_loss)]
fn format_tokens(n: u64) -> String {
    const UNITS: [(u64, char); 4] = [
        (1_000_000_000_000, 't'),
        (1_000_000_000, 'b'),
        (1_000_000, 'm'),
        (1_000, 'k'),
    ];
    for &(threshold, suffix) in &UNITS {
        if n >= threshold {
            // Round to 1 fraction digit (Intl's `maximumFractionDigits: 1`,
            // round-half-up), then drop a trailing `.0` like the TS `.replace`.
            let rounded = ((n as f64 / threshold as f64) * 10.0).round() / 10.0;
            let s = format!("{rounded:.1}");
            let s = s.strip_suffix(".0").unwrap_or(&s);
            return format!("{s}{suffix}");
        }
    }
    n.to_string()
}

/// `round((used / max) * 100)`, matching the TS `Math.round`. Returns `0`
/// when `max` is `0` (no model budget recorded), avoiding a divide-by-zero.
///
/// The f64 conversions are exact for the token counts seen here (well within
/// f64's 2^53 integer range), and the rounded percentage fits a `u64`; the
/// cast lints are allowed at the function level accordingly.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn percentage(used: u64, max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    // f64 round mirrors JS `Math.round` (round-half-up for the values seen here).
    ((used as f64 / max as f64) * 100.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "context".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[test]
    fn renders_header_with_locked_layout() {
        let s = render_context(
            "claude-opus-4-7",
            &platform_api::ContextUsageSnapshot {
                live_context_tokens: 50_000,
                max_context_tokens: 200_000,
                ..Default::default()
            },
        );
        // The `**Model:**` line ends with the markdown hard-break (two
        // trailing spaces) exactly as the TS source emits. Build `expected`
        // by concatenation so the trailing spaces are explicit (and survive
        // editor whitespace trimming).
        let expected = String::new()
            + "## Context Usage\n\n"
            + "**Model:** claude-opus-4-7  \n"
            + "**Tokens:** 50k / 200k (25%)\n";
        assert_eq!(s, expected);
    }

    #[test]
    fn renders_shared_category_breakdown() {
        use platform_api::{ContextUsageCategory, ContextUsageCategoryKind as Kind};
        let s = render_context(
            "claude-opus-5",
            &platform_api::ContextUsageSnapshot {
                live_context_tokens: 100_000,
                max_context_tokens: 1_000_000,
                breakdown: vec![
                    ContextUsageCategory::new(Kind::SystemPrompt, 25_000),
                    ContextUsageCategory::new(Kind::Messages, 75_000),
                    ContextUsageCategory::new(Kind::AutocompactBuffer, 13_000),
                    ContextUsageCategory::new(Kind::FreeSpace, 887_000),
                ],
                ..Default::default()
            },
        );
        assert!(s.contains("**Tokens:** 100k / 1m (10%)"));
        assert!(s.contains("| System prompt | 25k | 3% |"));
        assert!(s.contains("| Autocompact buffer | 13k | 1% |"));
        assert!(s.contains("| Free space | 887k | 89% |"));
    }

    #[test]
    fn renders_explicit_over_context_warning() {
        let s = render_context(
            "claude-opus-5",
            &platform_api::ContextUsageSnapshot {
                live_context_tokens: 1_012_345,
                max_context_tokens: 1_000_000,
                ..Default::default()
            },
        );
        assert!(s.contains(
            "⚠ Context exceeds the 1m-token limit by 12.3k tokens \u{2014} run /compact or /clear to continue."
        ));
    }

    #[test]
    fn format_tokens_matches_ts_compact_notation() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(12_345), "12.3k");
        assert_eq!(format_tokens(50_000), "50k");
        assert_eq!(format_tokens(200_000), "200k");
        assert_eq!(format_tokens(1_000_000), "1m");
        assert_eq!(format_tokens(1_500_000), "1.5m");
    }

    #[test]
    fn percentage_rounds_like_math_round() {
        // 25% exactly.
        assert_eq!(percentage(50_000, 200_000), 25);
        // 12.5% -> rounds to 13 (round-half-up).
        assert_eq!(percentage(25_000, 200_000), 13);
        // Zero max never divides by zero.
        assert_eq!(percentage(10, 0), 0);
        // Zero usage.
        assert_eq!(percentage(0, 200_000), 0);
    }

    #[tokio::test]
    async fn default_handle_renders_zero_usage() {
        // The mock inherits the trait defaults: empty status model + (0, 0)
        // window usage. With max = 0 the percentage is the safe 0%.
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ContextHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert!(s.starts_with("## Context Usage\n\n**Model:** "));
            assert!(s.contains("**Tokens:** 0 / 0 (0%)"));
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn reflects_status_snapshot_model() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_status_snapshot(platform_api::StatusSnapshot {
            model: "claude-sonnet-4-6".into(),
            ..platform_api::StatusSnapshot::default()
        });
        let h = ContextHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert!(s.contains("**Model:** claude-sonnet-4-6  \n"));
        } else {
            panic!();
        }
    }

    #[test]
    fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ContextHandler::new(mock);
        assert_eq!(h.name(), "context");
        assert_eq!(h.description(), "Show current context usage");
    }
}
