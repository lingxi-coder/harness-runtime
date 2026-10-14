//! Managed model allowlist — `availableModels` / `enforceAvailableModels` /
//! `modelOverrides`.
//!
//! 1:1 port of claude-code 2.1.207's managed model-restriction mechanism. An
//! enterprise administrator publishes an `availableModels` allowlist (plus the
//! `enforceAvailableModels` gate and an optional `modelOverrides` map) in the
//! MANAGED (`policySettings`) settings tier; the CLI then constrains which
//! models a user may select and which model `Default` resolves to.
//!
//! ## Allowlist entry semantics (binary `sl()` matcher, describe text verbatim)
//!
//! > Allowlist of models that users can select. Accepts family aliases
//! > (`"opus"` allows any opus version), version prefixes (`"opus-4-5"` allows
//! > only that version), and full model IDs. If undefined, all models are
//! > available. If empty array, only the default model is available.
//!
//! - `None` (unset) ⇒ every model is allowed.
//! - `Some([])` (empty) ⇒ nothing is allowed here (only the default model, which
//!   the picker/resolver keep selectable out-of-band).
//! - A **family alias** entry (`opus`/`sonnet`/`haiku`/`fable`) matches any model
//!   whose id contains that family token at a word boundary
//!   (binary `HYm`→`shi`) — UNLESS the allowlist also carries a more specific
//!   version-prefix of that family (binary `z5l`), in which case the broad
//!   family entry is skipped so the admin's narrower intent wins.
//! - A **version-prefix** entry (`opus-4-5`, `claude-opus-4-5`) matches a model
//!   whose id starts with the entry at a `-`/end boundary (binary `DYm`→`V5l`),
//!   trying both the bare entry and a `claude-`-prefixed form.
//! - A **full model id** entry matches that id exactly.
//! - `modelOverrides` reverse-maps a provider-specific id (e.g. a Bedrock
//!   inference-profile ARN) back to its Anthropic id before matching (binary
//!   `K5l`), so an admin whose fleet runs Bedrock can allowlist the Anthropic id.
//!
//! ## Normalization (binary `Ps`)
//!
//! Every id/entry is `trim().to_lowercase()`-ed and has a trailing `[1m]`
//! long-context suffix stripped before comparison.
//!
//! ## `enforceAvailableModels` policy-provenance (binary `ROn`)
//!
//! The enforce gate only binds when a **policy-owned** allowlist exists — the
//! flag on its own (no `availableModels` in the SAME policy view) is inert. A
//! policy source that exists but fails to load "refuses cascade-trust mode"
//! (fail-closed): enforcement from lower tiers is suppressed until the policy
//! source is fixed. See [`resolve_enforcement`].
//!
//! ## Deferred (documented remainder — needs the alias/catalog resolver)
//!
//! The binary matcher has two further branches that resolve an ALIAS *input*
//! (or a versioned-alias allowlist entry) through the live model catalog
//! (`Fx`/`Zo`/`_kr` — tier- and env-dependent). Those hops, the non-env-free
//! entitlement guard (`_On`), and the Windows `hkcu` registry cascade-trust
//! shortcut are NOT ported here; this module implements the env-free-resolution
//! path (the one the default-model constraint uses:
//! `sl(r,{allowlist,overridesMap,envFreeAliasResolution:!0})`), which covers the
//! three documented entry kinds (family alias, version prefix, full id) plus
//! `modelOverrides`.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::BTreeMap;

/// Family-alias tokens (binary `vxr`): a bare family name matches any version.
const FAMILY_ALIASES: &[&str] = &["sonnet", "opus", "haiku", "fable"];

/// All model-alias tokens (binary `wpe`), post-`Ps` (the `[1m]` variants
/// collapse onto their base). An allowlist entry that is a pure alias is handled
/// by the family/alias branches, not the version-prefix branch.
const ALL_ALIASES: &[&str] = &["sonnet", "opus", "haiku", "fable", "best", "opusplan"];

/// Normalize a scalar Rust model id with Native's UTF-16 `trim().toLowerCase()`
/// and trailing `[1m]` removal. Raw policy entries use the same transform over
/// their exact UTF-16 units, preserving lone surrogates.
#[must_use]
pub fn normalize(s: &str) -> String {
    String::from_utf16_lossy(&normalize_units(&s.encode_utf16().collect::<Vec<_>>()))
}

