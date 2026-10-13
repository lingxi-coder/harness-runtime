//! `skill_listing` system-reminder formatter + provider seam.
//!
//! 1:1 with claude-code 2.1.295 `src/tools/SkillTool/prompt.ts` (`eYt`)
//! and the `messages.ts:3728-3738`
//! renderer that wraps the listing in a `<system-reminder>` meta user message.
//!
//! IMPORTANT: the skill listing is NOT a system-prompt section. It is a
//! model-facing meta user message. The initial regular-user ingress persists it
//! as a typed attachment with the exact rendered cache; later per-step listing
//! messages remain in the outgoing snapshot. See
//! [`crate::conversation::ConversationOrchestrator::skill_listing_reminder_message`].
#![forbid(unsafe_code)]

use async_trait::async_trait;
use lingxi_core::settings::{SkillOverride, SkillOverrides};
use lingxi_core::types::utf16_json::{Utf16JsonBuilder, Utf16JsonProjection as Source};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

/// Minimal per-skill view the formatter needs. The composition root builds
/// these from `command_api::SlashCommand` so the orchestrator crate need not
/// depend on `command-api`.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillListingEntry {
    /// Canonical skill name (TS `cmd.name`), retained as exact UTF-16 units.
    pub name: Source,
    /// Native `Rwt(cmd, fullCatalog)` label. This is the canonical name when
    /// no unambiguous user-facing alias is applicable, or `name (alias)` when
    /// Native resolves the command's user-facing name back to this command.
    pub display_name: Source,
    /// Native `Oo(cmd)` value, when the command producer actually has one.
    /// Missing values fall back to the canonical name; aliases are not used to
    /// synthesize this property.
    pub user_facing_name: Option<Source>,
    /// Native optional unqualified command name used by skill override lookup.
    pub unqualified_name: Option<Source>,
    /// Whether this command came from Native `syncedSkills`.
    pub is_synced_skill: bool,
    /// Native reservation bit used only for direct unqualified lookup. Alias
    /// candidates still include the unqualified name when this is true.
    pub bare_name_reserved: bool,
    /// Native command aliases in source order.
    pub aliases: Vec<Source>,
    /// Exact Native `skillFrontmatter.source` projection, when the host has it.
    pub source: Option<Source>,
    /// Exact Native `pluginInfo.pluginManifest.name`, when present.
    pub plugin_name: Option<Source>,
    /// Short description (TS `cmd.description`).
    pub description: Source,
    /// Optional "when to use" guidance (TS `cmd.whenToUse`).
    pub when_to_use: Option<Source>,
    /// Native `cmd.type === "prompt" && cmd.source === "bundled"`.
    pub is_bundled_prompt: bool,
    /// Native `CC(cmd) === "name-only"`; name-only rows are protected and use
    /// the raw canonical name without a description.
    pub is_name_only: bool,
    /// The current Native `xwt(cmd.name)` usage score. Stable descending sort
    /// chooses which descriptions fit; final output keeps the source order.
    pub priority: f64,
}

/// Name-bearing data used by Native `Hs` while resolving a user-facing name.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillListingCatalogName {
    /// Canonical command name.
    pub name: Source,
    /// Optional `userFacingName` callback result.
    pub user_facing_name: Option<Source>,
    /// Command aliases, in full-catalog registration order.
    pub aliases: Vec<Source>,
}

/// Resolve Native `cte(cmd)` from the merged base `skillOverrides` and the
/// managed policy / CLI flag tiers. Invalid raw tiers are omitted by the host
/// settings loader before they reach this helper.
#[must_use]
pub fn native_skill_override(
    entry: &SkillListingEntry,
    base: Option<&SkillOverrides>,
    policy: Option<&SkillOverrides>,
    flag: Option<&SkillOverrides>,
) -> Option<SkillOverride> {
    let name = units(&entry.name);
    let direct_unqualified = entry
        .unqualified_name
        .as_ref()
        .filter(|_| !entry.is_synced_skill || !entry.bare_name_reserved);
    let mut selected = base
        .and_then(|overrides| overrides.get_units(&name))
        .or_else(|| {
            direct_unqualified.and_then(|key| base.and_then(|map| map.get_units(&units(key))))
        });

    let mut candidates = entry.aliases.iter().map(units).collect::<Vec<_>>();
    if let Some(unqualified) = entry.unqualified_name.as_ref() {
        candidates.push(units(unqualified));
    }
    if candidates.is_empty() {
        return selected;
    }

    for tier in [policy, flag].into_iter().flatten() {
        if tier.get_units(&name).is_some() {
            break;
        }
        let candidate_override = candidates
            .iter()
            .filter_map(|candidate| tier.get_units(candidate))
            .fold(None, |selected, candidate| {
                if selected.is_none_or(|current| {
                    skill_override_rank(candidate) > skill_override_rank(current)
                }) {
                    Some(candidate)
                } else {
                    selected
                }
            });
        if candidate_override.is_some_and(|value| {
            selected.is_none_or(|current| skill_override_rank(value) > skill_override_rank(current))
        }) {
            selected = candidate_override;
        }
        if direct_unqualified.is_some_and(|key| tier.get_units(&units(key)).is_some()) {
            break;
        }
    }
    selected
}

/// Apply Native `CC`'s prompt-type and bundled-skill gate to `cte`'s result.
#[must_use]
pub const fn native_skill_listing_mode(
    override_value: Option<SkillOverride>,
    is_plugin_prompt: bool,
    builtin_gate_active: bool,
) -> SkillOverride {
    if is_plugin_prompt {
        return SkillOverride::On;
    }
    let mode = match override_value {
        Some(value) => value,
        None => SkillOverride::On,
    };
    if builtin_gate_active && !matches!(mode, SkillOverride::Off) {
        SkillOverride::UserInvocableOnly
    } else {
        mode
    }
}

/// Native `rQ(name, settings)` gate used by `Xwr` for builtin prompt skills.
/// `remembered_core_skills` is `Some([])` when no policy verdict exists (the
/// source explicitly returns an empty array in that case); `None` represents
/// a verdict whose remembered-core list is nullish and therefore uses Native's
/// fixed fallback catalog.
#[must_use]
pub fn native_builtin_skill_gate_active(
    name: &Source,
    disable_from_env_or_settings: bool,
    policy_allows_bundled_skills: bool,
    remembered_core_skills: Option<&[Source]>,
) -> bool {
    if disable_from_env_or_settings {
        return true;
    }
    if policy_allows_bundled_skills {
        return false;
    }
    let name_units = units(name);
    let is_remembered = match remembered_core_skills {
        Some(remembered) => remembered
            .iter()
            .any(|skill_name| units(skill_name) == name_units),
        None => NATIVE_REMEMBERED_CORE_SKILLS
            .iter()
            .any(|skill_name| skill_name.encode_utf16().eq(name_units.iter().copied())),
    };
    !is_remembered
}

const NATIVE_REMEMBERED_CORE_SKILLS: [&str; 18] = [
    "artifact-diagramming",
    "batch",
    "claude-code-docs",
    "code-review",
    "commit",
    "dataviz",
    "debug",
    "explain-usage",
    "fewer-permission-prompts",
    "keybindings-help",
    "loop",
    "memory-types",
    "plugin-authoring",
    "pr",
    "simplify",
    "update-config",
    "verify",
    "workflow-authoring",
];

const fn skill_override_rank(value: SkillOverride) -> u8 {
    match value {
        SkillOverride::On => 0,
        SkillOverride::NameOnly => 1,
        SkillOverride::UserInvocableOnly => 2,
        SkillOverride::Off => 3,
    }
}

