//! Claude Code 2.1.296 `Pw` / `Glt` small-fast selection.
//! Pure policy over authoritative route, catalog and managed-policy facts.
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Redacted facts captured from the selected provider/credential route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifierModelRouteFacts {
    /// Native provider identity; absent metadata never guesses from a model ID.
    pub provider: Option<String>,
    /// Native endpoint identity, including an explicit host assumption.
    pub native_base_url: bool,
    /// Native API-key shape from the selected credential (never a bearer token).
    pub native_api_key: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClassifierModelSelection {
    pub route: ClassifierModelRouteFacts,
    pub main_model: String,
    pub small_fast_override: Option<String>,
    pub default_haiku_override: Option<String>,
    pub haiku_model: Option<String>,
    pub haiku_available: bool,
    pub sonnet_model: Option<String>,
    pub sonnet_available: bool,
    pub explicit_model: bool,
    pub default_model_from_env: bool,
    pub opus_override: bool,
    pub sonnet_override: bool,
}

/// Restriction check applied after the chosen model is resolved in its profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierModelRestriction {
    None,
    DeniedOnly,
    DeniedOrExactAllowlist,
}

pub fn select_classifier_model(
    input: &ClassifierModelSelection,
) -> (String, ClassifierModelRestriction) {
    use ClassifierModelRestriction::*;
    let provider = input.route.provider.as_deref().unwrap_or("");
    // A non-Anthropic product route keeps its configured provider/model.
    if !matches!(
        provider,
        "firstParty"
            | "anthropicAws"
            | "anthropicGoogleCloud"
            | "bedrock"
            | "vertex"
            | "foundry"
            | "gateway"
            | "mantle"
    ) {
        return (input.main_model.clone(), None);
    }
    let explicit = |model: &str| {
        (
            model.to_owned(),
            if is_canonical_haiku(model) {
                DeniedOnly
            } else {
                DeniedOrExactAllowlist
            },
        )
    };
    if let Some(model) = &input.small_fast_override {
        return explicit(model);
    }
    let supports_small_fast = input.default_haiku_override.is_some()
        || (provider == "firstParty"
            && (input.route.native_base_url || input.route.native_api_key))
        || matches!(provider, "anthropicAws" | "anthropicGoogleCloud")
        || (provider == "gateway" && input.haiku_available);
    if !supports_small_fast {
        if matches!(provider, "bedrock" | "vertex")
            && !input.explicit_model
            && !input.default_model_from_env
            && (!input.opus_override || input.sonnet_override)
            && input.sonnet_available
        {
            if let Some(model) = &input.sonnet_model {
                return (model.clone(), None);
            }
        }
        return (input.main_model.clone(), None);
    }
    if let Some(model) = &input.default_haiku_override {
        return explicit(model);
    }
    input
        .haiku_model
        .as_ref()
        .map(|model| (model.clone(), DeniedOnly))
        .unwrap_or_else(|| (input.main_model.clone(), None))
}

#[derive(Debug, Clone)]
struct ModelId {
    family: String,
    major: u32,
    minor: Option<u32>,
    trailer: Option<String>,
    base: String,
    legacy: bool,
}
fn trim(value: &str) -> &str {
    lingxi_core::host::effort::trim_js_whitespace(value)
}
fn context_free(value: &str) -> String {
    static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\[[12]m\]").unwrap());
    TAG.replace_all(value, "").into_owned()
}
fn model_id(value: &str) -> Option<ModelId> {
    let mut value = trim(value).to_lowercase();
    if value.is_empty() || value.chars().any(|c| trim(&c.to_string()).is_empty()) {
        return None;
    }
    if value.ends_with("[1m]") || value.ends_with("[2m]") {
        value.truncate(value.len() - 4);
    }
    if let Some(last) = value.rsplit('/').next() {
        value = last.into();
    }
    if let Some((region, rest)) = value.split_once(".anthropic.") {
        if !["us", "eu", "apac", "jp", "au", "us-gov", "global"].contains(&region) {
            return None;
        }
        value = rest.into();
    } else if let Some(rest) = value.strip_prefix("anthropic.") {
        value = rest.into();
    }
    static CURRENT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^claude-([a-z]+)-([0-9]{1,2})(?:-([0-9]{1,2}))?").unwrap());
    static LEGACY: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^claude-([0-9]{1,2})(?:-([0-9]{1,2}))?-([a-z]+)").unwrap());
    let (family, major, minor, end, legacy) = if let Some(c) = CURRENT.captures(&value) {
        let major = c.get(2)?;
        if value
            .as_bytes()
            .get(major.end())
            .is_some_and(u8::is_ascii_digit)
        {
            return None;
        }
        let minor = c.get(3).filter(|m| {
            !value
                .as_bytes()
                .get(m.end())
                .is_some_and(u8::is_ascii_digit)
        });
        (
            c[1].into(),
            major.as_str().parse().ok()?,
            minor.and_then(|m| m.as_str().parse().ok()),
            minor.map_or(major.end(), |m| m.end()),
            false,
        )
    } else if let Some(c) = LEGACY.captures(&value) {
        (
            c[3].into(),
            c[1].parse().ok()?,
            c.get(2).and_then(|m| m.as_str().parse().ok()),
            c.get(0)?.end(),
            true,
        )
    } else {
        return None;
    };
    let suffix = &value[end..];
    if !suffix.is_empty() && !suffix.starts_with(['-', '@']) {
        return None;
    }
    static TRAILER: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:-fast|-latest)?(?:-v[0-9]{1,3}@[0-9]{8}|[-@][0-9]{8})?(?:-v[0-9]{1,3}(?::[0-9]{1,3})?)?$").unwrap()
    });
    Some(ModelId {
        family,
        major,
        minor,
        trailer: (!TRAILER.is_match(suffix)).then(|| suffix.into()),
        base: value[..end].into(),
        legacy,
    })
}