/// ECMAScript `String.prototype.trim` whitespace, including FEFF (which Rust's
/// `char::is_whitespace` intentionally does not classify as whitespace).
fn is_js_trim_whitespace(value: u32) -> bool {
    matches!(
        value,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn scalar_at(units: &[u16], index: usize) -> (u32, usize) {
    let unit = units[index];
    if (0xd800..=0xdbff).contains(&unit)
        && units
            .get(index + 1)
            .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
    {
        let high = u32::from(unit) - 0xd800;
        let low = u32::from(units[index + 1]) - 0xdc00;
        (0x10000 + (high << 10) + low, 2)
    } else {
        (u32::from(unit), 1)
    }
}

fn trim_js_units(units: &[u16]) -> &[u16] {
    let mut start = 0;
    while start < units.len() {
        let (scalar, width) = scalar_at(units, start);
        if !is_js_trim_whitespace(scalar) {
            break;
        }
        start += width;
    }

    let mut end = units.len();
    while end > start {
        let (scalar, width) = if (0xdc00..=0xdfff).contains(&units[end - 1])
            && end >= 2
            && (0xd800..=0xdbff).contains(&units[end - 2])
        {
            let high = u32::from(units[end - 2]) - 0xd800;
            let low = u32::from(units[end - 1]) - 0xdc00;
            (0x10000 + (high << 10) + low, 2)
        } else {
            (u32::from(units[end - 1]), 1)
        };
        if !is_js_trim_whitespace(scalar) {
            break;
        }
        end -= width;
    }
    &units[start..end]
}

/// Lowercase each valid Unicode run as a whole so contextual Unicode mappings
/// such as Greek final sigma match JavaScript. An unpaired surrogate is copied
/// as its original unit and separates scalar runs.
fn lowercase_utf16(units: &[u16]) -> Vec<u16> {
    let mut result = Vec::with_capacity(units.len());
    let mut run = String::new();
    let mut index = 0;
    while index < units.len() {
        let unit = units[index];
        if (0xd800..=0xdbff).contains(&unit)
            && units
                .get(index + 1)
                .is_some_and(|next| (0xdc00..=0xdfff).contains(next))
        {
            let (scalar, width) = scalar_at(units, index);
            run.push(char::from_u32(scalar).expect("valid UTF-16 surrogate pair"));
            index += width;
            continue;
        }
        if (0xd800..=0xdfff).contains(&unit) {
            result.extend(run.to_lowercase().encode_utf16());
            run.clear();
            result.push(unit);
            index += 1;
            continue;
        }
        run.push(char::from_u32(u32::from(unit)).expect("non-surrogate BMP code unit"));
        index += 1;
    }
    result.extend(run.to_lowercase().encode_utf16());
    result
}

fn normalize_units(units: &[u16]) -> Vec<u16> {
    let mut normalized = lowercase_utf16(trim_js_units(units));
    const ONE_M_SUFFIX: &[u16] = &[b'[' as u16, b'1' as u16, b'm' as u16, b']' as u16];
    if normalized.ends_with(ONE_M_SUFFIX) {
        normalized.truncate(normalized.len() - ONE_M_SUFFIX.len());
    }
    normalized
}

fn normalized_allowlist(allowlist: &Utf16JsonProjection) -> Option<Vec<Vec<u16>>> {
    let length = allowlist.array_len("")?;
    Some(
        (0..length)
            .filter_map(|index| allowlist.string_units(&format!("/{index}")))
            .map(|entry| normalize_units(&entry))
            .collect(),
    )
}

/// Whether a normalized token is a bare family alias (binary `XB`→`vxr`).
#[must_use]
fn is_family_alias(s: &[u16]) -> bool {
    FAMILY_ALIASES
        .iter()
        .any(|alias| s == alias.encode_utf16().collect::<Vec<_>>())
}

/// Whether a normalized token is any model alias (binary `Fx`→`wpe`, post-`Ps`).
fn is_alias(s: &[u16]) -> bool {
    ALL_ALIASES
        .iter()
        .any(|alias| s == alias.encode_utf16().collect::<Vec<_>>())
}

/// ASCII `[a-z0-9]` boundary classification in Native's family matcher.
fn is_ascii_alphanumeric(unit: u16) -> bool {
    (b'a' as u16..=b'z' as u16).contains(&unit) || (b'0' as u16..=b'9' as u16).contains(&unit)
}

fn contains_units(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Binary `shi(e,t)`: family token match at start/end or non-ASCII-alphanumeric
/// boundaries, without converting a raw UTF-16 model string.
fn family_segment_match(model: &[u16], family: &[u16]) -> bool {
    if family.is_empty() {
        return false;
    }
    let flen = family.len();
    let mut start = 0;
    while start <= model.len().saturating_sub(flen) {
        let Some(relative) = contains_units(&model[start..], family) else {
            break;
        };
        let at = start + relative;
        let left_ok = at == 0 || !is_ascii_alphanumeric(model[at - 1]);
        let end = at + flen;
        let right_ok = end == model.len() || !is_ascii_alphanumeric(model[end]);
        if left_ok && right_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

/// Binary `V5l(e,t)`: `e` starts with `t` at a `-`/end boundary.
#[must_use]
fn starts_with_segment(model: &[u16], prefix: &[u16]) -> bool {
    if !model.starts_with(prefix) {
        return false;
    }
    model.len() == prefix.len() || model[prefix.len()] == b'-' as u16
}

/// Binary `DYm(e,t)` (env-free arm, `e` already a concrete id): match the
/// version-prefix entry against the bare form and a `claude-`-prefixed form.
#[must_use]
fn version_prefix_match(model: &[u16], entry: &[u16]) -> bool {
    if starts_with_segment(model, entry) {
        return true;
    }
    const CLAUDE_PREFIX: &[u16] = &[
        b'c' as u16,
        b'l' as u16,
        b'a' as u16,
        b'u' as u16,
        b'd' as u16,
        b'e' as u16,
        b'-' as u16,
    ];
    if !entry.starts_with(CLAUDE_PREFIX) {
        let mut prefixed = CLAUDE_PREFIX.to_vec();
        prefixed.extend_from_slice(entry);
        if starts_with_segment(model, &prefixed) {
            return true;
        }
    }
    false
}

fn is_prefix_segment_of_any(token: &[u16], allowlist: &[Vec<u16>]) -> bool {
    for entry in allowlist {
        if is_family_alias(entry) {
            continue;
        }
        let mut start = 0;
        while start <= entry.len().saturating_sub(token.len()) {
            let Some(relative) = contains_units(&entry[start..], token) else {
                break;
            };
            let at = start + relative;
            let end = at + token.len();
            if end == entry.len() || entry[end] == b'-' as u16 {
                return true;
            }
            start = at + 1;
        }
    }
    false
}

fn apply_overrides(model: &[u16], overrides: &BTreeMap<String, String>) -> Vec<u16> {
    let target = normalize_units(model);
    for (anthropic_id, provider_id) in overrides {
        if normalize_units(&provider_id.encode_utf16().collect::<Vec<_>>()) == target {
            return anthropic_id.encode_utf16().collect();
        }
    }
    model.to_vec()
}

fn is_model_allowed_normalized(
    model: &[u16],
    allowlist: &[Vec<u16>],
    overrides: Option<&BTreeMap<String, String>>,
) -> bool {
    if allowlist.is_empty() {
        return false;
    }
    let i = normalize_units(model);

    if allowlist.iter().any(|entry| entry == &i) && !is_family_alias(&i) {
        return true;
    }

    let mapped = match overrides {
        Some(overrides) if !overrides.is_empty() => apply_overrides(model, overrides),
        _ => model.to_vec(),
    };
    let a = normalize_units(&mapped);

    if allowlist.iter().any(|entry| entry == &a)
        && (!is_family_alias(&a) || !is_prefix_segment_of_any(&a, allowlist))
    {
        return true;
    }

    for entry in allowlist {
        if is_family_alias(entry)
            && !is_prefix_segment_of_any(entry, allowlist)
            && family_segment_match(&a, entry)
        {
            return true;
        }
    }

    for entry in allowlist {
        if !is_family_alias(entry) && !is_alias(entry) && version_prefix_match(&a, entry) {
            return true;
        }
    }
    false
}

/// Whether `model` is permitted by the allowlist (env-free-resolution arm of the
/// binary `sl()` matcher). `allowlist == None` ⇒ every model allowed; an empty
/// allowlist ⇒ nothing allowed here.
#[must_use]
pub fn is_model_allowed(
    model: &str,
    allowlist: Option<&Utf16JsonProjection>,
    overrides: Option<&BTreeMap<String, String>>,
) -> bool {
    is_model_allowed_utf16(
        &model.encode_utf16().collect::<Vec<_>>(),
        allowlist,
        overrides,
    )
}

/// Exact-source form of [`is_model_allowed`] for a Native model string that
/// cannot be represented as a Rust `String`.
#[must_use]
pub fn is_model_allowed_utf16(
    model: &[u16],
    allowlist: Option<&Utf16JsonProjection>,
    overrides: Option<&BTreeMap<String, String>>,
) -> bool {
    let entries = match allowlist {
        None => return true, // `if(!n)return!0`
        Some(allowlist) => match normalized_allowlist(allowlist) {
            Some(entries) => entries,
            None => return false,
        },
    };
    is_model_allowed_normalized(model, &entries, overrides)
}

/// Effective admission state for model-picker and explicit model-selection
/// consumers. This is separate from [`ModelEnforcement`], whose `Active`
/// variant represents only Native's managed `enforceAvailableModels` Default
/// remapping gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CatalogRestriction {
    /// The selected settings snapshot has no `availableModels` restriction.
    #[default]
    Unrestricted,
    /// The selected settings snapshot contains `availableModels`; an empty
    /// array denies every concrete model while callers may preserve Default as
    /// a separate picker row.
    Restricted(Utf16JsonProjection),
    /// The settings source is unavailable/invalid and Native's admission gate
    /// is fail-closed for concrete model candidates.
    Refused,
}

/// Check one concrete model against a settings-source admission state.
#[must_use]
pub fn model_allowed_under_catalog(
    model: &str,
    restriction: &CatalogRestriction,
    overrides: Option<&BTreeMap<String, String>>,
) -> bool {
    model_allowed_under_catalog_utf16(
        &model.encode_utf16().collect::<Vec<_>>(),
        restriction,
        overrides,
    )
}

/// Exact-source form of [`model_allowed_under_catalog`] for a Native model
/// string whose UTF-16 units include an unpaired surrogate.
#[must_use]
pub fn model_allowed_under_catalog_utf16(
    model: &[u16],
    restriction: &CatalogRestriction,
    overrides: Option<&BTreeMap<String, String>>,
) -> bool {
    match restriction {
        CatalogRestriction::Unrestricted => true,
        CatalogRestriction::Restricted(allowlist) => {
            is_model_allowed_utf16(model, Some(allowlist), overrides)
        }
        CatalogRestriction::Refused => false,
    }
}

/// Build the picker/request admission state from the merged settings source.
/// A failed read takes precedence over any stale list; a readable snapshot with
/// no `availableModels` key is unrestricted, while an explicit empty array is
/// a restriction that denies every concrete model candidate.
#[must_use]
pub fn catalog_restriction_from_settings(
    settings_readable: bool,
    available_models: Option<Utf16JsonProjection>,
) -> CatalogRestriction {
    if !settings_readable {
        CatalogRestriction::Refused
    } else if let Some(available_models) = available_models {
        CatalogRestriction::Restricted(available_models)
    } else {
        CatalogRestriction::Unrestricted
    }
}

/// Resolve the default model under an active allowlist to the "first allowed
/// `availableModels` entry" (binary `enforceAvailableModels` describe text: "if
/// the default model for the user tier is not in `availableModels`, Default
/// resolves to the first allowed `availableModels` entry instead").
///
/// Iterates the allowlist entries in order (admin priority) and returns the
/// first `candidate` that entry permits. `candidates` is the concrete model
/// catalog to resolve family/prefix entries against.
#[must_use]
pub fn first_allowed_model(
    allowlist: &Utf16JsonProjection,
    candidates: &[String],
    overrides: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    let allowlist = normalized_allowlist(allowlist)?;
    for entry in &allowlist {
        let single = [entry.clone()];
        for cand in candidates {
            if is_model_allowed_normalized(
                &cand.encode_utf16().collect::<Vec<_>>(),
                &single,
                overrides,
            ) {
                return Some(cand.clone());
            }
        }
    }
    None
}

/// The numeric version tuple that follows a family token in a model id, used to
/// order candidates by recency for [`newest_permitted_in_family`]. Returns
/// `None` when `model` does not carry `family` at a token boundary.
///
/// - `"claude-opus-4-8"` / family `"opus"` ⇒ `Some([4, 8])`
/// - `"claude-sonnet-4-5-20250929"` / `"sonnet"` ⇒ `Some([4, 5, 20250929])`
/// - `"claude-haiku-4-5"` / `"opus"` ⇒ `None` (different family)
///
/// The tuple compares lexicographically, so `[4, 8] > [4, 6]` and a dated
/// variant `[4, 5, 20250929]` outranks the bare `[4, 5]`.
#[must_use]
fn family_version_key(model: &str, family: &[u16]) -> Option<Vec<u64>> {
    let m = normalize_units(&model.encode_utf16().collect::<Vec<_>>());
    if family.is_empty() {
        return None;
    }
    let flen = family.len();
    let mut start = 0;
    while start <= m.len().saturating_sub(flen) {
        let Some(relative) = contains_units(&m[start..], family) else {
            break;
        };
        let at = start + relative;
        let left_ok = at == 0 || !is_ascii_alphanumeric(m[at - 1]);
        let end = at + flen;
        let right_ok = end == m.len() || !is_ascii_alphanumeric(m[end]);
        if left_ok && right_ok {
            let mut nums = Vec::new();
            let mut digits = Vec::new();
            for unit in &m[end..] {
                if (b'0' as u16..=b'9' as u16).contains(unit) {
                    digits.push(*unit as u8);
                } else if !digits.is_empty() {
                    if let Ok(number) = std::str::from_utf8(&digits).ok()?.parse::<u64>() {
                        nums.push(number);
                    }
                    digits.clear();
                }
            }
            if !digits.is_empty() {
                if let Ok(number) = std::str::from_utf8(&digits).ok()?.parse::<u64>() {
                    nums.push(number);
                }
            }
            return Some(nums);
        }
        start = at + 1;
    }
    None
}

/// The newest catalog model of a given family that the allowlist permits — the
/// env-free arm of the binary `j5()`/`ykr()` "newest permitted" selection used by
/// the plan-mode upgrade swap (binary `RF`). `family` must be a bare family alias
/// (`opus`/`sonnet`/`haiku`/`fable`); any other input yields `None`.
///
/// Candidates are filtered to that family AND to those [`is_model_allowed`]
/// permits, then the highest [`family_version_key`] wins (ties keep the earliest
/// catalog entry). Returns `None` when the family or catalog is empty or nothing
/// in the family is permitted (the caller then falls back to the resting model).
///
/// The `[1m]` long-context re-tag and the `model_access` entitlement guard (`P4`)
/// arms of the binary `j5()` are the documented deferred remainder — only the
/// `availableModels` allowlist path is modeled here (consistent with the landed
/// matcher's env-free scope).
#[must_use]
pub fn newest_permitted_in_family(
    family: &str,
    candidates: &[String],
    allowlist: Option<&Utf16JsonProjection>,
    overrides: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    let fam = normalize_units(&family.encode_utf16().collect::<Vec<_>>());
    if !is_family_alias(&fam) {
        return None;
    }
    let allowlist = match allowlist {
        None => None,
        Some(projection) => Some(normalized_allowlist(projection)?),
    };
    let mut best: Option<(Vec<u64>, &String)> = None;
    for cand in candidates {
        let Some(key) = family_version_key(cand, &fam) else {
            continue;
        };
        if !allowlist.as_deref().is_none_or(|entries| {
            is_model_allowed_normalized(
                &cand.encode_utf16().collect::<Vec<_>>(),
                entries,
                overrides,
            )
        }) {
            continue;
        }
        match &best {
            Some((best_key, _)) if *best_key >= key => {}
            _ => best = Some((key, cand)),
        }
    }
    best.map(|(_, cand)| cand.clone())
}

// ── enforceAvailableModels policy-provenance (binary `ROn`) ────────────────

/// Byte-exact warning strings (binary), emitted deduplicated by the caller.
pub mod warnings {
    /// The enforce flag is set in the policy view but no `availableModels` is —
    /// enforcement stays disabled (the flag requires a policy-owned allowlist).
    pub const ENFORCE_WITHOUT_ALLOWLIST: &str = "enforceAvailableModels: the policy view sets the enforce flag but not availableModels; enforcement is disabled (the flag requires a policy-owned allowlist)";
    /// A policy source exists but failed to load — refuse cascade-trust mode.
    pub const POLICY_LOAD_FAILED_CASCADE: &str = "enforceAvailableModels: a policy source exists but failed to load; refusing cascade-trust mode (model enforcement from user/project settings is disabled until the policy source is fixed)";
    /// Prefix of the policy-tier read-failure message (binary appends the error).
    pub const POLICY_READ_FAILED_PREFIX: &str =
        "enforceAvailableModels: policy-tier settings read failed; refusing cascade-trust mode: ";
    /// A model was requested for a subagent but is not allowlisted.
    pub const NOT_IN_ALLOWLIST_SUBAGENT: &str =
        "\" is not in the availableModels allowlist; inheriting the parent model instead";
    /// A model is not allowlisted; keep the current session model.
    pub const NOT_IN_ALLOWLIST_SESSION: &str =
        "\" is not in the availableModels allowlist; keeping the session model";
    /// A teammate model is not allowlisted; use the default teammate model.
    pub const NOT_IN_ALLOWLIST_TEAMMATE: &str =
        "\" is not in the availableModels allowlist; using the default teammate model instead";
    /// A server refusal-fallback target is not allowlisted; decline the swap.
    pub const NOT_IN_ALLOWLIST_SWAP_DECLINE: &str =
        "\" is not in the availableModels allowlist; declining the swap";

    // ── Plan-mode upgrade-model gating (binary `RF`) ───────────────────────
    //
    // Plan mode swaps a resting `opusplan` setting up to Opus (and a `haiku`
    // setting up to Sonnet). When that upgrade model is barred by the managed
    // restriction the binary picks the newest permitted model of the same family
    // (`newest_permitted_in_family`), else falls back to the resting model — each
    // path emitting one of these byte-exact strings.

    /// `opusplan` upgrade barred; a newer permitted Opus was substituted.
    pub const PLAN_OPUSPLAN_NEWEST: &str = "Plan mode: the opusplan upgrade model is not permitted by the org model restrictions (availableModels allowlist or model_access entitlement); planning uses the newest permitted Opus instead";
    /// `opusplan` upgrade barred and no permitted Opus exists; use the resting model.
    pub const PLAN_OPUSPLAN_RESTING: &str = "Plan mode: the opusplan upgrade model is not permitted by the org model restrictions (availableModels allowlist or model_access entitlement); planning uses the resting model instead";
    /// `haiku` plan upgrade (to Sonnet) barred; a newer permitted Sonnet was substituted.
    pub const PLAN_HAIKU_NEWEST: &str = "Plan mode: the haiku plan upgrade model is not permitted by the org model restrictions (availableModels allowlist or model_access entitlement); planning uses the newest permitted Sonnet instead";
    /// `haiku` plan upgrade barred and no permitted Sonnet exists; use the resting model.
    pub const PLAN_HAIKU_RESTING: &str = "Plan mode: the haiku plan upgrade model is not permitted by the org model restrictions (availableModels allowlist or model_access entitlement); planning uses the resting model instead";
}

/// The policy (managed / `policySettings`) view of the model-restriction keys.
#[derive(Debug, Clone, Default)]
pub struct PolicyModelView {
    /// Policy-tier `availableModels` (`None` ⇒ key absent in the policy view).
    pub available_models: Option<Utf16JsonProjection>,
    /// Policy-tier `enforceAvailableModels`.
    pub enforce: Option<bool>,
    /// Policy-tier `modelOverrides`.
    pub model_overrides: Option<BTreeMap<String, String>>,
}

/// Whether the policy source loaded, and if so its model view.
#[derive(Debug, Clone)]
pub enum PolicySource {
    /// The policy source loaded (or is simply absent) — carries its model view.
    Loaded(PolicyModelView),
    /// A policy source exists on disk but failed to parse (fail-closed).
    Failed,
}

/// Resolved enforcement state (binary `ROn` return, minus the Windows `hkcu`
/// cascade-trusted shortcut which has no LingXi analog).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEnforcement {
    /// A policy source failed to load — every model is refused (fail-closed).
    Refused,
    /// No enforcement in effect — the allowlist imposes no restriction.
    Inactive,
    /// Enforcement is active with a policy-owned allowlist.
    Active {
        /// The policy-owned allowlist.
        allowlist: Utf16JsonProjection,
        /// The policy-owned `modelOverrides` (empty when unset).
        overrides: BTreeMap<String, String>,
    },
}

/// Resolve `enforceAvailableModels` from the policy source (binary `ROn`).
///
/// `warn` receives each byte-exact warning string once (the caller is
/// responsible for de-duplication, mirroring the binary's module-level `SN`
/// set). Provenance rules:
/// - policy load failed ⇒ [`ModelEnforcement::Refused`] (fail-closed).
/// - enforce flag set but no policy `availableModels` ⇒ warn + inactive.
/// - enforce not `true`, or no/empty policy allowlist ⇒ inactive.
/// - otherwise ⇒ active with the policy-owned allowlist + overrides.
#[must_use]
pub fn resolve_enforcement(source: &PolicySource, warn: &mut dyn FnMut(&str)) -> ModelEnforcement {
    let view = match source {
        PolicySource::Failed => {
            warn(warnings::POLICY_READ_FAILED_PREFIX);
            return ModelEnforcement::Refused;
        }
        PolicySource::Loaded(v) => v,
    };

    // Enforce flag present-and-true but no policy-owned allowlist ⇒ inert.
    if view.enforce == Some(true) && view.available_models.is_none() {
        warn(warnings::ENFORCE_WITHOUT_ALLOWLIST);
        return ModelEnforcement::Inactive;
    }

    match &view.available_models {
        Some(list)
            if view.enforce == Some(true)
                && list.array_len("").is_some_and(|length| length > 0) =>
        {
            ModelEnforcement::Active {
                allowlist: list.clone(),
                overrides: view.model_overrides.clone().unwrap_or_default(),
            }
        }
        // enforce not true, or allowlist absent/empty ⇒ no restriction.
        _ => ModelEnforcement::Inactive,
    }
}

/// Gate a single model through resolved enforcement (binary `P4`): `Some(false)`
/// = refused/blocked, `None` = no enforcement, `Some(true)` = allowed.
#[must_use]
pub fn model_allowed_under(enforcement: &ModelEnforcement, model: &str) -> Option<bool> {
    match enforcement {
        ModelEnforcement::Refused => Some(false),
        ModelEnforcement::Inactive => None,
        ModelEnforcement::Active {
            allowlist,
            overrides,
        } => Some(is_model_allowed(model, Some(allowlist), Some(overrides))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_list(items: &[&str]) -> Utf16JsonProjection {
        Utf16JsonProjection::array(items.iter().map(|item| (*item).into()).collect()).unwrap()
    }

    fn sl(model: &str, allow: &[&str]) -> bool {
        is_model_allowed(model, Some(&model_list(allow)), None)
    }

    #[test]
    fn undefined_allowlist_permits_everything() {
        assert!(is_model_allowed("claude-opus-4-5-20990101", None, None));
        assert!(is_model_allowed("literally-anything", None, None));
    }

    #[test]
    fn empty_allowlist_permits_nothing() {
        assert!(!sl("claude-opus-4-5", &[]));
        assert!(!sl("opus", &[]));
    }

    #[test]
    fn full_model_id_exact_match() {
        assert!(sl(
            "claude-opus-4-5-20990101",
            &["claude-opus-4-5-20990101"]
        ));
        assert!(!sl(
            "claude-sonnet-4-5-20990101",
            &["claude-opus-4-5-20990101"]
        ));
    }

    #[test]
    fn family_alias_matches_any_version() {
        // "opus" allows any opus version (describe text).
        assert!(sl("claude-opus-4-5-20990101", &["opus"]));
        assert!(sl("claude-opus-4-1-20250805", &["opus"]));
        // but not a different family.
        assert!(!sl("claude-sonnet-4-5-20990101", &["opus"]));
        assert!(!sl("claude-haiku-4-5", &["opus"]));
    }

    #[test]
    fn family_alias_word_boundary_only() {
        // "opus" must match at a token boundary — no substring false-positives.
        assert!(!sl("claude-magnumopusish-1", &["opus"]));
        assert!(sl("claude-opus-4-5", &["opus"]));
    }

    #[test]
    fn version_prefix_matches_that_version_only() {
        // "opus-4-5" allows only that version (describe text). Bare + claude- form.
        assert!(sl("claude-opus-4-5-20990101", &["opus-4-5"]));
        assert!(sl("opus-4-5-20990101", &["opus-4-5"]));
        assert!(sl("claude-opus-4-5-20990101", &["claude-opus-4-5"]));
        // a different version is NOT matched by the prefix.
        assert!(!sl("claude-opus-4-1-20250805", &["opus-4-5"]));
        // and the prefix is boundary-anchored: opus-4-50 must not match opus-4-5.
        assert!(!sl("claude-opus-4-50-x", &["opus-4-5"]));
    }

    #[test]
    fn narrower_version_entry_suppresses_broad_family_entry() {
        // Admin lists BOTH "opus" and "opus-4-5": the family entry is suppressed
        // (binary z5l guard) so a different opus version is NOT allowed via the
        // broad family entry.
        assert!(sl("claude-opus-4-5-20990101", &["opus", "opus-4-5"]));
        assert!(!sl("claude-opus-4-1-20250805", &["opus", "opus-4-5"]));
    }

    #[test]
    fn case_and_whitespace_normalized() {
        assert!(sl("  Claude-OPUS-4-5-20990101  ", &["  OPUS  "]));
        assert!(sl("CLAUDE-OPUS-4-5", &["Claude-Opus-4-5"]));
        assert!(sl("\u{feff}CLAUDE-OPUS-4-5\u{feff}", &["opus"]));
        assert!(sl("\u{00a0}CLAUDE-OPUS-4-5\u{00a0}", &["opus"]));
        assert_eq!(normalize("ΟΣ"), "ος");
    }

    #[test]
    fn raw_utf16_surrogate_pattern_is_preserved_and_does_not_hide_siblings() {
        let allowlist = Utf16JsonProjection::parse(r#"["\ud800"," OPUS "]"#).unwrap();
        assert_eq!(allowlist.string_units("/0"), Some(vec![0xd800]));
        assert_eq!(normalized_allowlist(&allowlist).unwrap()[0], vec![0xd800]);
        assert!(is_model_allowed("claude-opus-4-8", Some(&allowlist), None));

        let only_unpaired = Utf16JsonProjection::parse(r#"["\ud800"]"#).unwrap();
        assert!(!is_model_allowed(
            "claude-opus-4-8",
            Some(&only_unpaired),
            None
        ));
    }

    #[test]
    fn source_model_allowlist_does_not_match_its_display_replacement() {
        let restriction = CatalogRestriction::Restricted(
            Utf16JsonProjection::parse(r#"["model-\ufffd"]"#).unwrap(),
        );
        let source_units = [
            b'm' as u16,
            b'o' as u16,
            b'd' as u16,
            b'e' as u16,
            b'l' as u16,
            b'-' as u16,
            0xd800,
        ];
        assert!(!model_allowed_under_catalog_utf16(
            &source_units,
            &restriction,
            None,
        ));
        assert!(model_allowed_under_catalog(
            "model-\u{fffd}",
            &restriction,
            None,
        ));

        let exact = CatalogRestriction::Restricted(
            Utf16JsonProjection::parse(r#"["model-\ud800"]"#).unwrap(),
        );
        assert!(model_allowed_under_catalog_utf16(
            &source_units,
            &exact,
            None,
        ));
    }

    #[test]
    fn non_string_projection_entries_do_not_discard_valid_patterns() {
        let allowlist = Utf16JsonProjection::parse(r#"[17,"sonnet",null]"#).unwrap();
        assert!(is_model_allowed(
            "claude-sonnet-4-8",
            Some(&allowlist),
            None
        ));
        assert!(!is_model_allowed("claude-opus-4-8", Some(&allowlist), None));
    }

    #[test]
    fn catalog_restriction_preserves_absent_empty_and_failed_source_states() {
        let model = "claude-opus-4-8";
        let unrestricted = catalog_restriction_from_settings(true, None);
        assert_eq!(unrestricted, CatalogRestriction::Unrestricted);
        assert!(model_allowed_under_catalog(model, &unrestricted, None));

        let empty = catalog_restriction_from_settings(true, Some(model_list(&[])));
        assert_eq!(empty, CatalogRestriction::Restricted(model_list(&[])));
        assert!(!model_allowed_under_catalog(model, &empty, None));

        // A stale list cannot turn a failed settings read into an admitted
        // candidate; Native's VQ() check is evaluated before the fallback list.
        let refused = catalog_restriction_from_settings(false, Some(model_list(&["opus"])));
        assert_eq!(refused, CatalogRestriction::Refused);
        assert!(!model_allowed_under_catalog(model, &refused, None));
    }

    #[test]
    fn one_m_suffix_stripped() {
        // The `[1m]` long-context suffix is stripped before comparison.
        assert!(sl("claude-opus-4-5-20990101[1m]", &["opus"]));
        assert!(sl("claude-opus-4-5[1M]", &["claude-opus-4-5"]));
        assert!(sl("opus[1m]", &["opus"]));
    }

    #[test]
    fn model_overrides_reverse_maps_provider_id() {
        // Admin allowlists the Anthropic id; the fleet runs a Bedrock ARN. The
        // ARN reverse-maps to the Anthropic id before matching.
        let mut ov = BTreeMap::new();
        ov.insert(
            "claude-opus-4-5".to_string(),
            "arn:aws:bedrock:us-east-1::inference-profile/opus".to_string(),
        );
        let allow = model_list(&["claude-opus-4-5"]);
        assert!(is_model_allowed(
            "arn:aws:bedrock:us-east-1::inference-profile/opus",
            Some(&allow),
            Some(&ov)
        ));
        // Without the override map, the raw ARN does not match.
        assert!(!is_model_allowed(
            "arn:aws:bedrock:us-east-1::inference-profile/opus",
            Some(&allow),
            None
        ));
    }

    #[test]
    fn first_allowed_model_picks_first_entry_in_order() {
        let allow = model_list(&["haiku", "opus"]);
        let catalog = vec![
            "claude-opus-4-5-20990101".to_string(),
            "claude-haiku-4-5-20990101".to_string(),
            "claude-sonnet-4-5-20990101".to_string(),
        ];
        // "haiku" is the first allowlist entry ⇒ resolves to the haiku catalog id.
        assert_eq!(
            first_allowed_model(&allow, &catalog, None).as_deref(),
            Some("claude-haiku-4-5-20990101")
        );
    }

    #[test]
    fn first_allowed_model_none_when_nothing_matches() {
        let allow = model_list(&["opus"]);
        let catalog = vec!["gpt-5.5".to_string(), "gemini-2.0".to_string()];
        assert_eq!(first_allowed_model(&allow, &catalog, None), None);
    }

    // ── newest_permitted_in_family (binary j5/ykr env-free arm) ─────────────

    fn cat(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn newest_permitted_picks_highest_allowed_version() {
        let catalog = cat(&[
            "claude-opus-4-1-20250805",
            "claude-opus-4-6",
            "claude-opus-4-8",
            "claude-sonnet-4-5-20250929",
        ]);
        // The whole opus family is allowed → the newest opus (4-8) wins.
        let allow = model_list(&["opus"]);
        assert_eq!(
            newest_permitted_in_family("opus", &catalog, Some(&allow), None).as_deref(),
            Some("claude-opus-4-8")
        );
    }

    #[test]
    fn newest_permitted_respects_a_narrower_allowlist() {
        let catalog = cat(&[
            "claude-opus-4-1-20250805",
            "claude-opus-4-6",
            "claude-opus-4-8",
        ]);
        // Only 4-6 is permitted → newest permitted opus is 4-6 (NOT the newer 4-8).
        let allow = model_list(&["opus-4-6"]);
        assert_eq!(
            newest_permitted_in_family("opus", &catalog, Some(&allow), None).as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn newest_permitted_none_when_family_absent_from_allowlist() {
        let catalog = cat(&["claude-opus-4-8", "claude-sonnet-4-5-20250929"]);
        // Allowlist permits only sonnet → no permitted opus.
        let allow = model_list(&["sonnet"]);
        assert_eq!(
            newest_permitted_in_family("opus", &catalog, Some(&allow), None),
            None
        );
    }

    #[test]
    fn newest_permitted_rejects_non_family_alias_input() {
        let catalog = cat(&["claude-opus-4-8"]);
        let allow = model_list(&["opus"]);
        // "opusplan" / "best" / a full id are not bare family aliases → None.
        assert_eq!(
            newest_permitted_in_family("opusplan", &catalog, Some(&allow), None),
            None
        );
        assert_eq!(
            newest_permitted_in_family("claude-opus-4-8", &catalog, Some(&allow), None),
            None
        );
    }

    #[test]
    fn newest_permitted_dated_variant_outranks_bare() {
        // A dated build sorts newer than the bare version of the same tuple prefix.
        let catalog = cat(&["claude-sonnet-4-5", "claude-sonnet-4-5-20250929"]);
        let allow = model_list(&["sonnet"]);
        assert_eq!(
            newest_permitted_in_family("sonnet", &catalog, Some(&allow), None).as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
    }

    #[test]
    fn newest_permitted_reverse_maps_overrides() {
        // The catalog carries a Bedrock ARN; the allowlist permits the Anthropic id.
        let catalog = cat(&["arn:aws:bedrock:us-east-1::inference-profile/opus-4-8"]);
        let allow = model_list(&["claude-opus-4-8"]);
        let mut ov = BTreeMap::new();
        ov.insert(
            "claude-opus-4-8".to_string(),
            "arn:aws:bedrock:us-east-1::inference-profile/opus-4-8".to_string(),
        );
        // The ARN carries the `opus` family token AND reverse-maps to an allowed id.
        assert_eq!(
            newest_permitted_in_family("opus", &catalog, Some(&allow), Some(&ov)).as_deref(),
            Some("arn:aws:bedrock:us-east-1::inference-profile/opus-4-8")
        );
    }

    // ── enforceAvailableModels policy-provenance ──────────────────────────

    fn resolve(view: PolicyModelView) -> (ModelEnforcement, Vec<String>) {
        let mut msgs = Vec::new();
        let out = resolve_enforcement(&PolicySource::Loaded(view), &mut |m| {
            msgs.push(m.to_string())
        });
        (out, msgs)
    }

    #[test]
    fn enforce_requires_policy_owned_allowlist() {
        // Flag true but no policy availableModels ⇒ inactive + byte-exact warn.
        let (state, msgs) = resolve(PolicyModelView {
            available_models: None,
            enforce: Some(true),
            model_overrides: None,
        });
        assert_eq!(state, ModelEnforcement::Inactive);
        assert_eq!(msgs, vec![warnings::ENFORCE_WITHOUT_ALLOWLIST.to_string()]);
    }

    #[test]
    fn enforce_active_with_policy_allowlist() {
        let (state, msgs) = resolve(PolicyModelView {
            available_models: Some(model_list(&["opus"])),
            enforce: Some(true),
            model_overrides: None,
        });
        assert_eq!(
            state,
            ModelEnforcement::Active {
                allowlist: model_list(&["opus"]),
                overrides: BTreeMap::new(),
            }
        );
        assert!(msgs.is_empty());
    }

    #[test]
    fn enforced_policy_retains_lone_surrogate_allowlist_entries() {
        let allowlist = Utf16JsonProjection::parse(r#"["\ud800","opus"]"#).unwrap();
        let (state, _) = resolve(PolicyModelView {
            available_models: Some(allowlist),
            enforce: Some(true),
            model_overrides: None,
        });
        let ModelEnforcement::Active { allowlist, .. } = state else {
            panic!("a scalar-unrepresentable string entry must not refuse the policy layer")
        };
        assert_eq!(allowlist.string_units("/0"), Some(vec![0xd800]));
        assert!(is_model_allowed("claude-opus-4-8", Some(&allowlist), None));
    }

    #[test]
    fn enforce_inactive_when_flag_absent() {
        // Allowlist present but enforce not true ⇒ inactive (no restriction).
        let (state, _) = resolve(PolicyModelView {
            available_models: Some(model_list(&["opus"])),
            enforce: None,
            model_overrides: None,
        });
        assert_eq!(state, ModelEnforcement::Inactive);
    }

    #[test]
    fn enforce_inactive_when_allowlist_empty() {
        // "Has no effect when availableModels is unset or an empty array."
        let (state, _) = resolve(PolicyModelView {
            available_models: Some(model_list(&[])),
            enforce: Some(true),
            model_overrides: None,
        });
        assert_eq!(state, ModelEnforcement::Inactive);
    }

    #[test]
    fn policy_load_failure_refuses_cascade_trust() {
        let mut msgs = Vec::new();
        let out = resolve_enforcement(&PolicySource::Failed, &mut |m| msgs.push(m.to_string()));
        assert_eq!(out, ModelEnforcement::Refused);
        assert_eq!(msgs, vec![warnings::POLICY_READ_FAILED_PREFIX.to_string()]);
    }

    #[test]
    fn model_allowed_under_gate() {
        let active = ModelEnforcement::Active {
            allowlist: model_list(&["opus"]),
            overrides: BTreeMap::new(),
        };
        assert_eq!(
            model_allowed_under(&active, "claude-opus-4-5-20990101"),
            Some(true)
        );
        assert_eq!(
            model_allowed_under(&active, "claude-sonnet-4-5"),
            Some(false)
        );
        assert_eq!(model_allowed_under(&ModelEnforcement::Inactive, "x"), None);
        assert_eq!(
            model_allowed_under(&ModelEnforcement::Refused, "claude-opus-4-5"),
            Some(false)
        );
    }
}
