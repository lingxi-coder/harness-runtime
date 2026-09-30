//! 4-tier memory subsystem for `LingXi` Core.
//!
//! v0.4.0 (M3-02) extends the v0.3.0 scaffold with the production
//! `lingxi_md` hierarchy loader, `memdir` scanner + fixed-point u64
//! ranker, real `team_paths` resolution, and a thin secret-scan adapter
//! over the v3 §16.5 `lingxi-secret` rule set.

#![forbid(unsafe_code)]

pub mod file;
pub mod gate;
pub mod index_cap;
pub mod lingxi_md;
pub mod memdir;
pub mod prefetch;
pub mod retention;
pub mod secret_scan;
pub mod selector;
pub mod session_memory;
pub mod snapshot;
pub mod surfacing;
pub mod team_memory;

pub use file::{
    parse_markdown_with_frontmatter, MemoryError, MemoryFile, MemoryFrontmatter,
    MAX_ENTRYPOINT_BYTES, MAX_ENTRYPOINT_LINES,
};
pub use gate::{auto_memory_enabled, AutoMemoryEnv};
pub use index_cap::{
    human_bytes, measure, memory_index_cap_notice, memory_index_cap_notice_measured, IndexMeasure,
    MemoryIndexNotice, TENGU_MEMDIR_ENTRYPOINT_NEAR_CAP,
};

// ------ M3-02 wire-identifier constants ------

/// Per-file cap (10 MB) for the **memdir** scanner.
///
/// ⚠️ Both halves of the old note were wrong. It said files over this cap are
/// "skipped with `tengu_memory_file_too_large`" — that event does not exist in
/// the 2.1.220 binary (0 occurrences) and its emitter has no production caller;
/// see [`lingxi_md::loader::TENGU_MEMORY_FILE_TOO_LARGE`]. And it said the
/// LINGXI.md hierarchy loader "reads every file whole (no size drop — parity
/// with claude-code `safelyReadMemoryFileAsync`)", citing leaked TS; the binary
/// skips at [`lingxi_md::loader::MEMORY_FILE_BYTE_LIMIT`] (4 MiB), which is
/// SMALLER than this constant.
///
/// This value is a LingXi memdir-scanner cap with no oracle counterpart, kept
/// because memdir depends on it. See [`get_large_memory_files`] for the
/// non-blocking 40k-char *warning* the LINGXI.md path surfaces on top of the
/// 4 MiB skip.
pub const MAX_MEMORY_FILE_SIZE: usize = 10 * 1024 * 1024;

/// FLOOR of the per-file memory-size warning threshold — claude-code `gn_`
/// (2.1.220 binary offset 230811938, `…,hn_=0.05,ELu=4194304,gn_=40000,…`).
///
/// The live threshold is NOT this constant: it is
/// [`max_memory_character_count`], i.e. `Math.max(gn_, round(window * hn_ *
/// charsPerToken))`. For every 200k-context model that product is 30k–40k, so
/// the floor wins and the effective limit IS 40 000 — which is why a flat
/// constant matched for so long. It diverges only for 1M-context models
/// (150 000 at 3 chars/token, 200 000 at 4).
///
/// This is a soft, non-blocking recommendation: files over this size are still
/// loaded in full. [`get_large_memory_files`] flags them so a caller can warn
/// the user, exactly as claude-code does — it never drops the file.
pub const MAX_MEMORY_CHARACTER_COUNT: usize = 40_000;

/// claude-code `hn_` = 0.05 (2.1.220 binary offset 230811917) — the fraction of
/// the context window a single memory file may occupy before it is flagged.
///
/// Stored in **basis points** so the whole threshold computation stays in
/// integer arithmetic. `Math.round(window * 0.05 * cpt)` is exactly
/// `(window * 500 * cpt + 5_000) / 10_000` under truncating integer division
/// for non-negative operands: adding half the divisor before dividing is
/// half-up rounding, and `Math.round` is half-up for non-negative inputs.
pub const MEMORY_CONTEXT_FRACTION_BPS: u64 = 500;

/// claude-code `_er` = 200000 (2.1.220 binary offset 228835855) — the context
/// window `pJr` falls back to when `JE(model, betas)` is not a finite positive
/// number (`Number.isFinite(t)&&t>0?t:_er`).
pub const DEFAULT_MEMORY_CONTEXT_WINDOW: u64 = 200_000;

