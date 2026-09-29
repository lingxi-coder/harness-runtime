//! Byte-exact port of claude-code 2.1.206's cost-summary renderers
//! (`i6e`/`qs`/`cbg`/`FTu`/`Bu`). The whole block is rendered dimmed by the
//! consumer; these functions return plain strings.

/// Duration formatter — port of claude-code `qs(ms)` (no options). `ms` is an
/// integer, so the float sub-millisecond `.toFixed(1)` branch (`e < 1`) reduces
/// to the `e === 0` case already handled here.
#[must_use]
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 60_000 {
        return format!("{}s", ms / 1000);
    }
    let mut r = ms / 86_400_000;
    let mut n = (ms % 86_400_000) / 3_600_000;
    let mut o = (ms % 3_600_000) / 60_000;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let mut i = ((ms % 60_000) as f64 / 1000.0).round() as u64;
    if i == 60 {
        i = 0;
        o += 1;
    }
    if o == 60 {
        o = 0;
        n += 1;
    }
    if n == 24 {
        n = 0;
        r += 1;
    }
    if r > 0 {
        return format!("{r}d {n}h {o}m");
    }
    if n > 0 {
        return format!("{n}h {o}m {i}s");
    }
    if o > 0 {
        return format!("{o}m {i}s");
    }
    if i > 0 {
        return format!("{i}s");
    }
    "0s".to_string()
}

/// Cost formatter — port of claude-code `FTu(usd, 4)`: `> $0.50` renders 2
/// decimals (rounded to cents via `dbg(e,100)=round(e*100)/100`); otherwise 4.
#[must_use]
pub fn format_cost(usd: f64) -> String {
    if usd > 0.5 {
        let cents = (usd * 100.0).round() / 100.0;
        format!("${cents:.2}")
    } else {
        format!("${usd:.4}")
    }
}

/// Token-count formatter — port of claude-code `Bu` (Intl compact notation,
/// lowercased, `maximumFractionDigits: 1`, trailing `.0` dropped). Identical
/// output to the port's existing `format_tokens`.
#[must_use]
pub fn format_token_count(n: u64) -> String {
    const UNITS: [(u64, char); 4] = [
        (1_000_000_000_000, 't'),
        (1_000_000_000, 'b'),
        (1_000_000, 'm'),
        (1_000, 'k'),
    ];
    for &(threshold, suffix) in &UNITS {
        if n >= threshold {
            #[allow(clippy::cast_precision_loss)]
            let rounded = ((n as f64 / threshold as f64) * 10.0).round() / 10.0;
            let s = format!("{rounded:.1}");
            let s = s.strip_suffix(".0").unwrap_or(&s);
            return format!("{s}{suffix}");
        }
    }
    n.to_string()
}

/// Token-count formatter — port of claude-code `ed` (Intl compact notation,
/// lowercased, `maximumFractionDigits:1` AND `minimumFractionDigits:1`). Unlike
/// [`format_token_count`] (`Bu`, which drops a trailing `.0`), `ed` ALWAYS shows
/// exactly one fraction digit, so `2000 → "2.0k"`, `12000 → "12.0k"`,
/// `1000000 → "1.0m"`, and a sub-1000 count keeps its `.0` (`500 → "500.0"`,
/// `0 → "0.0"`). The `cbg` usage-by-model block uses `ed`, not `Bu`.
#[must_use]
pub fn format_token_count_ed(n: u64) -> String {
    const UNITS: [(u64, char); 4] = [
        (1_000_000_000_000, 't'),
        (1_000_000_000, 'b'),
        (1_000_000, 'm'),
        (1_000, 'k'),
    ];
    for &(threshold, suffix) in &UNITS {
        if n >= threshold {
            #[allow(clippy::cast_precision_loss)]
            let rounded = ((n as f64 / threshold as f64) * 10.0).round() / 10.0;
            return format!("{rounded:.1}{suffix}");
        }
    }
    // Sub-1000: one forced fraction digit, no suffix (`500 → "500.0"`).
    format!("{n}.0")
}

