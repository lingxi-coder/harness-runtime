//! Pure Native prompt-suggestion controls and presentation transitions.
//!
//! The native-source oracle executes the shipped functions; persistence, clocks,
//! feature flags and protocol delivery are deliberately supplied by the caller.
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub(crate) const UNUSED_THRESHOLD: f64 = 20.0;
pub(crate) const NOTICE_CLAIMED: f64 = 21.0;
pub(crate) const BACKOFF_INTERVAL: u64 = 10;
pub(crate) const NEW_USER_WINDOW_MS: f64 = 1_209_600_000.0;
pub(crate) const BACKOFF_NOTICE: &str = "Showing fewer prompt suggestions";
pub(crate) const BACKOFF_NOTICE_DIM: &str = " · use one to bring them back";

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// `M.triBool()` uses the shared `Ne`/`Es` parsers: trim and case-insensitive.
pub(crate) fn parse_env(raw: Option<&str>) -> Option<bool> {
    match raw?
        .trim_matches(js_whitespace)
        .to_ascii_lowercase()
        .as_str()
    {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct Enablement {
    pub enabled: bool,
    pub source: &'static str,
}

/// Native `Gun`: the explicit environment setting precedes every other gate.
pub(crate) fn initial_enablement(
    env: Option<bool>,
    rollout_enabled: bool,
    non_interactive: bool,
    swarm_teammate: bool,
    setting: Option<bool>,
) -> Enablement {
    let (enabled, source) = if let Some(enabled) = env {
        (enabled, "env")
    } else if !rollout_enabled {
        (false, "growthbook")
    } else if non_interactive {
        (false, "non_interactive")
    } else if swarm_teammate {
        (false, "swarm_teammate")
    } else {
        (setting != Some(false), "setting")
    };
    Enablement { enabled, source }
}

/// Native `zun`: explicit SDK/CLI opt-in bypasses rollout and interactive gates.
pub(crate) fn explicit_enablement(env: Option<bool>, setting: Option<bool>) -> bool {
    env.unwrap_or(setting != Some(false))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CacheUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Suppression {
    reason: &'static str,
    cache_cold_by: Option<&'static str>,
}

impl Suppression {
    fn reason_only(reason: &'static str) -> Self {
        Self {
            reason,
            cache_cold_by: None,
        }
    }

    pub(crate) fn reason(self) -> &'static str {
        self.reason
    }
    pub(crate) fn cache_cold_by(self) -> Option<&'static str> {
        self.cache_cold_by
    }
}

pub(crate) fn cache_cold_by(usage: Option<CacheUsage>, thaw_enabled: bool) -> Option<&'static str> {
    let usage = usage?;
    let uncached = usage.input_tokens.saturating_add(usage.output_tokens);
    if uncached > 10_000 {
        Some("uncached")
    } else if uncached.saturating_add(usage.cache_creation_input_tokens) > 10_000 && !thaw_enabled {
        Some("cache_write")
    } else {
        None
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GenerationGates<'a> {
    pub aborted: bool,
    pub assistant_count: usize,
    pub last_response_api_error: bool,
    pub enabled: bool,
    pub pending_permission: bool,
    pub elicitation_active: bool,
    pub plan_mode: bool,
    pub rate_limit_status: &'a str,
    pub env: Option<bool>,
    pub usage: Option<CacheUsage>,
    pub cache_thaw: bool,
}

impl Default for GenerationGates<'_> {
    fn default() -> Self {
        Self {
            aborted: false,
            assistant_count: 2,
            last_response_api_error: false,
            enabled: true,
            pending_permission: false,
            elicitation_active: false,
            plan_mode: false,
            rate_limit_status: "allowed",
            env: None,
            usage: None,
            cache_thaw: false,
        }
    }
}