/// claude-code `isg` (2.1.220 binary offset 227936079) — the model families
/// whose token estimate is 4 characters per token. Everything else is 3.
///
/// Transcribed verbatim, in binary order:
/// `["claude-3-opus","claude-3-sonnet","claude-3-haiku","claude-3-5-sonnet",
/// "claude-3-5-haiku","claude-3-7-sonnet","claude-opus-4-0","claude-opus-4-1",
/// "claude-opus-4-5","claude-opus-4-6","claude-sonnet-4-0","claude-sonnet-4-5",
/// "claude-sonnet-4-6","claude-haiku-4-5"]`.
const ISG_FOUR_CHARS_PER_TOKEN: [&str; 14] = [
    "claude-3-opus",
    "claude-3-sonnet",
    "claude-3-haiku",
    "claude-3-5-sonnet",
    "claude-3-5-haiku",
    "claude-3-7-sonnet",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-sonnet-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-6",
    "claude-haiku-4-5",
];

/// `/prefix(?!-\d(?!\d))/` — does `haystack` contain `prefix` NOT followed by a
/// dash and a single (non-multi-digit) minor version?
///
/// claude-code spells the bare-major fallbacks in `JM` as
/// `/claude-opus-4(?!-\d(?!\d))/` and `/claude-sonnet-4(?!-\d(?!\d))/`, which
/// lets `claude-opus-4-20250514` (dash, `2`, then another digit) fall through to
/// `claude-opus-4-0` while `claude-opus-4-8` does not.
fn contains_bare_major(haystack: &str, prefix: &str) -> bool {
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(prefix) {
        let at = from + idx;
        let rest = &haystack.as_bytes()[at + prefix.len()..];
        // `-\d(?!\d)` matched ⇒ the negative lookahead fails at this position.
        let blocked = rest.len() >= 2
            && rest[0] == b'-'
            && rest[1].is_ascii_digit()
            && (rest.len() < 3 || !rest[2].is_ascii_digit());
        if !blocked {
            return true;
        }
        from = at + 1;
    }
    false
}

/// `Qs(lo(model))` — canonicalize a model id to its family before the `isg`
/// membership test.
///
/// `lo` (2.1.220 binary offset 227931611) delegates to `JM` (@227930206), which
/// lowercases, tries an exact registry-alias lookup (`LFr`, @225789143 — a map
/// built from the live model registry that LingXi's `memory` crate has no
/// access to and that only ever produces one of the same canonical families),
/// then walks the `includes` cascade below, and finally strips a trailing
/// `-YYYYMMDD` date. `Qs` (@227418303) then strips a trailing `[1m]`.
///
/// The cascade is written out in the binary's exact order, INCLUDING the
/// positions of the two `/…(?!-\d(?!\d))/` bare-major fallbacks. Order is
/// load-bearing: `claude-opus-4-8` must be tested before the bare
/// `claude-opus-4` arm, or an Opus 4.8 id would canonicalize to
/// `claude-opus-4-0` and wrongly pick up 4 chars/token.
fn canonical_model_family(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    // `if(e.includes(X))return X` — the plain-substring arms, in binary order.
    for family in [
        "claude-fable-5-1",
        "claude-mythos-5-1",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-opus-4-1",
    ] {
        if lower.contains(family) {
            return family.to_string();
        }
    }
    // `if(/claude-opus-4(?!-\d(?!\d))/.test(e))return"claude-opus-4-0"`
    if contains_bare_major(&lower, "claude-opus-4") {
        return "claude-opus-4-0".to_string();
    }
    for family in ["claude-sonnet-5", "claude-sonnet-4-6", "claude-sonnet-4-5"] {
        if lower.contains(family) {
            return family.to_string();
        }
    }
    // `if(/claude-sonnet-4(?!-\d(?!\d))/.test(e))return"claude-sonnet-4-0"`
    if contains_bare_major(&lower, "claude-sonnet-4") {
        return "claude-sonnet-4-0".to_string();
    }
    for family in [
        "claude-haiku-4-5",
        "claude-3-7-sonnet",
        "claude-3-5-sonnet",
        "claude-3-5-haiku",
        "claude-3-opus",
        "claude-3-sonnet",
        "claude-3-haiku",
    ] {
        if lower.contains(family) {
            return family.to_string();
        }
    }
    // `e.replace(/-\d{8}$/,"")` — a trailing dash + exactly 8 digits.
    let stripped = match lower.rfind('-') {
        Some(i) if lower.len() - i == 9 && lower[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            &lower[..i]
        }
        _ => lower.as_str(),
    };
    // `Qs(e) = e.replace(/\[1m\]$/i,"")` — applied AFTER `lo`.
    stripped
        .strip_suffix("[1m]")
        .unwrap_or(stripped)
        .to_string()
}