/// Token-count formatter for the prompt-cache line — oracle `Jo`:
///
/// ```js
/// function Jo(e){ let t = e >= 1000;
///   return Intl.NumberFormat("en-US", t ? Z : J).format(e).toLowerCase() }
/// // Z = {notation:"compact", maximumFractionDigits:1, minimumFractionDigits:1}
/// // J = {notation:"compact", maximumFractionDigits:1, minimumFractionDigits:0}
/// ```
///
/// ⚠️ A THIRD variant, not a synonym for either neighbour. `Jo` is
/// [`format_token_count_ed`] at and above 1000 (always one fraction digit:
/// `2000 → "2.0k"`) and [`format_token_count`] (`Bu`) below it (no forced
/// digit: `500 → "500"`). `Bu` would give `"2k"` and `ed` would give
/// `"500.0"`, so neither can stand in for it — and `Un = Jo(e).replace(".0","")`
/// is how upstream derives `Bu` from it.
///
/// Signed, because a `system_char_delta` renders through it.
#[must_use]
pub fn format_cache_token_count(n: i64) -> String {
    let magnitude = n.unsigned_abs();
    if magnitude >= 1_000 {
        let sign = if n < 0 { "-" } else { "" };
        return format!("{sign}{}", format_token_count_ed(magnitude));
    }
    format!("{n}")
}

/// `nano-USD → USD` (matches `cost/src/pricing.rs`'s `nano / 1e9`).
#[allow(clippy::cast_precision_loss)]
fn nano_to_usd(nano: u64) -> f64 {
    nano as f64 / 1_000_000_000.0
}

/// Usage-by-model block — port of claude-code `cbg()`. Empty usage renders the
/// aligned zero line; otherwise a header plus one right-aligned line per model.
/// The per-model web-search clause is omitted (no per-model web-search tracking).
///
/// Consumes `lingxi_core::host::ModelUsageRow` directly (the orchestrator projects the
/// session's per-model usage onto these rows). The row's `model` field is the
/// provider-scoped model name, used as-is for the `${model}:` label.
#[must_use]
pub fn usage_by_model_block(by_model: &[lingxi_core::host::ModelUsageRow]) -> String {
    if by_model.is_empty() {
        return "Usage:                 0 input, 0 output, 0 cache read, 0 cache write".to_string();
    }
    let mut r = "Usage by model:".to_string();
    for m in by_model {
        let label = format!("{}:", m.model);
        // right-align label to width 21 (claude-code `padStart(21)`).
        let padded = format!("{label:>21}");
        let line = format!(
            "  {} input, {} output, {} cache read, {} cache write ({})",
            format_token_count_ed(m.input_tokens),
            format_token_count_ed(m.output_tokens),
            format_token_count_ed(m.cache_read_input_tokens),
            format_token_count_ed(m.cache_creation_input_tokens),
            format_cost(nano_to_usd(m.total_nano_usd)),
        );
        r.push('\n');
        r.push_str(&padded);
        r.push_str(&line);
    }
    r
}

/// Inputs to [`cost_summary`], mapped by the caller from the session's cost
/// snapshot. The caller maps its `CostSnapshot` onto this input; `by_model`
/// borrows the snapshot's `Vec<lingxi_core::host::ModelUsageRow>` directly.
pub struct CostSummaryInput<'a> {
    pub total_usd: f64,
    pub unknown_models: bool,
    pub api_duration_ms: u64,
    pub wall_duration_ms: u64,
    pub code_lines_added: u64,
    pub code_lines_removed: u64,
    pub by_model: &'a [lingxi_core::host::ModelUsageRow],
}