impl GenerationGates<'_> {
    /// Native `oNr`/`x` ordering. Backoff runs only after this returns `None`.
    pub(crate) fn suppression(&self) -> Option<Suppression> {
        let reason = if self.aborted {
            Some("aborted")
        } else if self.assistant_count < 2 {
            Some("early_conversation")
        } else if self.last_response_api_error {
            Some("last_response_error")
        } else if !self.enabled {
            Some("disabled")
        } else if self.pending_permission {
            Some("pending_permission")
        } else if self.elicitation_active {
            Some("elicitation_active")
        } else if self.plan_mode {
            Some("plan_mode")
        } else if self.rate_limit_status != "allowed"
            && !(self.rate_limit_status == "allowed_warning" && self.env == Some(true))
        {
            Some("rate_limit")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Some(Suppression::reason_only(reason));
        }
        cache_cold_by(self.usage, self.cache_thaw).map(|by| Suppression {
            reason: "cache_cold",
            cache_cold_by: Some(by),
        })
    }
}

/// `Y$o`: unknown focus is permitted locally; background sessions require focus
/// unless they are blocked with no blocking operation. This gate precedes I/O.
pub(crate) fn cli_focus_suppression(
    focus: &str,
    background_mode: bool,
    background_blocked_without_block: bool,
) -> Option<&'static str> {
    if background_mode {
        (focus != "focused" && !background_blocked_without_block).then_some("bg_unattached")
    } else {
        (focus == "blurred").then_some("unfocused")
    }
}