/// claude-code `kC(model)` (2.1.220 binary offset 227931911):
/// `if(!e)return 4; let t=Ei(e),r=Qs(lo(t)).replace(/[._]/g,"-"); return
/// isg.has(r)?4:3`.
///
/// SCOPE: this estimator is used ONLY by the memory-file size warning
/// ([`max_memory_character_count`]). LingXi's compaction / context-meter
/// estimators keep their own flat 4 chars-per-token; unifying them would move
/// autocompact, PTL blocking and microcompact arming all at once and is a
/// deliberately separate change.
///
/// The alias resolution `Ei` (@227933508 — `opus`/`sonnet`/`haiku`/`fable`/
/// `best` → the configured concrete id) is the CALLER's job: pass a concrete
/// model id, not a picker alias.
#[must_use]
pub fn memory_chars_per_token(model: &str) -> u64 {
    // `if(!e)return 4` — an unset model.
    if model.is_empty() {
        return 4;
    }
    let canonical = canonical_model_family(model).replace(['.', '_'], "-");
    // MULTI-PROVIDER GATE (a LingXi divergence, not a port of `kC`).
    //
    // `isg.has(r) ? 4 : 3` is a two-way split over CLAUDE families: its `else`
    // arm asserts "a Claude model that is not in the older 4-chars list".
    // claude-code is Anthropic-only and never meets a third case. LingXi is
    // multi-provider, so a foreign id reaches here, matches no family, and
    // would otherwise inherit a claim about Claude tokenizers that cannot be
    // made about GPT/Kimi/GLM — shrinking `max_memory_character_count` by 25%
    // for every non-Anthropic model and firing the large-file warning early.
    //
    // 4 is claude-code's OWN answer when it cannot identify the model
    // (`if(!e)return 4`), which is exactly the epistemic position here. Note
    // `canonical_model_family` returns the lowercased id unchanged when it
    // matches nothing, so a provider-qualified Claude id (`bedrock/claude-…`,
    // `us.anthropic.claude-…`) still contains "claude" and is unaffected.
    if !canonical.contains("claude") {
        return 4;
    }
    if ISG_FOUR_CHARS_PER_TOKEN.contains(&canonical.as_str()) {
        4
    } else {
        3
    }
}

/// claude-code `pJr()` (2.1.220 binary offset 230802563):
/// `let t=JE(e,Mv()),r=Number.isFinite(t)&&t>0?t:_er; return
/// Math.max(gn_, Math.round(r*hn_*kC(e)))`.
///
/// `context_window` is the effective window for the model+betas (LingXi's
/// `llm_runtime::model::context_window_for_model`, the `JE`/`SZc` analog); `0`
/// stands for "unknown" and takes the [`DEFAULT_MEMORY_CONTEXT_WINDOW`]
/// fallback. `chars_per_token` comes from [`memory_chars_per_token`].
#[must_use]
pub fn max_memory_character_count(context_window: u64, chars_per_token: u64) -> usize {
    let window = if context_window > 0 {
        context_window
    } else {
        DEFAULT_MEMORY_CONTEXT_WINDOW
    };
    // `Math.round(window * 0.05 * cpt)` in integer arithmetic — see
    // [`MEMORY_CONTEXT_FRACTION_BPS`] for the identity.
    let scaled =
        u128::from(window) * u128::from(MEMORY_CONTEXT_FRACTION_BPS) * u128::from(chars_per_token);
    let rounded = usize::try_from((scaled + 5_000) / 10_000).unwrap_or(usize::MAX);
    rounded.max(MAX_MEMORY_CHARACTER_COUNT)
}