/// Resolve Native `Rwt(cmd, fullCatalog)`. Keep the whole catalog's order:
/// exact canonical names win, otherwise its first user-facing/alias match does.
#[must_use]
pub fn native_display_name(
    entry: &SkillListingEntry,
    full_catalog: &[SkillListingCatalogName],
) -> Source {
    let Some(candidate) = entry.user_facing_name.as_ref() else {
        return entry.name.clone();
    };
    let candidate_units = units(candidate);
    let name_units = units(&entry.name);
    if candidate_units == name_units
        || name_ends_with_qualified_candidate(&name_units, &candidate_units)
        || !valid_native_display_name(candidate)
    {
        return entry.name.clone();
    }

    let exact = full_catalog
        .iter()
        .find(|item| units(&item.name) == candidate_units);
    let resolved = exact.or_else(|| {
        full_catalog.iter().find(|item| {
            item.user_facing_name
                .as_ref()
                .is_some_and(|name| units(name) == candidate_units)
                || item
                    .aliases
                    .iter()
                    .any(|alias| units(alias) == candidate_units)
        })
    });
    if resolved.is_some_and(|item| units(&item.name) == name_units) {
        concatenate(&[&entry.name, &" (".into(), candidate, &")".into()])
    } else {
        entry.name.clone()
    }
}

fn name_ends_with_qualified_candidate(name: &[u16], candidate: &[u16]) -> bool {
    name.len() > candidate.len()
        && name[name.len() - candidate.len() - 1] == b':' as u16
        && name.ends_with(candidate)
}

fn valid_native_display_name(value: &Source) -> bool {
    let value_units = units(value);
    if value_units.is_empty() || value_units.len() > 256 {
        return false;
    }
    let Ok(value) = String::from_utf16(&value_units) else {
        return false;
    };
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(r"^[\p{L}\p{M}\p{N}_:.-]+$")
                .expect("the Native skill-name expression is valid")
        })
        .is_match(&value)
}

/// Live settings read by Native `Awt()` and `bFo()` when a listing is rendered.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SkillListingBudgetSettings {
    /// `settings.skillListingBudgetFraction`; unset uses `0.01`.
    pub budget_fraction: Option<f64>,
    /// `settings.skillListingMaxDescChars`; unset uses `1536` UTF-16 units.
    pub max_desc_chars: Option<f64>,
}

/// One immutable listing view shared by Native prompt rendering and `/context`
/// diagnostics. The host resolves eligibility, mode, aliases, usage priority,
/// and settings before returning this value.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillListingSnapshot {
    /// Eligible prompt commands in the same order used by the Skill tool.
    pub entries: Vec<SkillListingEntry>,
    /// Effective settings admitted by the host for this same snapshot.
    pub budget_settings: SkillListingBudgetSettings,
}

/// Native `J2t` reports whether the configured character budget already fits
/// or requires priority-based description selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillListingBudgetMode {
    /// The capped listing's JavaScript string length is within budget.
    Fits,
    /// The budget requires dropping descriptions by priority.
    Priority,
}

/// Per-command source projection consumed by Native `Osr` and `/context`.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeSkillTokenRow {
    /// Canonical command key used by Native's token map.
    pub canonical_name: Source,
    /// Native `Rwt(cmd, includedCatalog)` label used by `Osr`.
    pub display_name: Source,
    /// `Oo(cmd)` frontmatter name (user-facing name or canonical fallback).
    pub frontmatter_name: Source,
    /// Source and plugin provenance from Native's `m$o` projection.
    pub source: Option<Source>,
    /// Plugin manifest name when the exact producer retained plugin info.
    pub plugin_name: Option<Source>,
    /// Exact Native line passed to the token estimator, retained for auditing.
    pub rendered_line: Source,
    /// `Math.round(rendered_line.length / line_token_divisor)`.
    pub tokens: u64,
}

/// Native `J2t` diagnostics plus `Osr`'s per-row token projection.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeSkillListingDiagnostics {
    /// Descriptions longer than the configured UTF-16 cap, longest first.
    pub capped_skills: Vec<Source>,
    /// Character-budget selection mode.
    pub budget_mode: SkillListingBudgetMode,
    /// Commands whose descriptions do not fit under J2t's JS-length budget,
    /// longest selected description first.
    pub budget_truncated_skills: Vec<Source>,
    /// Total capped listing length in JavaScript UTF-16 code units.
    pub total_chars: usize,
    /// Uncapped listing length in JavaScript UTF-16 code units.
    pub raw_total_chars: usize,
    /// Native `ntt` budget, including source-faithful non-finite/negative values.
    pub budget: f64,
    /// Whether a positive valid `SLASH_COMMAND_TOOL_CHAR_BUDGET` selected it.
    pub budget_from_env: bool,
    /// Native `J2t` bytes-per-token input used for budget calculation.
    pub bytes_per_token: usize,
    /// The other-model `Eh(model)` divisor used by `Osr`'s line token count.
    pub line_token_divisor: usize,
    /// Ordered rows; `rendered_line` is diagnostic evidence, not an exposed
    /// Native `skillFrontmatter` property.
    pub rows: Vec<NativeSkillTokenRow>,
}

/// Supplies the model-invocable skill entries for the per-turn `skill_listing`
/// reminder. The production impl wraps the shared `CommandRegistry`; tests
/// inject a static fixture. Mirrors TS `getSkillToolCommands(cwd)`
/// (`commands.ts:565`).
#[async_trait]
pub trait SkillListingProvider: Send + Sync {
    /// Return one complete view so entries, mode, priority, and settings do
    /// not come from separate registry/settings reads.
    async fn skill_listing_snapshot(&self) -> SkillListingSnapshot;