pub(crate) fn persisted_streak(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BackoffInputs {
    pub persisted_streak: f64,
    pub now_ms: f64,
    /// Missing `firstStartTime` maps to `None` (the native epoch fallback).
    /// An invalid date maps to `Some(NaN)`, which native `Date.parse` preserves.
    pub first_start_ms: Option<f64>,
    pub env: Option<bool>,
}

impl BackoffInputs {
    pub(crate) fn backing_off(self) -> bool {
        persisted_streak(self.persisted_streak) >= UNUSED_THRESHOLD
            && self.env != Some(true)
            && (self.now_ms - self.first_start_ms.unwrap_or(0.0)).partial_cmp(&NEW_USER_WINDOW_MS)
                != Some(std::cmp::Ordering::Less)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnusedState {
    pub eligible_turns: u64,
    pub unused_in_a_row: u64,
}

impl UnusedState {
    pub(crate) fn eligible(&mut self, backing_off: bool) -> bool {
        if !backing_off {
            return true;
        }
        let eligible = self.eligible_turns.is_multiple_of(BACKOFF_INTERVAL);
        self.eligible_turns = self.eligible_turns.saturating_add(1);
        eligible
    }

    /// Native `K2t`. Only submissions after a shown suggestion call this.
    /// Returns a global-config update, if one is required.
    pub(crate) fn record_submission(&mut self, accepted: bool, streak: f64) -> Option<f64> {
        let streak = persisted_streak(streak);
        if accepted {
            self.unused_in_a_row = 0;
            return (streak != 0.0).then_some(0.0);
        }
        if streak >= UNUSED_THRESHOLD {
            return None;
        }
        self.unused_in_a_row = self.unused_in_a_row.saturating_add(1);
        if self.unused_in_a_row >= UNUSED_THRESHOLD as u64 {
            self.unused_in_a_row = 0;
            Some(UNUSED_THRESHOLD)
        } else {
            None
        }
    }

    /// Native `yKo`, used only when the config toggle changes false -> true.
    pub(crate) fn reenable(&mut self, streak: f64) -> Option<f64> {
        self.unused_in_a_row = 0;
        (persisted_streak(streak) != 0.0).then_some(0.0)
    }

    pub(crate) fn ask_on_next_turn(&mut self) {
        self.eligible_turns = 0;
    }
}

/// Native `XGr`: claiming the notice persists 21, distinct from threshold 20.
pub(crate) fn claim_notice(streak: f64, backing_off: bool) -> Option<f64> {
    let streak = persisted_streak(streak);
    (backing_off && streak >= UNUSED_THRESHOLD && streak != NOTICE_CLAIMED)
        .then_some(NOTICE_CLAIMED)
}

/// Native composer state. `generated` is not counted until actually displayed.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum Presentation {
    #[default]
    Empty,
    Generated {
        #[serde(serialize_with = "serialize_root_string_projection")]
        text: Utf16JsonProjection,
        #[serde(rename = "generationRequestId")]
        generation_request_id: Option<String>,
    },
    Shown {
        #[serde(serialize_with = "serialize_root_string_projection")]
        text: Utf16JsonProjection,
        #[serde(rename = "generationRequestId")]
        generation_request_id: Option<String>,
        #[serde(rename = "shownAt")]
        shown_at: f64,
    },
    Accepted {
        #[serde(serialize_with = "serialize_root_string_projection")]
        text: Utf16JsonProjection,
        #[serde(rename = "generationRequestId")]
        generation_request_id: Option<String>,
        #[serde(rename = "shownAt")]
        shown_at: f64,
        #[serde(rename = "acceptedAt")]
        accepted_at: f64,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPresentationFields {
    status: String,
    #[serde(default)]
    text: Option<Box<RawValue>>,
    #[serde(default)]
    generation_request_id: Option<Box<RawValue>>,
    #[serde(default)]
    shown_at: Option<Box<RawValue>>,
    #[serde(default)]
    accepted_at: Option<Box<RawValue>>,
}

impl<'de> Deserialize<'de> for Presentation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let fields: RawPresentationFields =
            serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;

        match fields.status.as_str() {
            "empty" => Ok(Self::Empty),
            "generated" => Ok(Self::Generated {
                text: parse_presentation_text::<D::Error>(fields.text.as_deref())?,
                generation_request_id: parse_optional_raw_field::<String, D::Error>(
                    fields.generation_request_id.as_deref(),
                )?,
            }),
            "shown" => Ok(Self::Shown {
                text: parse_presentation_text::<D::Error>(fields.text.as_deref())?,
                generation_request_id: parse_optional_raw_field::<String, D::Error>(
                    fields.generation_request_id.as_deref(),
                )?,
                shown_at: parse_required_raw_field::<f64, D::Error>(
                    fields.shown_at.as_deref(),
                    "shownAt",
                )?,
            }),
            "accepted" => Ok(Self::Accepted {
                text: parse_presentation_text::<D::Error>(fields.text.as_deref())?,
                generation_request_id: parse_optional_raw_field::<String, D::Error>(
                    fields.generation_request_id.as_deref(),
                )?,
                shown_at: parse_required_raw_field::<f64, D::Error>(
                    fields.shown_at.as_deref(),
                    "shownAt",
                )?,
                accepted_at: parse_required_raw_field::<f64, D::Error>(
                    fields.accepted_at.as_deref(),
                    "acceptedAt",
                )?,
            }),
            other => Err(<D::Error as serde::de::Error>::unknown_variant(
                other,
                &["empty", "generated", "shown", "accepted"],
            )),
        }
    }
}

fn parse_optional_raw_field<T, E>(raw: Option<&RawValue>) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    raw.map(|raw| serde_json::from_str(raw.get()).map_err(E::custom))
        .transpose()
}

fn parse_required_raw_field<T, E>(raw: Option<&RawValue>, field: &'static str) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    let raw = raw.ok_or_else(|| E::missing_field(field))?;
    serde_json::from_str(raw.get()).map_err(E::custom)
}

fn parse_presentation_text<E>(raw: Option<&RawValue>) -> Result<Utf16JsonProjection, E>
where
    E: serde::de::Error,
{
    let raw = raw.ok_or_else(|| E::missing_field("text"))?;
    let projection = Utf16JsonProjection::parse(raw.get()).map_err(E::custom)?;
    if projection.string_units("").is_none() {
        return Err(E::custom(
            "prompt suggestion presentation must be a root JSON string",
        ));
    }
    Ok(projection)
}

fn serialize_root_string_projection<S>(
    text: &Utf16JsonProjection,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if text.string_units("").is_none() {
        return Err(serde::ser::Error::custom(
            "prompt suggestion presentation must be a root JSON string",
        ));
    }
    lingxi_core::types::utf16_json::raw_json_serde::serialize(text, serializer)
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubmissionOutcome {
    pub accepted: bool,
    pub accept_method: Option<&'static str>,
    pub elapsed_ms: f64,
    pub similarity: f64,
    pub generation_request_id: Option<String>,
}

impl Presentation {
    pub(crate) fn is_shown(&self) -> bool {
        matches!(self, Self::Shown { .. } | Self::Accepted { .. })
    }

    pub(crate) fn mark_shown(&mut self, now_ms: f64) {
        if let Self::Generated {
            text,
            generation_request_id,
        } = self
        {
            *self = Self::Shown {
                text: std::mem::replace(text, Utf16JsonProjection::from("")),
                generation_request_id: generation_request_id.take(),
                shown_at: now_ms,
            };
        }
    }

    pub(crate) fn mark_accepted(&mut self, now_ms: f64) {
        match self {
            Self::Shown {
                text,
                generation_request_id,
                shown_at,
            }
            | Self::Accepted {
                text,
                generation_request_id,
                shown_at,
                ..
            } => {
                *self = Self::Accepted {
                    text: std::mem::replace(text, Utf16JsonProjection::from("")),
                    generation_request_id: generation_request_id.take(),
                    shown_at: *shown_at,
                    accepted_at: now_ms,
                };
            }
            _ => {}
        }
    }

    pub(crate) fn submission(
        &mut self,
        input: &str,
        now_ms: f64,
        skip_reset: bool,
    ) -> Option<SubmissionOutcome> {
        let (text, generation_request_id, shown_at, accepted_at) = match self {
            Self::Shown {
                text,
                generation_request_id,
                shown_at,
            } => (text, generation_request_id, *shown_at, 0.0),
            Self::Accepted {
                text,
                generation_request_id,
                shown_at,
                accepted_at,
            } => (text, generation_request_id, *shown_at, *accepted_at),
            _ => return None,
        };
        let via_tab = accepted_at > shown_at;
        let text_units = text
            .string_units("")
            .expect("prompt suggestion presentation is a root string");
        let input_units = input.encode_utf16().collect::<Vec<_>>();
        let accepted = via_tab || lowercase_utf16(&input_units) == lowercase_utf16(&text_units);
        let at = if accepted && accepted_at != 0.0 {
            accepted_at
        } else {
            now_ms
        };
        let denominator = text_units.len().max(1) as f64;
        let outcome = SubmissionOutcome {
            accepted,
            accept_method: accepted.then_some(if via_tab { "tab" } else { "enter" }),
            elapsed_ms: at - shown_at,
            similarity: ((input.encode_utf16().count() as f64 / denominator) * 100.0).round()
                / 100.0,
            generation_request_id: generation_request_id.clone(),
        };
        if !skip_reset {
            *self = Self::Empty;
        }
        Some(outcome)
    }
}

fn lowercase_utf16(units: &[u16]) -> Vec<u16> {
    let mut output = Vec::with_capacity(units.len());
    let mut run = String::new();
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok(character) => run.push(character),
            Err(error) => {
                output.extend(run.to_lowercase().encode_utf16());
                run.clear();
                output.push(error.unpaired_surrogate());
            }
        }
    }
    output.extend(run.to_lowercase().encode_utf16());
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_json_round_trip_preserves_unpaired_surrogate_units() {
        let source = r#"{"status":"shown","text":"left\ud800right","generationRequestId":null,"shownAt":1.0}"#;
        let decoded: Presentation = serde_json::from_str(source).unwrap();
        let text_units = match &decoded {
            Presentation::Shown { text, .. } => text.string_units("").unwrap(),
            _ => panic!("expected shown presentation"),
        };
        assert_eq!(
            text_units,
            [
                b'l' as u16,
                b'e' as u16,
                b'f' as u16,
                b't' as u16,
                0xd800,
                b'r' as u16,
                b'i' as u16,
                b'g' as u16,
                b'h' as u16,
                b't' as u16,
            ]
        );

        let encoded = serde_json::to_string(&decoded).unwrap();
        let wire = Utf16JsonProjection::parse(&encoded).unwrap();
        assert_eq!(wire.string_units("/text").unwrap(), text_units);
        let replayed: Presentation = serde_json::from_str(&encoded).unwrap();
        assert_eq!(replayed, decoded);
    }

    #[test]
    fn submission_does_not_conflate_unpaired_source_with_replacement_character() {
        let suggestion = Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        let mut presentation = Presentation::Shown {
            text: suggestion,
            generation_request_id: None,
            shown_at: 1.0,
        };

        let outcome = presentation.submission("�", 2.0, false).unwrap();

        assert!(!outcome.accepted);
        assert_eq!(outcome.accept_method, None);
    }
}
