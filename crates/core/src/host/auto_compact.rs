//! Source-aware Claude Code 2.1.319 auto-compaction settings.
//!
//! A higher settings tier's global window supersedes lower per-model windows.
//! A per-model `"auto"` masks the global window, so absence and auto differ.

use crate::settings::tracer::Source;
use crate::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedAutoCompactSize {
    InRange(u64),
    BelowMinimum(u64),
    AboveMaximum,
    Unusable,
}

/// Native 2.1.319 UCe: suffixes use parseFloat, bare values use Qf's
/// integer-prefix/scientific/grouped parser; shorthand applies only in commands.
#[must_use]
pub fn parse_size(raw: &str, bare_thousands: bool) -> ParsedAutoCompactSize {
    let raw = super::effort::trim_js_whitespace(raw);
    let multiplier = match raw.as_bytes().last() {
        Some(b'k' | b'K') => Some(1_000.0),
        Some(b'm' | b'M') => Some(1_000_000.0),
        _ => None,
    };
    let mut tokens = if let Some(multiplier) = multiplier {
        static FLOAT_PREFIX: std::sync::LazyLock<fancy_regex::Regex> =
            std::sync::LazyLock::new(|| {
                fancy_regex::Regex::new(
                    r"^[+-]?(?:Infinity|(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)",
                )
                .expect("JavaScript parseFloat prefix")
            });
        FLOAT_PREFIX
            .find(raw)
            .ok()
            .flatten()
            .and_then(|value| value.as_str().parse::<f64>().ok())
            .unwrap_or(f64::NAN)
            * multiplier
    } else {
        super::env::parse_int_env(raw)
    };
    if bare_thousands && multiplier.is_none() && (100.0..=1_000.0).contains(&tokens) {
        tokens *= 1_000.0;
    }
    if tokens.is_nan() || tokens < 1.0 {
        ParsedAutoCompactSize::Unusable
    } else if tokens < 100_000.0 {
        ParsedAutoCompactSize::BelowMinimum(tokens.round().min(99_999.0) as u64)
    } else if tokens > 1_000_000.0 {
        ParsedAutoCompactSize::AboveMaximum
    } else {
        ParsedAutoCompactSize::InRange(tokens.round() as u64)
    }
}

/// Wan: clamp usable environment values, keeping command-only bare shorthand off.
#[must_use]
pub fn resolve_env_window(raw: &str) -> Option<u64> {
    match parse_size(raw, false) {
        ParsedAutoCompactSize::InRange(tokens) => Some(tokens),
        ParsedAutoCompactSize::BelowMinimum(_) => Some(100_000),
        ParsedAutoCompactSize::AboveMaximum => Some(1_000_000),
        ParsedAutoCompactSize::Unusable => None,
    }
}

/// o8e/Gan diagnostic, with the same project/environment rebranding as the command.
#[must_use]
pub fn environment_diagnostic(raw: &str) -> Option<String> {
    let parsed = parse_size(raw, false);
    if matches!(parsed, ParsedAutoCompactSize::InRange(_)) {
        return None;
    }
    let length = raw.encode_utf16().count();
    let mut value = raw
        .encode_utf16()
        .take(32)
        .map(|unit| match unit {
            0x30..=0x39
            | 0x41..=0x5a
            | 0x61..=0x7a
            | 0x5f
            | 0x2e
            | 0x2c
            | 0x2b
            | 0x20
            | 0x24
            | 0x25
            | 0x7b
            | 0x7d
            | 0x3d
            | 0x3b
            | 0x2d => char::from_u32(u32::from(unit)).expect("ASCII"),
            _ => '?',
        })
        .collect::<String>();
    if length > 32 {
        value.push('…');
    }
    let environment = format!("LINGXI_AUTO_COMPACT_WINDOW=\"{value}\"");
    let product = branding::PRODUCT_NAME;
    let explanation = match parsed {
        ParsedAutoCompactSize::BelowMinimum(tokens) => {
            let digits = tokens.to_string();
            let grouped = digits
                .chars()
                .enumerate()
                .flat_map(|(index, digit)| {
                    let comma = (index > 0 && (digits.len() - index) % 3 == 0).then_some(',');
                    comma.into_iter().chain(std::iter::once(digit))
                })
                .collect::<String>();
            format!(
                "reads as {grouped} {}, so {product} treats it as the 100k minimum.",
                if tokens == 1 { "token" } else { "tokens" }
            )
        }
        ParsedAutoCompactSize::AboveMaximum => {
            format!("is too large, so {product} treats it as the 1M maximum.")
        }
        ParsedAutoCompactSize::Unusable => format!("isn't a usable size, so {product} ignores it."),
        ParsedAutoCompactSize::InRange(_) => unreachable!(),
    };
    Some(format!("{environment} {explanation} Set it to a size from 100k to 1M, such as 200k or 200000, or unset it."))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoCompactWindow {
    Auto,
    Tokens(u64),
}

impl AutoCompactWindow {
    #[must_use]
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        if value.as_str() == Some("auto") {
            return Some(Self::Auto);
        }
        let tokens = value.as_f64()?;
        (tokens.is_finite() && tokens.fract() == 0.0 && (100_000.0..=1_000_000.0).contains(&tokens))
            .then_some(Self::Tokens(tokens as u64))
    }

    #[must_use]
    pub fn tokens(self) -> Option<u64> {
        match self {
            Self::Auto => None,
            Self::Tokens(tokens) => Some(tokens),
        }
    }
}