    /// Snapshot precommit suggestions using the same eligible skill catalog.
    async fn bash_precommit_skills(
        &self,
        _entries: &[SkillListingEntry],
    ) -> tool_api::tool_trait::BashPrecommitSkills {
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
        Arc<dyn Fn() -> Pin<Box<dyn Future<Output = SkillListingSnapshot> + Send>> + Send + Sync>,
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
    /// Wrap an async, atomic snapshot loader as a [`SkillListingProvider`].
    pub fn new<F, Fut>(loader: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = SkillListingSnapshot> + Send + 'static,
    {
        let loader = Arc::new(
            move || -> Pin<Box<dyn Future<Output = SkillListingSnapshot> + Send>> {
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
    async fn skill_listing_snapshot(&self) -> SkillListingSnapshot {
        (self.loader)().await
    }

    async fn bash_precommit_skills(
        &self,
        entries: &[SkillListingEntry],
    ) -> tool_api::tool_trait::BashPrecommitSkills {
        let Some(loader) = self.precommit_loader.as_ref() else {
            return tool_api::tool_trait::BashPrecommitSkills::default();
        };
        loader(entries.to_vec()).await
    }
}

// Native prompt.ts:20-29
const SKILL_BUDGET_CONTEXT_PERCENT: f64 = 0.01;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
const DEFAULT_MAX_DESC_CHARS: f64 = 1_536.0;

/// Native `ntt(context, charsPerToken)`. The environment override is selected
/// before current settings and model-derived context are considered.
fn char_budget(
    context_window_tokens: Option<usize>,
    chars_per_token: usize,
    settings: SkillListingBudgetSettings,
) -> f64 {
    if let Some(value) = std::env::var("SLASH_COMMAND_TOOL_CHAR_BUDGET")
        .ok()
        .and_then(|value| parse_native_budget_env(&value))
        .filter(|value| *value != 0.0)
    {
        return value;
    }
    let context = context_window_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
    let fraction = settings
        .budget_fraction
        .unwrap_or(SKILL_BUDGET_CONTEXT_PERCENT);
    let calculated = (context as f64) * (chars_per_token as f64) * fraction;
    if calculated.is_nan() {
        f64::NAN
    } else {
        calculated.floor().max(1.0)
    }
}

/// Reproduce Native `J2t` diagnostics and `Osr`'s line-to-token accounting.
/// This intentionally uses JavaScript UTF-16 `.length` for diagnostics and
/// tokens; the prompt listing renderer separately uses Bun terminal width.
/// `entries` must be the same mode-filtered snapshot passed to the prompt
/// renderer, with `display_name` and `priority` already resolved by the host.
#[must_use]
pub fn native_skill_listing_diagnostics(
    entries: &[SkillListingEntry],
    context_window_tokens: Option<usize>,
    budget_chars_per_token: usize,
    settings: SkillListingBudgetSettings,
    line_token_divisor: usize,
) -> NativeSkillListingDiagnostics {
    assert!(
        line_token_divisor > 0,
        "Native token divisor must be positive"
    );

    struct BudgetEntry {
        name_length: usize,
        desc_length: usize,
        entry_length: usize,
        raw_length: usize,
    }

    let included_catalog = entries
        .iter()
        .map(|entry| SkillListingCatalogName {
            name: entry.name.clone(),
            user_facing_name: entry.user_facing_name.clone(),
            aliases: entry.aliases.clone(),
        })
        .collect::<Vec<_>>();
    // J2t/Osr resolve Rwt against the included diagnostic list, while eYt
    // resolves against the full registry catalog. Keep these source scopes
    // separate even though both use the same Rwt helper.
    let diagnostic_display_names = entries
        .iter()
        .map(|entry| native_display_name(entry, &included_catalog))
        .collect::<Vec<_>>();
    let max_desc_chars = settings.max_desc_chars.unwrap_or(DEFAULT_MAX_DESC_CHARS);
    let measured: Vec<BudgetEntry> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            if entry.is_name_only {
                let name_length = units(&entry.name).len();
                return BudgetEntry {
                    name_length,
                    desc_length: 0,
                    entry_length: name_length + 2,
                    raw_length: name_length + 2,
                };
            }
            let name_length = units(&diagnostic_display_names[index]).len();
            let description_length = units(&skill_description(entry)).len();
            let desc_length =
                description_length.min(js_slice_end(description_length, max_desc_chars));
            BudgetEntry {
                name_length,
                desc_length,
                entry_length: name_length + 4 + desc_length,
                raw_length: name_length + 4 + description_length,
            }
        })
        .collect();
    let separators = entries.len().saturating_sub(1);
    let total_chars = measured
        .iter()
        .map(|entry| entry.entry_length)
        .sum::<usize>()
        + separators;
    let raw_total_chars = measured.iter().map(|entry| entry.raw_length).sum::<usize>() + separators;

    let mut capped = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            if entry.is_name_only {
                return None;
            }
            let description_length = units(&skill_description(entry)).len();
            (description_length as f64 > max_desc_chars).then_some((index, description_length))
        })
        .collect::<Vec<_>>();
    capped.sort_by(|left, right| right.1.cmp(&left.1));
    let capped_skills = capped
        .into_iter()
        .map(|(index, _)| entries[index].name.clone())
        .collect();

    let budget = char_budget(context_window_tokens, budget_chars_per_token, settings);
    let budget_from_env = std::env::var("SLASH_COMMAND_TOOL_CHAR_BUDGET")
        .ok()
        .and_then(|value| parse_native_budget_env(&value))
        .is_some_and(|value| value > 0.0);
    let mut truncated_indices = Vec::new();
    let budget_mode = if total_chars as f64 <= budget {
        SkillListingBudgetMode::Fits
    } else {
        let protected =
            |index: usize| entries[index].is_name_only || entries[index].is_bundled_prompt;
        let reserved_chars = measured
            .iter()
            .enumerate()
            .map(|(index, measured)| {
                if protected(index) {
                    measured.entry_length
                } else {
                    measured.name_length + 2
                }
            })
            .sum::<usize>()
            + separators;
        let mut remaining = budget - reserved_chars as f64;
        let mut candidates = (0..entries.len())
            .filter(|index| !protected(*index))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            entries[*right]
                .priority
                .partial_cmp(&entries[*left].priority)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut kept = vec![false; entries.len()];
        for index in candidates.iter().copied() {
            let description_cost = measured[index].entry_length - (measured[index].name_length + 2);
            if description_cost as f64 <= remaining {
                kept[index] = true;
                remaining -= description_cost as f64;
            }
        }
        truncated_indices = candidates
            .into_iter()
            .filter(|index| !kept[*index])
            .collect();
        truncated_indices.sort_by(|left, right| {
            measured[*right]
                .desc_length
                .cmp(&measured[*left].desc_length)
        });
        SkillListingBudgetMode::Priority
    };
    let budget_truncated_skills = truncated_indices
        .iter()
        .map(|index| entries[*index].name.clone())
        .collect::<Vec<_>>();
    let truncated_names = truncated_indices
        .iter()
        .map(|index| units(&entries[*index].name))
        .collect::<std::collections::HashSet<_>>();

    let rows = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let line = if entry.is_name_only {
                canonical_name_line(entry)
            } else if truncated_names.contains(&units(&entry.name)) {
                display_name_line(&diagnostic_display_names[index])
            } else {
                let description = slice_description(&skill_description(entry), max_desc_chars);
                concatenate(&[
                    &"- ".into(),
                    &diagnostic_display_names[index],
                    &": ".into(),
                    &description,
                ])
            };
            let line_length = units(&line).len();
            let tokens = ((line_length as f64) / (line_token_divisor as f64)).round() as u64;
            NativeSkillTokenRow {
                canonical_name: entry.name.clone(),
                display_name: diagnostic_display_names[index].clone(),
                frontmatter_name: entry
                    .user_facing_name
                    .clone()
                    .unwrap_or_else(|| entry.name.clone()),
                source: entry.source.clone(),
                plugin_name: entry.plugin_name.clone(),
                rendered_line: line,
                tokens,
            }
        })
        .collect();

    NativeSkillListingDiagnostics {
        capped_skills,
        budget_mode,
        budget_truncated_skills,
        total_chars,
        raw_total_chars,
        budget,
        budget_from_env,
        bytes_per_token: budget_chars_per_token,
        line_token_divisor,
        rows,
    }
}

/// Native `lj` environment-number parser. JavaScript `Number` runs first;
/// only a NaN result falls back to the source's grouped-decimal syntax.
fn parse_native_budget_env(value: &str) -> Option<f64> {
    let number = javascript_number(value);
    if !number.is_nan() {
        return Some(number);
    }

    let trimmed = lingxi_core::host::effort::trim_js_whitespace(value);
    if trimmed.encode_utf16().count() > 32 {
        return None;
    }
    parse_grouped_budget_number(trimmed)
}

fn javascript_number(value: &str) -> f64 {
    let value = lingxi_core::host::effort::trim_js_whitespace(value);
    if value.is_empty() {
        return 0.0;
    }
    if value == "Infinity" || value == "+Infinity" {
        return f64::INFINITY;
    }
    if value == "-Infinity" {
        return f64::NEG_INFINITY;
    }

    let (radix, digits) = if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (16_u32, digits)
    } else if let Some(digits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        (2_u32, digits)
    } else if let Some(digits) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        (8_u32, digits)
    } else {
        (0, "")
    };
    if radix != 0 {
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut number = 0.0;
        for digit in digits.chars() {
            let Some(value) = digit.to_digit(radix) else {
                return f64::NAN;
            };
            number = number * f64::from(radix) + f64::from(value);
        }
        return number;
    }

    if !is_javascript_decimal(value) {
        return f64::NAN;
    }
    value.parse().unwrap_or(f64::NAN)
}

fn is_javascript_decimal(value: &str) -> bool {
    let value = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    let (mantissa, exponent) = match value.find(['e', 'E']) {
        Some(index) if !value[index + 1..].contains('e') && !value[index + 1..].contains('E') => {
            (&value[..index], Some(&value[index + 1..]))
        }
        Some(_) => return false,
        None => (value, None),
    };
    if let Some(exponent) = exponent {
        let exponent = exponent
            .strip_prefix('+')
            .or_else(|| exponent.strip_prefix('-'))
            .unwrap_or(exponent);
        if exponent.is_empty() || !exponent.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
    }

    let mut dot = false;
    let mut digit = false;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => digit = true,
            b'.' if !dot => dot = true,
            _ => return false,
        }
    }
    digit
}