/// Native `Rp` + `RPs` recognition; arbitrary paths are not trusted model IDs.
pub fn is_canonical_haiku(value: &str) -> bool {
    let Some(model) = model_id(value) else {
        return false;
    };
    if model.family != "haiku" || model.trailer.is_some() {
        return false;
    }
    let lowered = value.to_lowercase();
    let Some(base) = lowered.rfind(&model.base) else {
        return false;
    };
    let prefix = lowered[..base]
        .rfind('/')
        .map_or("", |slash| &lowered[..=slash]);
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:arn:aws(?:-[a-z]+)*:bedrock:[a-z0-9-]*:[0-9]*:(?:foundation-model|inference-profile)/|(?:projects/[^/]+/locations/[^/]+/)?publishers/anthropic/models/)$").unwrap()
    });
    prefix.is_empty() || PREFIX.is_match(prefix)
}

fn family(value: &str) -> bool {
    matches!(value, "sonnet" | "opus" | "haiku" | "fable")
}
fn dynamic_alias(value: &str) -> bool {
    matches!(value, "best" | "opusplan" | "default")
}
fn matches_model(rule: &ModelId, model: &ModelId) -> bool {
    rule.family == model.family
        && rule.major == model.major
        && rule
            .minor
            .is_none_or(|minor| minor == model.minor.unwrap_or(0))
        && rule.trailer.as_ref().is_none_or(|trailer| {
            model.trailer.as_deref().unwrap_or("") == trailer
                || model
                    .trailer
                    .as_deref()
                    .unwrap_or("")
                    .starts_with(&format!("{trailer}-"))
        })
}

/// Managed `deniedModels`, including family/version/snapshot/provider spellings
/// and reverse modelOverrides. Dynamic aliases/empty entries are ignored.
pub fn classifier_model_denied(
    model: &str,
    entries: &[String],
    overrides: &BTreeMap<String, String>,
) -> bool {
    let normalized = context_free(trim(model)).to_lowercase();
    let mut candidates = vec![normalized.clone()];
    for (canonical, wire) in overrides {
        if context_free(trim(wire)).eq_ignore_ascii_case(&normalized) {
            candidates.push(context_free(trim(canonical)).to_lowercase());
        }
    }
    entries.iter().any(|entry| {
        let entry = context_free(trim(entry)).to_lowercase();
        let entry = trim(&entry);
        if entry.is_empty() || dynamic_alias(entry) {
            return false;
        }
        if family(entry) {
            return candidates.iter().any(|candidate| {
                model_id(candidate).is_some_and(|id| id.family == entry)
                    || candidate
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .any(|part| part == entry)
            });
        }
        let parsed = model_id(entry).or_else(|| {
            (!entry.starts_with("claude-"))
                .then(|| model_id(&format!("claude-{entry}")))
                .flatten()
        });
        candidates.iter().any(|candidate| match &parsed {
            Some(rule) => model_id(candidate).is_some_and(|id| matches_model(rule, &id)),
            None => candidate == entry,
        })
    })
}

/// Native exact managed allowlist branch used only by explicit noncanonical
/// helper overrides. Unlike deniedModels, a canonical date/version is exact.
pub fn classifier_model_exact_allowed(
    model: &str,
    entries: &[String],
    overrides: &BTreeMap<String, String>,
) -> bool {
    let mut candidates = vec![context_free(trim(model)).to_lowercase()];
    for (canonical, wire) in overrides {
        if context_free(trim(wire)).eq_ignore_ascii_case(&candidates[0]) {
            candidates.push(context_free(trim(canonical)).to_lowercase());
        }
    }
    let effective: Vec<_> = entries
        .iter()
        .map(|entry| context_free(trim(entry)).to_lowercase())
        .filter(|entry| !entry.is_empty() && !dynamic_alias(entry))
        .collect();
    if effective.is_empty() {
        return true;
    }
    effective.iter().any(|entry| {
        let bare_family = entry.strip_prefix("claude-").unwrap_or(entry);
        if family(bare_family) {
            return candidates.iter().any(|candidate| {
                model_id(candidate).is_some_and(|id| id.family == bare_family)
                    || candidate
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .any(|part| part == bare_family)
            });
        }
        let rule = model_id(entry).or_else(|| {
            (!entry.starts_with("claude-"))
                .then(|| model_id(&format!("claude-{entry}")))
                .flatten()
        });
        candidates.iter().any(|candidate| match &rule {
            Some(rule) => model_id(candidate).is_some_and(|id| {
                rule.family == id.family
                    && rule.major == id.major
                    && rule.minor.unwrap_or(0) == id.minor.unwrap_or(0)
                    && rule.legacy == id.legacy
                    && rule.trailer == id.trailer
                    && (!candidate.contains("-latest") || entry.contains("-latest"))
            }),
            None => candidate == entry,
        })
    })
}