/// JS `String.prototype.length` — the count of UTF-16 code units, NOT of
/// Unicode scalar values.
///
/// claude-code compares `r.content.length > t` (`fJr`, 2.1.220 binary offset
/// 230809207). A `chars().count()` would under-count every astral character by
/// one, so a CJK-plus-emoji memory file could sit just under the port's
/// threshold while the oracle flags it.
#[must_use]
pub fn memory_file_char_count(content: &str) -> usize {
    content.chars().map(char::len_utf16).sum()
}

/// claude-code `yd(e)` (2.1.220 binary offset 229070611):
/// `fOg(e>=1000).format(e).toLowerCase()`, where `fOg` (@229072804) is
/// `Intl.NumberFormat("en-US",{notation:"compact",maximumFractionDigits:1,
/// minimumFractionDigits: e>=1000 ? 1 : 0})`.
///
/// NOTE this is `yd`, NOT its sibling `wa(e){return yd(e).replace(".0","")}` —
/// the trailing `.0` is KEPT: `40000 → "40.0k"`, `150000 → "150.0k"`.
/// Sub-1000 values render as plain integers (`900 → "900"`). Intl's default
/// rounding mode is `halfExpand`, which for non-negative inputs is half-up.
///
/// ⚠️ The unit must be re-selected AFTER rounding. ICU picks the compact
/// multiplier from the ROUNDED value, so `999_950` formats as `1.0m`, not
/// `1000.0k`. Rounding inside the pre-selected unit and stopping there is the
/// classic carry bug; it can only ever promote by one decade (the rounded
/// mantissa is at most 1000.0), so one re-check per unit suffices.
#[must_use]
pub fn format_compact_count(value: u64) -> String {
    // en-US compact short units, largest first.
    const UNITS: [(u64, char); 4] = [
        (1_000_000_000_000, 't'),
        (1_000_000_000, 'b'),
        (1_000_000, 'm'),
        (1_000, 'k'),
    ];
    for (idx, (divisor, suffix)) in UNITS.into_iter().enumerate() {
        if value >= divisor {
            let d = u128::from(divisor);
            let mut tenths = (u128::from(value) * 10 + d / 2) / d;
            let mut suffix = suffix;
            // Rounding carried the mantissa to 1000.0 of this unit — ICU would
            // have bucketed one decade up instead. `t` is the largest unit, so
            // it has nowhere to promote to and keeps the carried mantissa.
            if tenths >= 10_000 && idx > 0 {
                tenths /= 1_000;
                suffix = UNITS[idx - 1].1;
            }
            return format!("{}.{}{suffix}", tenths / 10, tenths % 10);
        }
    }
    value.to_string()
}

/// claude-code `htf()` (2.1.220 binary offset 241152161) — the `/status`
/// warning row for one oversized memory file:
/// ``r.push(`Large ${Ad(o.path)} will impact performance (${yd(o.content.length)} chars > ${yd(n)})`)``.
///
/// `path_display` is the caller's already-shortened path (`Ad`, @226626865:
/// cwd-relative when the file is under cwd, else `~`-abbreviated when under
/// `$HOME`, else absolute).
#[must_use]
pub fn format_large_memory_file_status_row(
    path_display: &str,
    chars: u64,
    max_chars: u64,
) -> String {
    format!(
        "Large {path_display} will impact performance ({} chars > {})",
        format_compact_count(chars),
        format_compact_count(max_chars)
    )
}

/// Return the subset of `files` whose body exceeds `max_chars` UTF-16 code
/// units.
///
/// 1:1 with claude-code `fJr` (2.1.220 binary offset 230809207):
/// `e.filter((r)=>!tHt(r.path)&&LLu(r.type)&&r.content.length>t)`. This is a
/// **warning** list — the returned files are NOT removed from the memory set;
/// every file is still loaded whole.
///
/// The two oracle-side filters are structurally satisfied here rather than
/// re-implemented:
/// * `tHt(path)` (@230802523) skips the two SYNTHETIC managed paths the oracle
///   fabricates for policy-provided memory text. LingXi's managed tier is
///   always a real on-disk file, so there is no synthetic path to skip.
/// * `LLu(type)` (@230808926) admits exactly `User | Project | Local | Managed`
///   — i.e. it excludes `AutoMem`/`AutoMemPinned`.
///   [`lingxi_md::LingxiMdTier`] has precisely those four variants and no
///   auto-memory variant (LingXi's auto-memory lives in the separate `memdir` /
///   `session_memory` subsystems, which never reach this list), so the tier
///   filter admits every element of the LINGXI.md hierarchy.
///
/// `max_chars` comes from [`max_memory_character_count`]; pass
/// [`MAX_MEMORY_CHARACTER_COUNT`] for the 200k-context default.
#[must_use]
pub fn get_large_memory_files(files: &[MemoryFile], max_chars: usize) -> Vec<&MemoryFile> {
    files
        .iter()
        .filter(|f| memory_file_char_count(&f.content) > max_chars)
        .collect()
}

