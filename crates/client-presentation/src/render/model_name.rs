//! Friendly model-display-name resolution (claude-code `renderModelName` →
//! `getPublicModelDisplayName`, `utils/model/model.ts`).
//!
//! The slash-palette, `/stats`, and the model header show a friendly name
//! (`Opus 4.6`) for publicly-known Claude models rather than the raw wire id
//! (`claude-opus-4-6-20260101`). Unknown / non-Claude ids fall through to the
//! raw id verbatim — exactly claude-code's `default: return null` → `return
//! model` behavior.
//!
//! Unlike claude-code's exact-id `switch`, LingXi's wire ids carry date and
//! `-x`/`-v1` suffixes (`claude-opus-4-6-20260101`) AND newer models than the
//! leaked snapshot (`opus-4-7`, `opus-4-8`), so matching is by version PREFIX
//! with a `-`/end boundary (so `claude-opus-4-1` never matches
//! `claude-opus-4-10`). The `[1m]` suffix maps to the `(1M context)` label.

/// `(version-prefix, friendly-name)` table, most-specific first within each
/// family so `claude-opus-4-6` wins over the `claude-opus-4` catch-all.
const DISPLAY_NAMES: &[(&str, &str)] = &[
    ("claude-opus-4-8", "Opus 4.8"),
    ("claude-opus-4-7", "Opus 4.7"),
    ("claude-opus-4-6", "Opus 4.6"),
    ("claude-opus-4-5", "Opus 4.5"),
    ("claude-opus-4-1", "Opus 4.1"),
    ("claude-opus-4", "Opus 4"),
    // 2.1.198 registry: claude-sonnet-5 → "Sonnet 5". Boundary matching keeps
    // it distinct from claude-sonnet-4-x (and vice versa).
    ("claude-sonnet-5", "Sonnet 5"),
    ("claude-sonnet-4-6", "Sonnet 4.6"),
    ("claude-sonnet-4-5", "Sonnet 4.5"),
    ("claude-sonnet-4", "Sonnet 4"),
    ("claude-3-7-sonnet", "Sonnet 3.7"),
    ("claude-3-5-sonnet", "Sonnet 3.5"),
    ("claude-haiku-4-5", "Haiku 4.5"),
    ("claude-haiku-4", "Haiku 4"),
    ("claude-3-5-haiku", "Haiku 3.5"),
];

/// `true` when `base` is exactly `prefix` or `prefix` followed by `-` — a
/// version-boundary match so `claude-opus-4-1` does not match
/// `claude-opus-4-10`.
fn matches_version(base: &str, prefix: &str) -> bool {
    base == prefix
        || base
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Friendly display name for a model id (claude-code `renderModelName`).
/// Publicly-known Claude models map to `Opus 4.6` etc.; everything else
/// (custom/aliased/non-Claude) returns the raw id unchanged. A trailing
/// `[1m]` becomes the ` (1M context)` label.
#[must_use]
pub fn render_model_name(model: &str) -> String {
    // Split a trailing `[1m]` (case-insensitive), keeping the base for lookup.
    let (base, one_m) = match model.len().checked_sub(4) {
        Some(cut) if model[cut..].eq_ignore_ascii_case("[1m]") => (&model[..cut], true),
        _ => (model, false),
    };
    match DISPLAY_NAMES
        .iter()
        .find(|(prefix, _)| matches_version(base, prefix))
    {
        Some((_, name)) if one_m => format!("{name} (1M context)"),
        Some((_, name)) => (*name).to_string(),
        // Unknown id → raw verbatim (incl. any `[1m]` suffix).
        None => model.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_claude_ids_with_date_suffixes() {
        assert_eq!(render_model_name("claude-opus-4-6"), "Opus 4.6");
        assert_eq!(render_model_name("claude-opus-4-6-20260101"), "Opus 4.6");
        assert_eq!(render_model_name("claude-opus-4-8-20260115"), "Opus 4.8");
        assert_eq!(render_model_name("claude-opus-4-5-20251101-v1"), "Opus 4.5");
        assert_eq!(render_model_name("claude-sonnet-4-6"), "Sonnet 4.6");
        // Sonnet 5 (2.1.198) — including the 1M-context label; boundary match
        // keeps claude-sonnet-4-5 on "Sonnet 4.5".
        assert_eq!(render_model_name("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(render_model_name("claude-sonnet-5-20260203"), "Sonnet 5");
        assert_eq!(
            render_model_name("claude-sonnet-5[1m]"),
            "Sonnet 5 (1M context)"
        );
        assert_eq!(
            render_model_name("claude-sonnet-4-5-20250929"),
            "Sonnet 4.5"
        );
        assert_eq!(render_model_name("claude-haiku-4-5"), "Haiku 4.5");
        // bare opus 4 (any date) → "Opus 4".
        assert_eq!(render_model_name("claude-opus-4-20250514"), "Opus 4");
        assert_eq!(render_model_name("claude-opus-4-0-20250514"), "Opus 4");
    }

    #[test]
    fn one_m_suffix_appends_context_label() {
        assert_eq!(
            render_model_name("claude-opus-4-6[1m]"),
            "Opus 4.6 (1M context)"
        );
        assert_eq!(
            render_model_name("claude-sonnet-4-5[1M]"),
            "Sonnet 4.5 (1M context)"
        );
    }

    #[test]
    fn version_boundary_avoids_false_prefix_match() {
        // Hypothetical 4-10 must NOT mislabel as "Opus 4.1" — the boundary check
        // skips the 4-1 entry, so it falls to the "claude-opus-4" catch-all
        // ("Opus 4") rather than the wrong minor version.
        assert_eq!(render_model_name("claude-opus-4-10"), "Opus 4");
    }

    #[test]
    fn unknown_and_non_claude_ids_pass_through() {
        assert_eq!(render_model_name("gpt-4o"), "gpt-4o");
        assert_eq!(render_model_name("glm-4.6"), "glm-4.6");
        assert_eq!(render_model_name("o3-mini"), "o3-mini");
        assert_eq!(render_model_name(""), "");
    }
}