impl Serialize for AutoCompactWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Tokens(tokens) => serializer.serialize_u64(*tokens),
        }
    }
}

impl<'de> Deserialize<'de> for AutoCompactWindow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::from_value(&value).ok_or_else(|| {
            serde::de::Error::custom("expected auto or an integer from 100000 to 1000000")
        })
    }
}

impl schemars::JsonSchema for AutoCompactWindow {
    fn schema_name() -> String {
        "AutoCompactWindow".into()
    }

    fn json_schema(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        serde_json::from_value(serde_json::json!({
            "anyOf": [
                { "type": "string", "enum": ["auto"] },
                { "type": "integer", "minimum": 100000, "maximum": 1000000 }
            ]
        }))
        .expect("static auto-compaction schema")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoCompactSettingsLayer {
    pub source: Source,
    pub enabled: Option<bool>,
    pub window: Option<AutoCompactWindow>,
    pub model_windows: Vec<(String, AutoCompactWindow)>,
}

impl AutoCompactSettingsLayer {
    #[must_use]
    pub fn from_projection(source: Source, projection: &Utf16JsonProjection) -> Self {
        let window = |value: Utf16JsonProjection| {
            serde_json::from_str::<AutoCompactWindow>(&value.to_json_string().ok()?).ok()
        };
        let model_windows = projection
            .subprojection("/modelSettings")
            .ok()
            .and_then(|models| {
                let mut values = Vec::new();
                for entry in models.object_entries()? {
                    let Ok(key) = String::from_utf16(entry.code_units) else {
                        continue;
                    };
                    let Some(value) = models
                        .subprojection_property_units(entry.code_units)
                        .ok()
                        .and_then(|row| row.subprojection("/autoCompactWindow").ok())
                        .and_then(window)
                    else {
                        continue;
                    };
                    values.push((key, value));
                }
                Some(values)
            })
            .unwrap_or_default();
        Self {
            source,
            enabled: projection
                .at_pointer("/autoCompactEnabled")
                .and_then(|v| v.as_bool()),
            window: projection
                .subprojection("/autoCompactWindow")
                .ok()
                .and_then(window),
            model_windows,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoCompactSettings {
    /// Ascending settings priority, retaining Native's per-tier window scope.
    pub layers: Vec<AutoCompactSettingsLayer>,
}

impl AutoCompactSettings {
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.layers
            .iter()
            .rev()
            .find_map(|layer| layer.enabled)
            .unwrap_or(true)
    }

    /// TZr: exact canonical keys win aliases within one tier; higher global
    /// windows override every lower tier, including lower per-model settings.
    #[must_use]
    pub fn window_for(
        &self,
        settings_key: &str,
        canonical: impl Fn(&str) -> String,
    ) -> Option<AutoCompactWindow> {
        for layer in self.layers.iter().rev() {
            let mut alias = None;
            for (key, window) in &layer.model_windows {
                if key == settings_key {
                    return Some(*window);
                }
                if alias.is_none() && canonical(key) == settings_key {
                    alias = Some(*window);
                }
            }
            if alias.is_some() {
                return alias;
            }
            if layer.window.is_some() {
                return layer.window;
            }
        }
        None
    }

    /// Mirror a successful user-settings write immediately; the next host
    /// settings reload replaces this snapshot with the authoritative tiers.
    pub fn set_user_window(&mut self, settings_key: &str, value: AutoCompactWindow) {
        let layer = if let Some(index) = self
            .layers
            .iter()
            .position(|layer| layer.source == Source::User)
        {
            &mut self.layers[index]
        } else {
            let index = self
                .layers
                .iter()
                .position(|layer| !matches!(layer.source, Source::Defaults | Source::Env))
                .unwrap_or(self.layers.len());
            self.layers.insert(
                index,
                AutoCompactSettingsLayer {
                    source: Source::User,
                    enabled: None,
                    window: None,
                    model_windows: Vec::new(),
                },
            );
            &mut self.layers[index]
        };
        if super::effort_table::object_prototype_key(settings_key) {
            layer.window = value.tokens().map(AutoCompactWindow::Tokens);
            return;
        }
        if let Some((_, current)) = layer
            .model_windows
            .iter_mut()
            .find(|(key, _)| key == settings_key)
        {
            *current = value;
        } else {
            layer.model_windows.push((settings_key.to_owned(), value));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoCompactCommandSnapshot {
    pub settings_key: String,
    pub display_name: String,
    pub enabled: bool,
    pub window: u64,
    pub configured: u64,
    pub source: String,
    pub recommend_auto: bool,
    pub override_warning: bool,
    pub user_settings_path: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(source: Source, json: &str) -> AutoCompactSettingsLayer {
        AutoCompactSettingsLayer::from_projection(
            source,
            &Utf16JsonProjection::parse(json).unwrap(),
        )
    }

    #[test]
    fn environment_and_command_share_suffixes_but_only_command_accepts_bare_thousands() {
        assert_eq!(
            parse_size("200", true),
            ParsedAutoCompactSize::InRange(200_000)
        );
        assert_eq!(
            parse_size("200", false),
            ParsedAutoCompactSize::BelowMinimum(200)
        );
        for (raw, expected) in [
            ("200k", Some(200_000)),
            (".5m", Some(500_000)),
            ("150.0005k", Some(150_001)),
            ("200", Some(100_000)),
            ("Infinitym", Some(1_000_000)),
            ("Infinity", None),
        ] {
            assert_eq!(resolve_env_window(raw), expected, "{raw}");
        }
        assert!(environment_diagnostic(".5m").is_none());
        assert_eq!(environment_diagnostic("200"), Some(format!("LINGXI_AUTO_COMPACT_WINDOW=\"200\" reads as 200 tokens, so {} treats it as the 100k minimum. Set it to a size from 100k to 1M, such as 200k or 200000, or unset it.", branding::PRODUCT_NAME)));
    }

    #[test]
    fn higher_global_masks_lower_model_and_model_auto_masks_global() {
        let settings = AutoCompactSettings {
            layers: vec![
                layer(
                    Source::User,
                    r#"{"autoCompactEnabled":false,"modelSettings":{"m":{"autoCompactWindow":500000}}}"#,
                ),
                layer(Source::Project, r#"{"autoCompactWindow":200000}"#),
            ],
        };
        assert_eq!(
            settings.window_for("m", str::to_owned),
            Some(AutoCompactWindow::Tokens(200_000))
        );
        assert!(!settings.enabled());
        let settings = AutoCompactSettings {
            layers: vec![layer(
                Source::User,
                r#"{"autoCompactWindow":200000,"modelSettings":{"m":{"autoCompactWindow":"auto"}}}"#,
            )],
        };
        assert_eq!(
            settings.window_for("m", str::to_owned),
            Some(AutoCompactWindow::Auto)
        );
    }

    #[test]
    fn canonical_key_wins_alias_and_profiles_remain_distinct() {
        let mut settings = AutoCompactSettings {
            layers: vec![layer(
                Source::User,
                r#"{"modelSettings":{"a/m":{"autoCompactWindow":500000},"alias":{"autoCompactWindow":200000}}}"#,
            )],
        };
        assert_eq!(
            settings.window_for("a/m", |_| "a/m".into()),
            Some(AutoCompactWindow::Tokens(500_000))
        );
        assert_eq!(settings.window_for("b/m", str::to_owned), None);
        settings.set_user_window("b/m", AutoCompactWindow::Auto);
        assert_eq!(
            settings.window_for("a/m", str::to_owned),
            Some(AutoCompactWindow::Tokens(500_000))
        );
        assert_eq!(
            settings.window_for("b/m", str::to_owned),
            Some(AutoCompactWindow::Auto)
        );
    }
}