/// Age penalty unit in days. `age_blocks = age_days / 30`.
pub const MEMORY_AGE_PENALTY_DAYS: u64 = 30;

/// Hard-drop threshold (365 days). Scan-time hygiene; the only drop in
/// the loader path. Age otherwise penalizes, never drops.
pub const MEMORY_AGE_HARD_DROP_DAYS: u64 = 365;

/// Floor on age weight (bps). Very old entries still reachable at 10%.
pub const MEMORY_MIN_AGE_WEIGHT_BPS: u32 = 1_000;

/// Default top-k for `find_relevant`. Caller-overridable.
pub const DEFAULT_RELEVANT_MEMORIES: usize = 5;

#[cfg(test)]
mod large_memory_file_tests {
    use super::*;

    #[test]
    fn max_memory_character_count_is_five_percent_of_context_in_chars_with_a_40k_floor() {
        // `pJr` (2.1.220 binary @230802563):
        //   function pJr(e=Mi()){ let t=JE(e,Mv()),
        //     r=Number.isFinite(t)&&t>0?t:_er;
        //     return Math.max(gn_, Math.round(r*hn_*kC(e))) }
        // with hn_=0.05 (@230811917), gn_=40000 (@230811938), _er=200000.
        assert_eq!(max_memory_character_count(200_000, 3), 40_000); // 30_000 → floor
        assert_eq!(max_memory_character_count(200_000, 4), 40_000); // exactly the floor
        assert_eq!(max_memory_character_count(1_000_000, 3), 150_000);
        assert_eq!(max_memory_character_count(1_000_000, 4), 200_000);
        // `Number.isFinite(t)&&t>0?t:_er` — a 0/unknown window falls back to 200k.
        assert_eq!(max_memory_character_count(0, 3), 40_000);
        // Half-up rounding (`Math.round`): 100_003 * 0.05 * 3 = 15000.45 → 15000,
        // 100_007 * 0.05 * 3 = 15001.05 → 15001 (both under the floor, so probe
        // the raw product through a window large enough to clear it).
        assert_eq!(max_memory_character_count(1_000_003, 3), 150_000); // 150000.45
        assert_eq!(max_memory_character_count(1_000_007, 3), 150_001); // 150001.05
    }

    #[test]
    fn chars_per_token_is_4_for_the_isg_set_and_3_otherwise() {
        // `kC` (@227931911): `if(!e)return 4; … isg.has(canonical)?4:3`.
        // `isg` (@227936079) — the exact 14-element set.
        for m in [
            "claude-3-opus",
            "claude-3-sonnet",
            "claude-3-haiku",
            "claude-3-5-sonnet",
            "claude-3-5-haiku",
            "claude-3-7-sonnet",
            "claude-opus-4-0",
            "claude-opus-4-1",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-sonnet-4-0",
            "claude-sonnet-4-5",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
        ] {
            assert_eq!(memory_chars_per_token(m), 4, "{m} is in isg");
        }
        // Dated / vendor-prefixed ids canonicalize into the same families.
        assert_eq!(memory_chars_per_token("claude-opus-4-1-20250805"), 4);
        // `/claude-opus-4(?!-\d(?!\d))/` → "claude-opus-4-0": the dated Opus 4
        // id (`ssg[0]`, @227936079) has "-2" followed by another digit, so the
        // inner `(?!\d)` fails and the outer lookahead succeeds.
        assert_eq!(memory_chars_per_token("claude-opus-4-20250514"), 4);
        assert_eq!(memory_chars_per_token("claude-sonnet-4-20250514"), 4);
        // …but a single-digit minor version IS blocked by that lookahead and
        // must NOT collapse onto claude-opus-4-0. `claude-opus-4-9` is not in
        // the `JM` cascade at all, so ONLY the lookahead keeps it off the
        // 4-chars-per-token branch.
        assert_eq!(memory_chars_per_token("claude-opus-4-9"), 3);
        assert_eq!(memory_chars_per_token("claude-sonnet-4-9"), 3);
        // The bare major with no minor at all DOES take the fallback.
        assert_eq!(memory_chars_per_token("claude-opus-4"), 4);
        assert_eq!(memory_chars_per_token("claude-sonnet-4"), 4);
        assert_eq!(
            memory_chars_per_token("us.anthropic.claude-opus-4-5-v1:0"),
            4
        );
        assert_eq!(memory_chars_per_token("claude-3-5-haiku-20241022"), 4);
        // NOT in isg (present in `asg` but that is a different set) → 3.
        assert_eq!(memory_chars_per_token("claude-opus-4-7"), 3);
        assert_eq!(memory_chars_per_token("claude-opus-4-8"), 3);
        assert_eq!(memory_chars_per_token("claude-opus-5"), 3);
    }