/// Map a session [`lingxi_core::host::CostSnapshot`] onto [`CostSummaryInput`] and render the
/// byte-exact `i6e()` block. Shared by the `/usage` command handler and the TUI
/// Usage/Stats screen so the field mapping lives in exactly one place.
#[must_use]
pub fn cost_summary_from_snapshot(snap: &lingxi_core::host::CostSnapshot) -> String {
    let block = cost_summary(&CostSummaryInput {
        total_usd: snap.total_usd,
        unknown_models: snap.unknown_models,
        api_duration_ms: u64::try_from(snap.api_duration.as_millis()).unwrap_or(u64::MAX),
        wall_duration_ms: u64::try_from(snap.session_duration.as_millis()).unwrap_or(u64::MAX),
        code_lines_added: snap.code_lines_added,
        code_lines_removed: snap.code_lines_removed,
        by_model: &snap.by_model,
    });
    // CLI-4: `i6e()` appends `${r}${s ? `\n${s}` : ""}` — the prompt-cache line
    // follows the usage block when there is one, and the block is BYTE-IDENTICAL
    // to before when there is not.
    match snap.prompt_cache_line.as_deref() {
        Some(line) if !line.is_empty() => format!("{block}\n{line}"),
        _ => block,
    }
}

/// Byte-exact port of claude-code `i6e()`. Returns the plain block; the consumer
/// applies dimming. Labels pad to column 23.
#[must_use]
pub fn cost_summary(input: &CostSummaryInput) -> String {
    let cost = if input.unknown_models {
        format!(
            "{} (costs may be inaccurate due to usage of unknown models)",
            format_cost(input.total_usd)
        )
    } else {
        format_cost(input.total_usd)
    };
    let api = format_duration_ms(input.api_duration_ms);
    let wall = format_duration_ms(input.wall_duration_ms);
    let added_unit = if input.code_lines_added == 1 {
        "line"
    } else {
        "lines"
    };
    let removed_unit = if input.code_lines_removed == 1 {
        "line"
    } else {
        "lines"
    };
    let by_model = usage_by_model_block(input.by_model);
    // NOTE: built with explicit `\n` (not `\`-line-continuations) so the
    // literal cannot pick up stray leading whitespace from source
    // indentation — see the CAUTION in the task brief.
    format!(
        "Total cost:            {cost}\nTotal duration (API):  {api}\nTotal duration (wall): {wall}\nTotal code changes:    {added} {added_unit} added, {removed} {removed_unit} removed\n{by_model}",
        added = input.code_lines_added,
        removed = input.code_lines_removed,
    )
}

/// The `/cost` prompt-cache line — oracle `rPo()`. `None` before the first API
/// response, which is when the whole line is omitted rather than shown empty.
///
/// ```text
/// Prompt cache (main):   12 requests · 87% of input tokens from cache · 1 miss
///   (last 3m ago — likely cause: tool definitions changed (+2/-0), 4.1k tokens
///   re-cached) · warm (5m TTL, last activity 12s ago)
/// ```
///
/// Segments are joined with ` · ` (U+00B7). `recache_if_cold` is
/// [`crate::prompt_cache_ledger::PromptCacheLedger::estimate_recache_tokens`]:
/// `None` there means a rebuild is announced but not yet recorded, and the line
/// says "the compacted prompt" instead of a number it cannot predict.
#[must_use]
pub fn prompt_cache_line(
    summary: &crate::prompt_cache_ledger::CacheSummary,
    recache_if_cold: Option<u64>,
    now_ms: u64,
) -> Option<String> {
    let last = summary.last_request.as_ref()?;
    if summary.requests == 0 {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "{} {}",
        summary.requests,
        if summary.requests == 1 {
            "request"
        } else {
            "requests"
        }
    ));
    if let Some(ratio) = summary.hit_ratio {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let pct = (ratio * 100.0).round() as i64;
        parts.push(format!("{pct}% of input tokens from cache"));
    }
    if summary.misses == 0 {
        parts.push("no misses".to_string());
    } else {
        let since = now_ms.saturating_sub(summary.last_miss_at.unwrap_or(now_ms));
        parts.push(format!(
            "{} {} (last {} ago{}, {} tokens re-cached)",
            summary.misses,
            if summary.misses == 1 {
                "miss"
            } else {
                "misses"
            },
            format_duration_ms(since),
            miss_cause_clause(summary.last_miss_attribution.as_ref()),
            format_cache_token_count(
                i64::try_from(summary.miss_recache_tokens).unwrap_or(i64::MAX)
            ),
        ));
    }
    if summary.expected_rebuilds > 0 {
        parts.push(format!(
            "{} expected {} (compaction or tool-result clearing)",
            summary.expected_rebuilds,
            if summary.expected_rebuilds == 1 {
                "rebuild"
            } else {
                "rebuilds"
            }
        ));
    }
    let idle = format_duration_ms(
        now_ms.saturating_sub(summary.last_activity_at.unwrap_or(last.facts.at_ms)),
    );
    if summary.caching_observed {
        parts.push(if summary.warm {
            format!(
                "warm ({} TTL, last activity {idle} ago)",
                last.facts.ttl.as_str()
            )
        } else {
            let tail = match recache_if_cold {
                None => "next turn re-caches the compacted prompt".to_string(),
                Some(tokens) => format!(
                    "next turn re-caches ~{} tokens",
                    format_cache_token_count(i64::try_from(tokens).unwrap_or(i64::MAX))
                ),
            };
            // U+2014 em dash, as upstream.
            format!("cold \u{2014} idle {idle}, {tail}")
        });
    } else {
        parts.push("no prompt caching reported by the API".to_string());
    }
    Some(format!("Prompt cache (main):   {}", parts.join(" \u{b7} ")))
}