/// Source regex `^[+-]?\\d{1,3}([_,\\u00A0\\u202F ])\\d{3}(?:\\1\\d{3})*$`.
fn parse_grouped_budget_number(value: &str) -> Option<f64> {
    let (sign, unsigned) = if let Some(rest) = value.strip_prefix('-') {
        (-1.0, rest)
    } else if let Some(rest) = value.strip_prefix('+') {
        (1.0, rest)
    } else {
        (1.0, value)
    };
    let units = unsigned.encode_utf16().collect::<Vec<_>>();
    let separator_index = units
        .iter()
        .position(|unit| is_budget_group_separator(*unit))?;
    let separator = units[separator_index];
    let first_group = &units[..separator_index];
    if !(1..=3).contains(&first_group.len())
        || !first_group
            .iter()
            .all(|unit| (b'0' as u16..=b'9' as u16).contains(unit))
    {
        return None;
    }

    let mut digits = first_group.to_vec();
    let mut group_len = 0;
    for unit in &units[separator_index + 1..] {
        if *unit == separator {
            if group_len != 3 {
                return None;
            }
            group_len = 0;
        } else if (b'0' as u16..=b'9' as u16).contains(unit) {
            group_len += 1;
            digits.push(*unit);
        } else {
            return None;
        }
    }
    if group_len != 3 {
        return None;
    }
    let digits = String::from_utf16(&digits).ok()?;
    Some(sign * digits.parse::<f64>().ok()?)
}

fn is_budget_group_separator(unit: u16) -> bool {
    unit == b'_' as u16
        || unit == b',' as u16
        || unit == b' ' as u16
        || unit == 0x00a0
        || unit == 0x202f
}

/// Native `Twt(cmd)`. Guidance remains part of the same source string.
fn skill_description(e: &SkillListingEntry) -> Source {
    match &e.when_to_use {
        Some(w) if !units(w).is_empty() => concatenate(&[&e.description, &" - ".into(), w]),
        _ => e.description.clone(),
    }
}

fn name_only_line(e: &SkillListingEntry) -> Source {
    if e.is_name_only {
        return canonical_name_line(e);
    }
    display_name_line(&e.display_name)
}

fn canonical_name_line(e: &SkillListingEntry) -> Source {
    display_name_line(&e.name)
}

fn display_name_line(display_name: &Source) -> Source {
    concatenate(&[&"- ".into(), display_name])
}

/// Native `vFo`: cap by JavaScript UTF-16 string length, slicing code units and
/// then appending U+2026. A cap boundary may intentionally split a surrogate.
fn cap_description(source: &Source, max_desc_chars: f64) -> Source {
    let original = units(source);
    if (original.len() as f64).partial_cmp(&max_desc_chars) != Some(std::cmp::Ordering::Greater) {
        return source.clone();
    }
    let end = js_slice_end(original.len(), max_desc_chars - 1.0);
    let mut capped = original[..end].to_vec();
    capped.push(0x2026);
    from_units(capped)
}

/// Native `Osr` uses `Twt(cmd).slice(0, maxDescChars)` without `vFo`'s
/// ellipsis. The original UTF-16 units remain intact, including a split pair.
fn slice_description(source: &Source, max_desc_chars: f64) -> Source {
    let original = units(source);
    from_units(original[..js_slice_end(original.len(), max_desc_chars)].to_vec())
}

/// JavaScript `String.prototype.slice(0, end)` end-index coercion.
fn js_slice_end(length: usize, end: f64) -> usize {
    if end.is_nan() || end == f64::NEG_INFINITY {
        return 0;
    }
    if end == f64::INFINITY {
        return length;
    }
    let integer = end.trunc();
    let length = length as f64;
    if integer < 0.0 {
        (length + integer).clamp(0.0, length) as usize
    } else {
        integer.clamp(0.0, length) as usize
    }
}

fn full_line(e: &SkillListingEntry, max_desc_chars: f64) -> Source {
    if e.is_name_only {
        return name_only_line(e);
    }
    let description = cap_description(&skill_description(e), max_desc_chars);
    concatenate(&[&"- ".into(), &e.display_name, &": ".into(), &description])
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

// Native `ae(text)` is `Bun.stringWidth(text, { ambiguousIsNarrow: true })`.
// Invalid UTF-16 units and terminal control sequences have no measured width;
// they stay untouched in the emitted source string.
fn measurement_text(units: &[u16]) -> String {
    let decoded = char::decode_utf16(units.iter().copied())
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    strip_terminal_controls(&decoded)
}

fn strip_terminal_controls(text: &[char]) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        EscapeIntermediate,
        Csi,
        StringControl,
        StringControlEscape,
    }

    let mut state = State::Text;
    let mut output = String::with_capacity(text.len());
    for ch in text.iter().copied() {
        state = match state {
            State::Text => {
                if ch == '\u{1b}' {
                    State::Escape
                } else if ch == '\u{9b}' {
                    State::Csi
                } else if matches!(ch, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}') {
                    State::StringControl
                } else if ch.is_control() {
                    State::Text
                } else {
                    output.push(ch);
                    State::Text
                }
            }
            State::Escape => match ch {
                '[' => State::Csi,
                ']' | 'P' | 'X' | '^' | '_' => State::StringControl,
                ' '..='/' => State::EscapeIntermediate,
                _ => State::Text,
            },
            State::EscapeIntermediate => {
                if (' '..='/').contains(&ch) {
                    State::EscapeIntermediate
                } else if ('0'..='~').contains(&ch) {
                    State::Text
                } else {
                    State::Escape
                }
            }
            State::Csi => {
                if ('@'..='~').contains(&ch) {
                    State::Text
                } else {
                    State::Csi
                }
            }
            State::StringControl => {
                if ch == '\u{7}' || ch == '\u{9c}' {
                    State::Text
                } else if ch == '\u{1b}' {
                    State::StringControlEscape
                } else {
                    State::StringControl
                }
            }
            State::StringControlEscape => {
                if ch == '\\' {
                    State::Text
                } else if ch == '\u{1b}' {
                    State::StringControlEscape
                } else {
                    State::StringControl
                }
            }
        };
    }
    output
}

fn units_width(units: &[u16]) -> usize {
    let text = measurement_text(units);
    UnicodeWidthStr::width(text.as_str())
}

fn string_width(source: &Source) -> usize {
    units_width(&units(source))
}