    /// A NON-ANTHROPIC model takes the unset-model default of 4, not the
    /// newer-Claude-family 3.
    ///
    /// `kC`'s `isg.has(r) ? 4 : 3` is a two-way split over CLAUDE families: the
    /// `else` arm asserts "a Claude model that is not in the older 4-chars
    /// list". claude-code is Anthropic-only, so it never meets a third case.
    /// LingXi is multi-provider (a user-confirmed accepted divergence), so a
    /// foreign id reaches this function, matches no family, and would silently
    /// inherit an assertion about Claude tokenizers that cannot be made about
    /// GPT/Kimi/GLM. That would shrink `max_memory_character_count` by 25% for
    /// every non-Anthropic model and fire the large-file warning early.
    ///
    /// 4 is claude-code's own answer when it cannot identify the model
    /// (`if(!e)return 4`), which is exactly the epistemic position here.
    #[test]
    fn non_anthropic_models_take_the_unknown_model_default() {
        for m in [
            "gpt-5.4",
            "gpt-4o",
            "kimi-k2",
            "glm-4.6",
            "gemini-2.5-pro",
            "deepseek-chat",
            "llama-3.3-70b",
        ] {
            assert_eq!(
                memory_chars_per_token(m),
                4,
                "{m} is not a Claude model — the isg/else split does not apply"
            );
        }
        // A provider-qualified Claude id must be unaffected by the gate: it
        // still resolves through the Claude path, whatever that path decides.
        // Asserted as an EQUIVALENCE rather than a literal so this pins the
        // gate's scope without also re-asserting the family cascade.
        for (qualified, bare) in [
            ("us.anthropic.claude-opus-4-8-v1:0", "claude-opus-4-8"),
            ("bedrock/claude-opus-4-1", "claude-opus-4-1"),
            ("vertex/claude-sonnet-4-5", "claude-sonnet-4-5"),
        ] {
            assert_eq!(
                memory_chars_per_token(qualified),
                memory_chars_per_token(bare),
                "{qualified} must resolve exactly as {bare} — the gate must not \
                 catch a provider-qualified Claude id"
            );
        }
        assert_eq!(memory_chars_per_token("claude-sonnet-5"), 3);
        // INTENTIONAL DIVERGENCE from `kC`, flipped from 3 to 4.
        //
        // `kC` is a pure two-way split and does give 3 for "gpt-5.5", so the
        // old assertion was a faithful port — this is not a corrected bug, it
        // is LingXi choosing different behavior, and it is recorded as such.
        //
        // The oracle is Anthropic-only, so its `else` arm can only ever mean
        // "a Claude family not in the older 4-chars list". LingXi is
        // multi-provider (a user-confirmed accepted divergence), so that arm
        // would silently apply a Claude-tokenizer claim to GPT/Kimi/GLM and
        // shrink the memory-file threshold by 25% for all of them. See the
        // gate in `memory_chars_per_token` and the dedicated test below.
        assert_eq!(memory_chars_per_token("gpt-5.5"), 4);
        // `if(!e)return 4` — the empty/unset model.
        assert_eq!(memory_chars_per_token(""), 4);
    }