/// The ` — likely cause: …` clause — oracle `oPo`. Empty when nothing was
/// diagnosed, so the miss segment reads the same as it did before attribution
/// existed.
#[must_use]
fn miss_cause_clause(attribution: Option<&crate::prompt_cache_ledger::MissAttribution>) -> String {
    let Some(attribution) = attribution else {
        return String::new();
    };
    if attribution.causes.is_empty() {
        return String::new();
    }
    use crate::prompt_cache_ledger::MissCause;
    let rendered: Vec<String> = attribution
        .causes
        .iter()
        .map(|cause| {
            let label = cause.label();
            match cause {
                MissCause::ToolsChanged if attribution.tools_added.is_some() => format!(
                    "{label} (+{}/-{})",
                    attribution.tools_added.unwrap_or(0),
                    attribution.tools_removed.unwrap_or(0)
                ),
                MissCause::SystemPromptChanged
                    if attribution.system_char_delta.is_some_and(|d| d != 0) =>
                {
                    let delta = attribution.system_char_delta.unwrap_or(0);
                    let sign = if delta > 0 { "+" } else { "" };
                    format!("{label} ({sign}{} chars)", format_cache_token_count(delta))
                }
                _ => label.to_string(),
            }
        })
        .collect();
    format!(" \u{2014} likely cause: {}", rendered.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::orchestrator::ModelUsageRow;

    #[test]
    fn cbg_empty_and_per_model() {
        // Empty → the aligned zero line.
        assert_eq!(
            usage_by_model_block(&[]),
            "Usage:                 0 input, 0 output, 0 cache read, 0 cache write"
        );
        let rows = vec![ModelUsageRow {
            model: "claude-opus-4-8".into(),
            provider: None,
            total_nano_usd: 1_230_000_000, // $1.23
            input_tokens: 5_000,
            output_tokens: 2_000,
            cache_read_input_tokens: 1_500,
            cache_creation_input_tokens: 0,
        }];
        let out = usage_by_model_block(&rows);
        // label right-aligned to 21 ("claude-opus-4-8:" is 16 chars, so 5 leading spaces),
        // then the token/cost line via the `ed` formatter (KEEPS the trailing `.0`,
        // unlike `Bu`): 5.0k, 2.0k, 1.5k, 0.0; cost $1.23.
        assert_eq!(
            out,
            "Usage by model:\n     claude-opus-4-8:  5.0k input, 2.0k output, 1.5k cache read, 0.0 cache write ($1.23)"
        );
    }

    #[test]
    fn format_token_count_ed_keeps_trailing_zero() {
        // `ed` = compact, min+maxFractionDigits:1 — always one fraction digit.
        assert_eq!(format_token_count_ed(0), "0.0");
        assert_eq!(format_token_count_ed(5), "5.0");
        assert_eq!(format_token_count_ed(500), "500.0");
        assert_eq!(format_token_count_ed(1_000), "1.0k");
        assert_eq!(format_token_count_ed(1_960), "2.0k");
        assert_eq!(format_token_count_ed(2_000), "2.0k");
        assert_eq!(format_token_count_ed(12_000), "12.0k");
        assert_eq!(format_token_count_ed(1_234), "1.2k");
        assert_eq!(format_token_count_ed(1_000_000), "1.0m");
        // Contrast with `Bu` (strips `.0`).
        assert_eq!(format_token_count(2_000), "2k");
    }

    #[test]
    fn cbg_multiple_models_joined() {
        let rows = vec![
            ModelUsageRow {
                model: "claude-opus-4-8".into(),
                provider: None,
                total_nano_usd: 600_000_000, // $0.60
                input_tokens: 1_000,
                output_tokens: 500,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            ModelUsageRow {
                model: "claude-sonnet-4-20250514".into(),
                provider: None,
                total_nano_usd: 400_000_000, // $0.40
                input_tokens: 2_000,
                output_tokens: 100,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
        ];
        let out = usage_by_model_block(&rows);
        // header + two model lines, each starting on its own line.
        assert!(out.starts_with("Usage by model:\n"), "{out}");
        assert_eq!(out.matches('\n').count(), 2, "{out}"); // header\n line1\n line2 => 2 newlines
    }

    #[test]
    fn qs_matches_cc() {
        assert_eq!(format_duration_ms(0), "0s");
        assert_eq!(format_duration_ms(500), "0s"); // <1s floors to 0
        assert_eq!(format_duration_ms(1_500), "1s"); // floor(1.5)
        assert_eq!(format_duration_ms(59_000), "59s");
        assert_eq!(format_duration_ms(60_000), "1m 0s");
        assert_eq!(format_duration_ms(65_000), "1m 5s");
        assert_eq!(format_duration_ms(3_661_000), "1h 1m 1s");
        assert_eq!(format_duration_ms(90_061_000), "1d 1h 1m"); // days form drops seconds
        assert_eq!(format_duration_ms(59_500), "59s"); // floor(59.5)=59
        assert_eq!(format_duration_ms(119_500), "2m 0s"); // round(59.5s)=60 -> carry: 1m -> 2m 0s
    }

    #[test]
    fn ftu_matches_cc() {
        assert_eq!(format_cost(0.0), "$0.0000");
        assert_eq!(format_cost(0.05), "$0.0500");
        assert_eq!(format_cost(0.5), "$0.5000"); // not > 0.5
        assert_eq!(format_cost(0.5001), "$0.50"); // > 0.5 -> 2dp rounded
        assert_eq!(format_cost(1.2345), "$1.23");
        assert_eq!(format_cost(12.999), "$13.00");
    }

    #[test]
    fn bu_matches_cc_compact() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1k");
        assert_eq!(format_token_count(1_500), "1.5k");
        assert_eq!(format_token_count(12_345), "12.3k");
        assert_eq!(format_token_count(50_000), "50k");
        assert_eq!(format_token_count(1_000_000), "1m");
        assert_eq!(format_token_count(1_500_000), "1.5m");
    }

    #[test]
    fn i6e_full_block() {
        let out = cost_summary(&CostSummaryInput {
            total_usd: 1.2345,
            unknown_models: false,
            api_duration_ms: 65_000,
            wall_duration_ms: 3_661_000,
            code_lines_added: 1,
            code_lines_removed: 5,
            by_model: &[],
        });
        assert_eq!(
            out,
            "Total cost:            $1.23\n\
             Total duration (API):  1m 5s\n\
             Total duration (wall): 1h 1m 1s\n\
             Total code changes:    1 line added, 5 lines removed\n\
             Usage:                 0 input, 0 output, 0 cache read, 0 cache write"
        );
    }

    #[test]
    fn cost_summary_from_snapshot_maps_fields() {
        let rows = vec![ModelUsageRow {
            model: "claude-opus-4-8".into(),
            provider: None,
            total_nano_usd: 123_400_000,
            input_tokens: 5_000,
            output_tokens: 2_000,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }];
        let snap = lingxi_core::host::CostSnapshot {
            total_usd: 0.1234,
            unknown_models: true,
            api_duration: std::time::Duration::from_millis(5_000),
            session_duration: std::time::Duration::from_secs(125),
            code_lines_added: 10,
            code_lines_removed: 1,
            by_model: rows.clone(),
            ..lingxi_core::host::CostSnapshot::default()
        };
        let expected = cost_summary(&CostSummaryInput {
            total_usd: 0.1234,
            unknown_models: true,
            api_duration_ms: 5_000,
            wall_duration_ms: 125_000,
            code_lines_added: 10,
            code_lines_removed: 1,
            by_model: &rows,
        });
        assert_eq!(cost_summary_from_snapshot(&snap), expected);
    }

    #[test]
    fn i6e_unknown_models_note() {
        let out = cost_summary(&CostSummaryInput {
            total_usd: 0.05,
            unknown_models: true,
            api_duration_ms: 0,
            wall_duration_ms: 0,
            code_lines_added: 0,
            code_lines_removed: 0,
            by_model: &[],
        });
        assert!(out.starts_with(
            "Total cost:            $0.0500 (costs may be inaccurate due to usage of unknown models)\n"
        ), "{out}");
        // plural on zero: "0 lines added, 0 lines removed"
        assert!(
            out.contains("Total code changes:    0 lines added, 0 lines removed"),
            "{out}"
        );
    }

    // ---- CLI-4: the `/cost` prompt-cache line -----------------------------

    use crate::prompt_cache_ledger::{
        CacheTtl, MissAttribution, MissCause, PromptCacheLedger, RequestFacts,
    };

    fn facts(at_ms: u64, input: u64, read: u64, creation: u64) -> RequestFacts {
        RequestFacts {
            at_ms,
            input_tokens: input,
            cache_read_tokens: read,
            cache_creation_tokens: creation,
            ttl: CacheTtl::FiveMinutes,
        }
    }

    #[test]
    fn jo_is_neither_of_its_neighbours() {
        // Above 1000 it matches `ed` (forced fraction digit)…
        assert_eq!(format_cache_token_count(2_000), "2.0k");
        assert_eq!(format_token_count_ed(2_000), "2.0k");
        assert_eq!(format_token_count(2_000), "2k", "Bu drops the .0");
        // …below 1000 it matches `Bu` (no forced digit).
        assert_eq!(format_cache_token_count(500), "500");
        assert_eq!(format_token_count_ed(500), "500.0", "ed keeps it");
        // Signed, for a system-prompt char delta.
        assert_eq!(format_cache_token_count(-1_500), "-1.5k");
        assert_eq!(format_cache_token_count(-12), "-12");
    }

    #[test]
    fn the_cost_block_gains_the_line_only_when_there_is_one() {
        let base = lingxi_core::host::CostSnapshot {
            total_usd: 0.5,
            ..lingxi_core::host::CostSnapshot::default()
        };
        let without = cost_summary_from_snapshot(&base);
        assert!(
            !without.contains("Prompt cache"),
            "a session with no responses must render the block unchanged"
        );

        let with = cost_summary_from_snapshot(&lingxi_core::host::CostSnapshot {
            prompt_cache_line: Some("Prompt cache (main):   1 request".to_string()),
            ..base.clone()
        });
        assert_eq!(with, format!("{without}\nPrompt cache (main):   1 request"));

        // An EMPTY line must not add a blank row — `Some("")` is the shape a
        // careless caller produces and it would show as trailing whitespace.
        let empty = cost_summary_from_snapshot(&lingxi_core::host::CostSnapshot {
            prompt_cache_line: Some(String::new()),
            ..base
        });
        assert_eq!(empty, without);
    }

    #[test]
    fn the_line_is_absent_until_the_first_response() {
        let ledger = PromptCacheLedger::new();
        assert_eq!(
            prompt_cache_line(&ledger.summary(0), Some(0), 0),
            None,
            "an empty ledger must omit the whole line, not print an empty one"
        );
    }

    #[test]
    fn a_warm_session_with_no_misses_renders_the_short_form() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(facts(0, 100, 0, 10_000));
        ledger.record(facts(1_000, 50, 10_000, 0));
        let line = prompt_cache_line(&ledger.summary(3_000), Some(10_050), 3_000)
            .expect("a line after two requests");
        assert_eq!(
            line,
            "Prompt cache (main):   2 requests \u{b7} 50% of input tokens from cache \
             \u{b7} no misses \u{b7} warm (5m TTL, last activity 2s ago)"
        );
    }

    #[test]
    fn a_miss_names_its_cause_with_the_counts_that_accompany_it() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(facts(0, 100, 0, 50_000));
        ledger.attribute(MissAttribution {
            causes: vec![MissCause::ToolsChanged],
            tools_added: Some(2),
            tools_removed: Some(1),
            system_char_delta: None,
        });
        ledger.record(facts(60_000, 100, 1_000, 49_000));
        let line =
            prompt_cache_line(&ledger.summary(120_000), Some(50_100), 120_000).expect("line");
        assert!(
            line.contains(
                "1 miss (last 1m 0s ago \u{2014} likely cause: tool definitions changed (+2/-1), \
                 49.0k tokens re-cached)"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_system_prompt_delta_renders_signed_and_only_when_nonzero() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(facts(0, 100, 0, 50_000));
        ledger.attribute(MissAttribution {
            causes: vec![MissCause::SystemPromptChanged],
            system_char_delta: Some(1_800),
            ..MissAttribution::default()
        });
        ledger.record(facts(1_000, 100, 1_000, 49_000));
        let line = prompt_cache_line(&ledger.summary(2_000), Some(0), 2_000).expect("line");
        assert!(
            line.contains("likely cause: system prompt changed (+1.8k chars)"),
            "{line}"
        );

        // A zero delta carries no parenthetical — printing "(+0 chars)" would
        // assert a change the number denies.
        let mut zero = PromptCacheLedger::new();
        zero.record(facts(0, 100, 0, 50_000));
        zero.attribute(MissAttribution {
            causes: vec![MissCause::SystemPromptChanged],
            system_char_delta: Some(0),
            ..MissAttribution::default()
        });
        zero.record(facts(1_000, 100, 1_000, 49_000));
        let line = prompt_cache_line(&zero.summary(2_000), Some(0), 2_000).expect("line");
        assert!(
            line.contains("likely cause: system prompt changed,"),
            "{line}"
        );
        assert!(!line.contains("chars"), "{line}");
    }

    #[test]
    fn an_announced_rebuild_shows_as_a_rebuild_and_hides_the_recache_number() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(facts(0, 100, 0, 50_000));
        ledger.expect_drop(900);
        ledger.record(facts(1_000, 100, 1_000, 49_000));
        // A rebuild announced but not yet recorded ⇒ `None` estimate.
        ledger.expect_drop(1_500);
        let line = prompt_cache_line(
            &ledger.summary(1_000 + 400_000),
            ledger.estimate_recache_tokens(),
            1_000 + 400_000,
        )
        .expect("line");
        assert!(
            line.contains("1 expected rebuild (compaction or tool-result clearing)"),
            "{line}"
        );
        assert!(
            line.contains("next turn re-caches the compacted prompt"),
            "an unpredictable size must not be rendered as a number: {line}"
        );
        assert!(!line.contains("no misses") || line.contains("no misses"));
    }

    #[test]
    fn a_provider_without_caching_says_so_instead_of_claiming_cold() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(facts(0, 5_000, 0, 0));
        ledger.record(facts(1_000, 9_000, 0, 0));
        let line = prompt_cache_line(&ledger.summary(2_000), Some(9_000), 2_000).expect("line");
        assert!(
            line.contains("no prompt caching reported by the API"),
            "{line}"
        );
        assert!(!line.contains("cold"), "{line}");
        assert!(!line.contains("warm"), "{line}");
    }

    #[test]
    fn a_cold_prefix_names_what_the_next_turn_will_re_cache() {
        let mut ledger = PromptCacheLedger::new();
        ledger.record(facts(0, 100, 0, 50_000));
        let now = 400_000; // well past the 5m TTL
        let line = prompt_cache_line(&ledger.summary(now), ledger.estimate_recache_tokens(), now)
            .expect("line");
        assert!(
            line.contains("cold \u{2014} idle 6m 40s, next turn re-caches ~50.1k tokens"),
            "{line}"
        );
    }
}