/// Native 2.1.295 `eYt`. Descriptions with the highest usage score are kept
/// whole until the remaining budget is spent; other skills remain visible by
/// name only. The returned lines always retain catalog order.
#[must_use]
pub fn format_within_budget(
    entries: &[SkillListingEntry],
    context_window_tokens: Option<usize>,
    chars_per_token: usize,
    settings: SkillListingBudgetSettings,
) -> Source {
    if entries.is_empty() {
        return "".into();
    }
    let budget = char_budget(context_window_tokens, chars_per_token, settings);
    let max_desc_chars = settings.max_desc_chars.unwrap_or(DEFAULT_MAX_DESC_CHARS);
    let full: Vec<Source> = entries
        .iter()
        .map(|entry| full_line(entry, max_desc_chars))
        .collect();
    let name_only: Vec<Source> = entries.iter().map(name_only_line).collect();
    let full_total: usize = full.iter().map(string_width).sum::<usize>() + entries.len() - 1;
    if full_total as f64 <= budget {
        return join_lines(&full);
    }
    tracing::warn!(
        "Skill listing over budget: {} skills, {} chars > {} budget — descriptions will be truncated. Run /skills to disable some, or raise skillListingBudgetFraction in settings.",
        entries.len(),
        full_total,
        budget
    );

    let protected = |entry: &SkillListingEntry| entry.is_name_only || entry.is_bundled_prompt;
    let mut remaining = budget
        - entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                if protected(entry) {
                    string_width(&full[index])
                } else {
                    string_width(&name_only[index])
                }
            })
            .sum::<usize>() as f64
        - (entries.len() - 1) as f64;
    let mut candidates: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| (!protected(entry)).then_some(index))
        .collect();
    // `Array.prototype.sort` is stable; NaN comparator results are equal.
    candidates.sort_by(|left, right| {
        entries[*right]
            .priority
            .partial_cmp(&entries[*left].priority)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut keep_descriptions = vec![false; entries.len()];
    for index in candidates {
        let cost = string_width(&full[index]) as f64 - string_width(&name_only[index]) as f64;
        if cost <= remaining {
            keep_descriptions[index] = true;
            remaining -= cost;
        }
    }
    join_lines(
        &entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                if protected(entry) || keep_descriptions[index] {
                    full[index].clone()
                } else {
                    name_only[index].clone()
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
    chars_per_token: usize,
    settings: SkillListingBudgetSettings,
) -> Option<Source> {
    let body = format_within_budget(entries, context_window_tokens, chars_per_token, settings);
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
    use std::sync::MutexGuard;

    const CHARS_PER_TOKEN: usize = 4;

    // Serializes tests that mutate the process-global budget override and
    // restores its exact prior value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct BudgetEnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl BudgetEnvGuard {
        fn set(value: Option<&str>) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
            let previous = std::env::var_os("SLASH_COMMAND_TOOL_CHAR_BUDGET");
            if let Some(value) = value {
                std::env::set_var("SLASH_COMMAND_TOOL_CHAR_BUDGET", value);
            } else {
                std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
            }
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for BudgetEnvGuard {
        fn drop(&mut self) {
            if let Some(value) = self.previous.take() {
                std::env::set_var("SLASH_COMMAND_TOOL_CHAR_BUDGET", value);
            } else {
                std::env::remove_var("SLASH_COMMAND_TOOL_CHAR_BUDGET");
            }
        }
    }

    fn entry(name: &str, desc: &str, when: Option<&str>, bundled: bool) -> SkillListingEntry {
        SkillListingEntry {
            name: name.into(),
            display_name: name.into(),
            user_facing_name: None,
            unqualified_name: None,
            is_synced_skill: false,
            bare_name_reserved: false,
            aliases: Vec::new(),
            source: Some("user".into()),
            plugin_name: None,
            description: desc.into(),
            when_to_use: when.map(Into::into),
            is_bundled_prompt: bundled,
            is_name_only: false,
            priority: 0.0,
        }
    }

    fn format(entries: &[SkillListingEntry], context: Option<usize>) -> Source {
        format_within_budget(
            entries,
            context,
            CHARS_PER_TOKEN,
            SkillListingBudgetSettings::default(),
        )
    }

    fn source(value: lingxi_core::types::utf16_json::JsonValueView<'_>) -> Source {
        from_units(value.code_units().expect("oracle fixture string").to_vec())
    }

    fn optional_source(
        value: lingxi_core::types::utf16_json::JsonValueView<'_>,
        key: &str,
    ) -> Option<Source> {
        value.get_property(key).map(source)
    }

    fn array_values<'a>(
        value: lingxi_core::types::utf16_json::JsonValueView<'a>,
    ) -> Vec<lingxi_core::types::utf16_json::JsonValueView<'a>> {
        let len = value.array_len().expect("oracle fixture array");
        (0..len)
            .map(|index| value.get_index(index).expect("in-range array item"))
            .collect()
    }

    fn string_field<'a>(
        value: lingxi_core::types::utf16_json::JsonValueView<'a>,
        key: &str,
    ) -> Option<&'a str> {
        value.get_property(key).and_then(|field| field.as_str())
    }

    fn number_field(
        value: lingxi_core::types::utf16_json::JsonValueView<'_>,
        key: &str,
    ) -> Option<f64> {
        value.get_property(key).and_then(|field| field.as_number())
    }

    fn optional_string_units(
        value: lingxi_core::types::utf16_json::JsonValueView<'_>,
        key: &str,
    ) -> Option<Vec<u16>> {
        value
            .get_property(key)
            .and_then(|field| field.code_units().map(<[u16]>::to_vec))
    }

    fn settings_at(
        fixture: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        pointer: &str,
    ) -> Option<lingxi_core::settings::SettingsJson> {
        let projection = fixture.subprojection(pointer).ok()?;
        let json = projection.to_json_string().ok()?;
        serde_json::from_str(&json).ok()
    }

    fn catalog_names(
        value: lingxi_core::types::utf16_json::JsonValueView<'_>,
    ) -> Vec<SkillListingCatalogName> {
        array_values(value)
            .into_iter()
            .map(|item| SkillListingCatalogName {
                name: source(item.get_property("name").expect("catalog name")),
                user_facing_name: optional_source(item, "userFacingName"),
                aliases: item
                    .get_property("aliases")
                    .map(array_values)
                    .unwrap_or_default()
                    .into_iter()
                    .map(source)
                    .collect(),
            })
            .collect()
    }

    fn oracle_entries(
        value: lingxi_core::types::utf16_json::JsonValueView<'_>,
        catalog: &[SkillListingCatalogName],
        settings: &lingxi_core::settings::SettingsJson,
        policy: Option<&lingxi_core::settings::SettingsJson>,
        flag: Option<&lingxi_core::settings::SettingsJson>,
    ) -> Vec<SkillListingEntry> {
        use lingxi_core::settings::SkillOverride;

        array_values(value)
            .into_iter()
            .filter_map(|item| {
                let name = source(item.get_property("name").expect("entry name"));
                let raw_source = string_field(item, "source");
                let native_source = raw_source.map(|value| {
                    if string_field(item, "loadedFrom") == Some("syncedSkills") {
                        "syncedSkills"
                    } else if matches!(value, "builtin" | "bundled") {
                        "built-in"
                    } else {
                        value
                    }
                });
                let mut entry = SkillListingEntry {
                    name: name.clone(),
                    display_name: name,
                    user_facing_name: optional_source(item, "userFacingName"),
                    unqualified_name: optional_source(item, "unqualifiedName"),
                    is_synced_skill: string_field(item, "loadedFrom") == Some("syncedSkills"),
                    bare_name_reserved: item
                        .get_property("bareNameReserved")
                        .and_then(|field| field.as_bool())
                        .unwrap_or(false),
                    aliases: item
                        .get_property("aliases")
                        .map(array_values)
                        .unwrap_or_default()
                        .into_iter()
                        .map(source)
                        .collect(),
                    source: native_source.map(|value| value.into()),
                    plugin_name: item
                        .get_property("pluginInfo")
                        .and_then(|plugin_info| plugin_info.get_property("pluginManifest"))
                        .and_then(|manifest| manifest.get_property("name"))
                        .map(source),
                    description: source(item.get_property("description").expect("description")),
                    when_to_use: optional_source(item, "whenToUse"),
                    is_bundled_prompt: string_field(item, "source") == Some("bundled"),
                    is_name_only: false,
                    priority: 0.0,
                };
                let is_plugin_prompt = string_field(item, "source") == Some("plugin");
                let mode = native_skill_listing_mode(
                    native_skill_override(
                        &entry,
                        settings.skill_overrides.as_ref(),
                        policy.and_then(|settings| settings.skill_overrides.as_ref()),
                        flag.and_then(|settings| settings.skill_overrides.as_ref()),
                    ),
                    is_plugin_prompt,
                    false,
                );
                if matches!(mode, SkillOverride::Off | SkillOverride::UserInvocableOnly) {
                    return None;
                }
                entry.is_name_only = mode == SkillOverride::NameOnly;
                entry.display_name = native_display_name(&entry, catalog);
                Some(entry)
            })
            .collect()
    }

    #[test]
    fn matches_all_pinned_295_formatter_oracle_outputs() {
        let fixture = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(include_str!(
            "../../tests/fixtures/native_skill_listing_295_goldens.json"
        ))
        .expect("valid source-extracted Native formatter fixture");
        let diagnostics_fixture = lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
            include_str!("../../tests/fixtures/native_skill_listing_295_diagnostics.json"),
        )
        .expect("valid source-extracted Native diagnostics fixture");
        assert_eq!(
            fixture.string_at("/targetSha256").expect("target digest"),
            "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
        );
        assert_eq!(
            fixture
                .string_at("/sourceChunkSha256")
                .expect("source digest"),
            "ae64b2c8c87be0e990738da478adbbe5cd6a8201cef7953d7684cf59bd780120"
        );
        let cases = fixture.array_len("/cases").expect("oracle cases");
        assert_eq!(cases, 21);
        assert_eq!(diagnostics_fixture.array_len("/cases"), Some(cases));

        for case_index in 0..cases {
            let case_pointer = format!("/cases/{case_index}");
            let input_pointer = format!("{case_pointer}/input");
            let case = fixture
                .subprojection(&case_pointer)
                .expect("case projection");
            let diagnostics_case = diagnostics_fixture
                .subprojection(&case_pointer)
                .expect("diagnostics case projection");
            let input = fixture
                .subprojection(&input_pointer)
                .expect("input projection");
            let _env = BudgetEnvGuard::set(input.string_at("/env"));
            let base_settings = settings_at(&fixture, &format!("{input_pointer}/settings"))
                .expect("valid Native oracle settings");
            let policy_settings = input
                .get_property("layers")
                .and_then(|layers| layers.get_property("policySettings"))
                .and_then(|_| {
                    settings_at(&fixture, &format!("{input_pointer}/layers/policySettings"))
                });
            let flag_settings = input
                .get_property("layers")
                .and_then(|layers| layers.get_property("flagSettings"))
                .and_then(|_| {
                    settings_at(&fixture, &format!("{input_pointer}/layers/flagSettings"))
                });
            let catalog = catalog_names(
                input
                    .get_property("fullCatalog")
                    .expect("fullCatalog array"),
            );
            let mut entries = oracle_entries(
                input.get_property("entries").expect("entries array"),
                &catalog,
                &base_settings,
                policy_settings.as_ref(),
                flag_settings.as_ref(),
            );
            if let Some(priorities) = input
                .get_property("priority")
                .and_then(|value| value.object_entries())
            {
                for entry in &mut entries {
                    let name_units = units(&entry.name);
                    if let Some(priority) = priorities
                        .iter()
                        .find(|candidate| candidate.code_units == name_units)
                    {
                        entry.priority = priority.value.as_number().unwrap_or(0.0);
                    }
                }
            }
            let budget_settings = SkillListingBudgetSettings {
                budget_fraction: base_settings.skill_listing_budget_fraction,
                max_desc_chars: base_settings.skill_listing_max_desc_chars,
            };
            let actual = format_within_budget(
                &entries,
                input
                    .get_property("contextWindow")
                    .and_then(|value| value.as_number())
                    .map(|value| value as usize),
                input
                    .get_property("charsPerToken")
                    .and_then(|value| value.as_number())
                    .expect("charsPerToken") as usize,
                budget_settings,
            );
            assert_eq!(
                units(&actual),
                case.string_units("/output").expect("oracle raw output"),
                "case {}",
                case.string_at("/id").expect("case id")
            );

            let line_token_divisor = diagnostics_case
                .number_at("/ehBytesPerToken")
                .expect("Osr model divisor") as usize;
            let diagnostics = native_skill_listing_diagnostics(
                &entries,
                input
                    .get_property("contextWindow")
                    .and_then(|value| value.as_number())
                    .map(|value| value as usize),
                input
                    .get_property("charsPerToken")
                    .and_then(|value| value.as_number())
                    .expect("J2t charsPerToken") as usize,
                budget_settings,
                line_token_divisor,
            );
            assert_eq!(
                diagnostics.line_token_divisor as f64,
                diagnostics_case.number_at("/ehBytesPerToken").unwrap(),
                "Osr model divisor case {}",
                diagnostics_case.string_at("/id").unwrap()
            );
            let expected_diagnostics = diagnostics_case.get_property("j2t").expect("J2t output");
            let units_array = |value| {
                array_values(value)
                    .into_iter()
                    .map(|entry| units(&source(entry)))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                diagnostics
                    .capped_skills
                    .iter()
                    .map(units)
                    .collect::<Vec<_>>(),
                units_array(expected_diagnostics.get_property("cappedSkills").unwrap()),
                "cappedSkills case {}",
                diagnostics_case.string_at("/id").unwrap()
            );
            let expected_mode = string_field(expected_diagnostics, "budgetMode").unwrap();
            assert_eq!(
                diagnostics.budget_mode,
                match expected_mode {
                    "fits" => SkillListingBudgetMode::Fits,
                    "priority" => SkillListingBudgetMode::Priority,
                    other => panic!("unknown Native budget mode {other:?}"),
                },
                "budgetMode case {}",
                diagnostics_case.string_at("/id").unwrap()
            );
            assert_eq!(
                diagnostics
                    .budget_truncated_skills
                    .iter()
                    .map(units)
                    .collect::<Vec<_>>(),
                units_array(
                    expected_diagnostics
                        .get_property("budgetTruncatedSkills")
                        .unwrap()
                ),
                "budgetTruncatedSkills case {}",
                diagnostics_case.string_at("/id").unwrap()
            );
            assert_eq!(
                diagnostics.total_chars as f64,
                number_field(expected_diagnostics, "totalChars").unwrap()
            );
            assert_eq!(
                diagnostics.raw_total_chars as f64,
                number_field(expected_diagnostics, "rawTotalChars").unwrap()
            );
            assert_eq!(
                diagnostics.budget,
                number_field(expected_diagnostics, "budget").unwrap()
            );
            assert_eq!(
                diagnostics.budget_from_env,
                expected_diagnostics
                    .get_property("budgetFromEnv")
                    .and_then(|value| value.as_bool())
                    .unwrap()
            );
            assert_eq!(
                diagnostics.bytes_per_token as f64,
                number_field(expected_diagnostics, "bytesPerToken").unwrap()
            );

            let osr_rows = diagnostics_case
                .get_property("osr")
                .map(array_values)
                .expect("Osr rows");
            assert_eq!(diagnostics.rows.len(), osr_rows.len());
            for (actual_row, expected_row) in diagnostics.rows.iter().zip(osr_rows) {
                assert_eq!(
                    units(&actual_row.canonical_name),
                    expected_row
                        .get_property("name")
                        .and_then(|value| value.code_units().map(<[u16]>::to_vec))
                        .unwrap()
                );
                assert_eq!(
                    units(&actual_row.rendered_line),
                    expected_row
                        .get_property("line")
                        .and_then(|value| value.code_units().map(<[u16]>::to_vec))
                        .unwrap()
                );
                assert_eq!(
                    actual_row.tokens as f64,
                    number_field(expected_row, "tokens").unwrap()
                );
                assert_eq!(
                    diagnostics.line_token_divisor as f64,
                    number_field(expected_row, "bytesPerToken").unwrap()
                );
            }

            let frontmatter_rows = diagnostics_case
                .get_property("skillFrontmatter")
                .map(array_values)
                .expect("skillFrontmatter rows");
            assert_eq!(diagnostics.rows.len(), frontmatter_rows.len());
            for (actual_row, expected_row) in diagnostics.rows.iter().zip(frontmatter_rows) {
                assert_eq!(
                    units(&actual_row.frontmatter_name),
                    expected_row
                        .get_property("name")
                        .and_then(|value| value.code_units().map(<[u16]>::to_vec))
                        .unwrap()
                );
                assert_eq!(
                    actual_row.source.as_ref().map(units),
                    optional_string_units(expected_row, "source")
                );
                assert_eq!(
                    actual_row.plugin_name.as_ref().map(units),
                    optional_string_units(expected_row, "pluginName")
                );
                assert_eq!(
                    actual_row.tokens as f64,
                    number_field(expected_row, "tokens").unwrap()
                );
            }
        }
    }

    fn render(entries: &[SkillListingEntry], context: Option<usize>) -> Option<Source> {
        render_reminder(
            entries,
            context,
            CHARS_PER_TOKEN,
            SkillListingBudgetSettings::default(),
        )
    }

    #[tokio::test]
    async fn lazy_provider_invokes_loader_for_each_listing_request() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_loader = calls.clone();
        let provider = LazySkillListingProvider::new(move || {
            calls_for_loader.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                SkillListingSnapshot {
                    entries: vec![entry("live", "loaded from the current source", None, false)],
                    budget_settings: SkillListingBudgetSettings::default(),
                }
            }
        });

        assert_eq!(
            provider.skill_listing_snapshot().await.entries[0]
                .name
                .string_at(""),
            Some("live")
        );
        assert_eq!(
            provider.skill_listing_snapshot().await.entries[0]
                .name
                .string_at(""),
            Some("live")
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn empty_entries_render_none() {
        assert_eq!(render(&[], Some(200_000)), None);
        assert_eq!(format(&[], None).string_at(""), Some(""));
    }

    #[test]
    fn renders_byte_exact_system_reminder() {
        let _g = BudgetEnvGuard::set(None);
        let entries = vec![
            entry("debug", "Debug a failing test", None, false),
            entry(
                "loop",
                "Run a prompt on a loop",
                Some("for repeated tasks"),
                false,
            ),
        ];
        let got = render(&entries, Some(200_000)).expect("some");
        let want = "<system-reminder>\n\
            The following skills are available for use with the Skill tool:\n\n\
            - debug: Debug a failing test\n\
            - loop: Run a prompt on a loop - for repeated tasks\n\
            </system-reminder>";
        assert_eq!(got.string_at(""), Some(want));
    }

    #[test]
    fn exact_and_under_budget_boundaries_match_native_golden() {
        let exact = BudgetEnvGuard::set(Some("9"));
        let line = [entry("one", "ab", None, false)];
        assert_eq!(format(&line, Some(100)).string_at(""), Some("- one: ab"));
        drop(exact);

        let _under = BudgetEnvGuard::set(Some("8"));
        assert_eq!(format(&line, Some(100)).string_at(""), Some("- one"));
    }

    #[test]
    fn default_single_description_cap_is_1536_utf16_units() {
        let _g = BudgetEnvGuard::set(Some("10000"));
        let long = "A".repeat(1_600);
        let rendered = format(&[entry("max", &long, None, false)], Some(100));
        let units = units(&rendered);
        assert_eq!(units.len(), "- max: ".encode_utf16().count() + 1_536);
        assert_eq!(units.last(), Some(&0x2026));
        assert_eq!(
            &units[7..],
            &[vec![b'A' as u16; 1_535], vec![0x2026]].concat()
        );
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
    fn native_display_name_uses_full_catalog_identity_and_order() {
        let mut skill = entry("plugin:review-skill", "desc", None, false);
        skill.user_facing_name = Some("review".into());
        let matching_catalog = [SkillListingCatalogName {
            name: skill.name.clone(),
            user_facing_name: skill.user_facing_name.clone(),
            aliases: vec!["review".into()],
        }];
        assert_eq!(
            native_display_name(&skill, &matching_catalog).string_at(""),
            Some("plugin:review-skill (review)")
        );

        let exact_name_wins_over_earlier_alias = [
            SkillListingCatalogName {
                name: "other".into(),
                user_facing_name: Some("review".into()),
                aliases: vec![],
            },
            SkillListingCatalogName {
                name: "review".into(),
                user_facing_name: None,
                aliases: vec![],
            },
        ];
        assert_eq!(
            native_display_name(&skill, &exact_name_wins_over_earlier_alias).string_at(""),
            Some("plugin:review-skill")
        );

        let first_alias_match_wins = [
            SkillListingCatalogName {
                name: "other".into(),
                user_facing_name: None,
                aliases: vec!["review".into()],
            },
            matching_catalog[0].clone(),
        ];
        assert_eq!(
            native_display_name(&skill, &first_alias_match_wins).string_at(""),
            Some("plugin:review-skill")
        );

        skill.user_facing_name = Some("bad name".into());
        assert_eq!(
            native_display_name(&skill, &matching_catalog).string_at(""),
            Some("plugin:review-skill")
        );

        skill.name = "plugin:review".into();
        skill.user_facing_name = Some("review".into());
        assert_eq!(
            native_display_name(&skill, &matching_catalog).string_at(""),
            Some("plugin:review")
        );
    }

    #[test]
    fn native_skill_override_respects_alias_ranking_and_exact_tier_stops() {
        let parse = |source: &str| {
            serde_json::from_str::<lingxi_core::settings::SettingsJson>(source)
                .unwrap()
                .skill_overrides
                .unwrap()
        };
        let base = parse(r#"{"skillOverrides":{"canonical":"on"}}"#);
        let policy_alias = parse(r#"{"skillOverrides":{"short":"off"}}"#);
        let flag_alias = parse(r#"{"skillOverrides":{"short":"name-only"}}"#);
        let mut skill = entry("canonical", "desc", None, false);
        skill.unqualified_name = Some("short".into());
        assert_eq!(
            native_skill_override(&skill, Some(&base), Some(&policy_alias), Some(&flag_alias)),
            Some(SkillOverride::Off),
            "the higher-ranked policy alias wins and a weaker flag alias cannot relax it"
        );

        let exact_policy = parse(r#"{"skillOverrides":{"canonical":"off"}}"#);
        assert_eq!(
            native_skill_override(
                &skill,
                Some(&base),
                Some(&exact_policy),
                Some(&policy_alias)
            ),
            Some(SkillOverride::On),
            "an exact managed key stops tier lookup but does not replace merged base value"
        );

        skill.is_synced_skill = true;
        skill.bare_name_reserved = true;
        assert_eq!(
            native_skill_override(&skill, Some(&base), Some(&policy_alias), None),
            Some(SkillOverride::Off),
            "bare-name reservation blocks direct unqualified lookup, not alias candidates"
        );
    }

    #[test]
    fn native_builtin_gate_distinguishes_missing_verdict_from_nullish_memory() {
        let empty_remembered = [];
        assert!(native_builtin_skill_gate_active(
            &"custom-skill".into(),
            false,
            false,
            Some(&empty_remembered),
        ));
        assert!(!native_builtin_skill_gate_active(
            &"custom-skill".into(),
            false,
            true,
            Some(&empty_remembered),
        ));
        assert!(!native_builtin_skill_gate_active(
            &"debug".into(),
            false,
            false,
            None,
        ));
        assert!(native_builtin_skill_gate_active(
            &"custom-skill".into(),
            false,
            false,
            None,
        ));
        assert!(native_builtin_skill_gate_active(
            &"debug".into(),
            true,
            true,
            None,
        ));
    }

    #[test]
    fn listing_preserves_source_name_description_and_guidance() {
        let _g = BudgetEnvGuard::set(None);
        let entry = SkillListingEntry {
            name: from_units(vec![0xd800]),
            display_name: from_units(vec![0xd800]),
            user_facing_name: None,
            unqualified_name: None,
            is_synced_skill: false,
            bare_name_reserved: false,
            aliases: Vec::new(),
            source: None,
            plugin_name: None,
            description: from_units(vec![0xdc00]),
            when_to_use: Some(from_units(vec![0xd801])),
            is_bundled_prompt: false,
            is_name_only: false,
            priority: 0.0,
        };
        let actual = format(&[entry], None);
        assert_eq!(
            units(&actual),
            vec![45, 32, 0xd800, 58, 32, 0xdc00, 32, 45, 32, 0xd801]
        );
    }

    #[test]
    fn bun_string_width_uses_narrow_ambiguous_width_and_zero_width_lone_surrogates() {
        let mut original = vec![0xd800, 0x301];
        original.extend("·中😀".encode_utf16());
        let source = from_units(original);
        assert_eq!(string_width(&source), 5);
        // A real replacement character retains its own display column.
        assert_eq!(string_width(&"\u{fffd}\u{301}·中😀".into()), 6);
    }

    #[test]
    fn bun_143_display_width_matches_terminal_and_unicode_probes() {
        let probes = [
            ("a", 1),
            ("界", 2),
            ("a界", 3),
            ("e\u{0301}", 1),
            ("😀", 2),
            ("👩‍💻", 2),
            ("👩‍👩‍👧‍👦", 2),
            ("🇺🇸", 2),
            ("©", 1),
            ("Ω", 1),
            ("\u{1b}[31mred\u{1b}[0m", 3),
            ("\u{1b}]8;;https://example.com\u{7}link\u{1b}]8;;\u{7}", 4),
            ("\0x\u{7}", 1),
            ("x\u{8}Y", 2),
            ("a\tb", 2),
            ("a\nb", 2),
            ("a\r\nb", 2),
            ("abc אבג", 7),
            ("a\u{202e}bc\u{202c}", 3),
            ("♥\u{fe0e}", 1),
            ("♥\u{fe0f}", 2),
            ("⚙\u{fe0e}", 1),
            ("⚙\u{fe0f}", 2),
            ("1️⃣", 2),
            ("a‍b", 2),
            ("\u{1b}(0", 0),
            ("\u{1b}$B", 0),
            ("\u{1b}#8", 0),
            ("\u{1b}(0x", 1),
            ("\u{1b}$Bx", 1),
            ("\u{1b}#8x", 1),
        ];
        for (text, expected) in probes {
            assert_eq!(string_width(&text.into()), expected, "probe {text:?}");
        }
        assert_eq!(probes.len(), 31);
        assert_eq!(units_width(&[0xd800]), 0);
        assert_eq!(units_width(&[0xdc00]), 0);
        assert_eq!(units_width(&[b'x' as u16, 0xdc00]), 1);
        assert_eq!(units_width(&[0xd800, b'x' as u16]), 1);
    }

    #[test]
    fn in_budget_long_description_matches_native_293_raw_listing_line() {
        let _g = BudgetEnvGuard::set(None);
        let entries = [entry("native-width-probe", &"a".repeat(1200), None, false)];
        let actual = format(&entries, None);
        // This is a direct slice of the captured Native request, with no
        // JSON decoding, reserialization, whitespace changes, or trailing LF.
        let native = include_bytes!(
            "../../../../docs/parity/current-alignment/fixtures/commands/native-baseline/2.1.293/skill-description-width/control-ascii-1200/skill-listing-line.raw.txt"
        );
        assert_eq!(actual.string_at("").unwrap().as_bytes(), native);
    }

    #[test]
    fn in_budget_description_retains_all_cjk_text() {
        let _g = BudgetEnvGuard::set(None);
        let long = "中".repeat(200);
        let actual = format(&[entry("han", &long, None, false)], None);
        assert_eq!(
            units(&actual),
            format!("- han: {long}").encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn in_budget_description_retains_all_emoji_graphemes() {
        let _g = BudgetEnvGuard::set(None);
        let long = "👨‍👩‍👧‍👦".repeat(100);
        let actual = format(&[entry("emoji", &long, None, false)], None);
        assert_eq!(
            units(&actual),
            format!("- emoji: {long}")
                .encode_utf16()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tiny_budget_falls_back_to_names_only_for_non_bundled() {
        // Protected bundled descriptions stay full; other descriptions become
        // name-only when no candidate increment fits.
        let _g = BudgetEnvGuard::set(Some("15"));
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
        let body = format(&entries, Some(1_000));
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
    fn budget_greedily_keeps_fitting_descriptions_and_preserves_input_order() {
        let _g = BudgetEnvGuard::set(Some("22"));
        let mut high = entry("high", "a description too long to fit", None, false);
        high.priority = 10.0;
        let mut first_tie = entry("low", "ok", None, false);
        first_tie.priority = 1.0;
        let mut second_tie = entry("tie", "ok", None, false);
        second_tie.priority = 1.0;
        let body = format(&[high, first_tie, second_tie], Some(1_000));
        assert_eq!(
            body.string_at(""),
            Some("- high\n- low: ok\n- tie"),
            "the oversized high-priority row is skipped so the next row fits; ties stay stable and output order stays original"
        );
    }

    #[test]
    fn vfo_caps_by_utf16_units_and_name_only_uses_raw_name() {
        let _g = BudgetEnvGuard::set(None);
        let mut emoji = entry("canonical", "😀x", None, false);
        emoji.display_name = "canonical (friendly)".into();
        let settings = SkillListingBudgetSettings {
            budget_fraction: None,
            max_desc_chars: Some(2.0),
        };
        let capped = format_within_budget(&[emoji.clone()], None, 4, settings);
        let mut expected = "- canonical (friendly): "
            .encode_utf16()
            .collect::<Vec<_>>();
        expected.extend([0xd83d, 0x2026]);
        assert_eq!(units(&capped), expected);

        emoji.is_name_only = true;
        let name_only = format_within_budget(&[emoji], None, 4, settings);
        assert_eq!(name_only.string_at(""), Some("- canonical"));
    }

    #[test]
    fn native_environment_budget_accepts_js_number_and_grouped_decimal_forms() {
        assert_eq!(parse_native_budget_env("0x10"), Some(16.0));
        assert_eq!(parse_native_budget_env("-1.5e2"), Some(-150.0));
        assert_eq!(parse_native_budget_env("1_000"), Some(1_000.0));
        assert_eq!(
            parse_native_budget_env("12\u{00a0}345\u{00a0}678"),
            Some(12_345_678.0)
        );
        assert_eq!(parse_native_budget_env("1,000"), Some(1_000.0));
        assert_eq!(parse_native_budget_env("-1,234,567"), Some(-1_234_567.0));
        assert_eq!(parse_native_budget_env("1_00"), None);
        assert_eq!(parse_native_budget_env("1_000 000"), None);
        assert_eq!(parse_native_budget_env("1,000_000"), None);
        assert_eq!(parse_native_budget_env("NaN"), None);

        {
            let _g = BudgetEnvGuard::set(Some("-2.5"));
            assert_eq!(
                char_budget(Some(200_000), 4, SkillListingBudgetSettings::default()),
                -2.5
            );
        }
        {
            let _g = BudgetEnvGuard::set(Some("0"));
            assert_eq!(
                char_budget(Some(200_000), 4, SkillListingBudgetSettings::default()),
                8_000.0
            );
        }
        {
            let _g = BudgetEnvGuard::set(Some("Infinity"));
            assert_eq!(
                char_budget(Some(200_000), 4, SkillListingBudgetSettings::default()),
                f64::INFINITY
            );
        }
    }

    #[test]
    fn live_budget_settings_override_native_defaults() {
        let _g = BudgetEnvGuard::set(None);
        assert_eq!(
            char_budget(Some(200_000), 4, SkillListingBudgetSettings::default()),
            8_000.0
        );
        assert_eq!(
            char_budget(None, 4, SkillListingBudgetSettings::default()),
            8_000.0
        );
        assert_eq!(
            char_budget(
                Some(1_000),
                4,
                SkillListingBudgetSettings {
                    budget_fraction: Some(0.005),
                    max_desc_chars: None,
                }
            ),
            20.0
        );
        assert_eq!(
            char_budget(Some(100), 3, SkillListingBudgetSettings::default()),
            3.0
        );
    }
}