    #[test]
    fn compact_count_matches_intl_compact_notation() {
        // `yd(e)` (@229070611): `fOg(e>=1000).format(e).toLowerCase()` where
        // `fOg` (@229072804) is Intl.NumberFormat("en-US", {notation:"compact",
        // maximumFractionDigits:1, minimumFractionDigits: n>=1000 ? 1 : 0}).
        // NOTE this is `yd`, NOT `wa` — the trailing `.0` is KEPT.
        assert_eq!(format_compact_count(40_000), "40.0k");
        assert_eq!(format_compact_count(52_310), "52.3k");
        assert_eq!(format_compact_count(150_000), "150.0k");
        assert_eq!(format_compact_count(900), "900");
        assert_eq!(format_compact_count(0), "0");
        assert_eq!(format_compact_count(999), "999");
        assert_eq!(format_compact_count(1_000), "1.0k");
        assert_eq!(format_compact_count(1_500_000), "1.5m");
        assert_eq!(format_compact_count(2_000_000_000), "2.0b");
    }

    /// ICU selects the compact multiplier from the ROUNDED value, so the top
    /// 50 units of every decade carry into the next unit. Verified against
    /// `Intl.NumberFormat("en-US",{notation:"compact",maximumFractionDigits:1,
    /// minimumFractionDigits:1})` in node.
    #[test]
    fn compact_count_reselects_the_unit_after_rounding_carries() {
        // Just below the carry — stays in its own unit.
        assert_eq!(format_compact_count(999_449), "999.4k");
        assert_eq!(format_compact_count(999_949), "999.9k");
        // Half-up rounding pushes the mantissa to 1000.0k ⇒ ICU says `1.0m`.
        assert_eq!(format_compact_count(999_950), "1.0m");
        assert_eq!(format_compact_count(999_999), "1.0m");
        assert_eq!(format_compact_count(1_000_000), "1.0m");
        // Same carry one and two decades up.
        assert_eq!(format_compact_count(999_949_999), "999.9m");
        assert_eq!(format_compact_count(999_950_000), "1.0b");
        assert_eq!(format_compact_count(999_999_999), "1.0b");
        assert_eq!(format_compact_count(999_950_000_000), "1.0t");
        // `t` is the largest unit — nothing to promote to, so the carried
        // mantissa stands (matching Intl, which keeps counting in trillions).
        assert_eq!(format_compact_count(999_950_000_000_000), "1000.0t");
    }

    #[test]
    fn large_memory_file_status_row_is_byte_locked() {
        // `htf()` (@241152161):
        //   `Large ${Ad(o.path)} will impact performance (${yd(o.content.length)} chars > ${yd(n)})`
        assert_eq!(
            format_large_memory_file_status_row("LINGXI.md", 52_310, 40_000),
            "Large LINGXI.md will impact performance (52.3k chars > 40.0k)"
        );
    }

    #[test]
    fn get_large_memory_files_uses_the_supplied_threshold() {
        use std::time::SystemTime;
        let mk = |path: &str, content: String| MemoryFile {
            path: std::path::PathBuf::from(path),
            mtime: SystemTime::UNIX_EPOCH,
            frontmatter: MemoryFrontmatter::default(),
            content,
        };
        let files = vec![
            mk("/a/LINGXI.md", "x".repeat(50_000)),
            mk("/b/LINGXI.md", "x".repeat(30_000)),
        ];
        // 40k floor → only /a is flagged.
        let flagged = get_large_memory_files(&files, 40_000);
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].path, std::path::PathBuf::from("/a/LINGXI.md"));
        // A 1M-context model raises the bar to 150k → nothing is flagged.
        let flagged = get_large_memory_files(&files, max_memory_character_count(1_000_000, 3));
        assert!(flagged.is_empty());
        // Strictly greater-than (`>`), never `>=`.
        let exact = vec![mk("/c/LINGXI.md", "x".repeat(40_000))];
        assert!(get_large_memory_files(&exact, 40_000).is_empty());
    }

    #[test]
    fn char_count_matches_js_string_length_utf16_units() {
        // JS `content.length` counts UTF-16 code units, so an astral character
        // (a surrogate pair) counts as 2, not 1.
        assert_eq!(memory_file_char_count("abc"), 3);
        assert_eq!(memory_file_char_count("é"), 1); // BMP → 1 unit
        assert_eq!(memory_file_char_count("😀"), 2); // U+1F600 → surrogate pair
    }
}
