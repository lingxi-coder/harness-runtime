//! Parse `settings.routing` into a partial `ChainConfig` + a raw fallback map
//! (spec §5.2). Fallback target strings stay raw here; `assemble` validates them
//! into `ChainEntry`s.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::types::{ChainConfig, RetryOverride};

/// Default total attempts when `routing.retry.maxAttempts` is absent.
const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default base backoff when `routing.retry.backoffMs` is absent.
const DEFAULT_BACKOFF_MS: u64 = 250;

/// Parse `settings.routing`. Returns the partial `ChainConfig` (aliases + retry;
/// `chains` left empty for `assemble`), the raw `key -> ["provider/model", …]`
/// fallback map, and collected warnings.
#[must_use]
pub fn parse_routing(
    routing: Option<&Value>,
) -> (ChainConfig, BTreeMap<String, Vec<String>>, Vec<String>) {
    let mut cfg = ChainConfig::default();
    let mut raw_fallback: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut warnings = Vec::new();

    let Some(obj) = routing.and_then(Value::as_object) else {
        return (cfg, raw_fallback, warnings);
    };

    if let Some(aliases) = obj.get("aliases").and_then(Value::as_object) {
        for (k, v) in aliases {
            if let Some(s) = v.as_str() {
                cfg.aliases.insert(k.clone(), s.to_string());
            } else {
                warnings.push(format!(
                    "routing.aliases[{k:?}]: value is not a string; skipped"
                ));
            }
        }
    }

    if let Some(fb) = obj.get("fallback").and_then(Value::as_object) {
        for (k, v) in fb {
            let Some(arr) = v.as_array() else {
                warnings.push(format!(
                    "routing.fallback[{k:?}]: value is not an array; skipped"
                ));
                continue;
            };
            let targets: Vec<String> = arr
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            if targets.is_empty() {
                warnings.push(format!(
                    "routing.fallback[{k:?}]: no string targets; skipped"
                ));
                continue;
            }
            raw_fallback.insert(k.clone(), targets);
        }
    }

    if let Some(retry) = obj.get("retry").and_then(Value::as_object) {
        let max_attempts = retry
            .get("maxAttempts")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_MAX_ATTEMPTS)
            .max(1);
        let backoff_ms = retry
            .get("backoffMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BACKOFF_MS);
        cfg.retry = Some(RetryOverride {
            max_attempts,
            backoff_ms,
        });
    }

    (cfg, raw_fallback, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn none_routing_yields_empty_config() {
        let (cfg, fb, warns) = parse_routing(None);
        assert!(cfg.aliases.is_empty());
        assert!(cfg.chains.is_empty());
        assert!(cfg.retry.is_none());
        assert!(fb.is_empty());
        assert!(warns.is_empty());
    }

    #[test]
    fn parses_aliases() {
        let v = json!({ "aliases": { "fast": "deepseek/deepseek-chat", "smart": "openrouter/openai/gpt-4o" } });
        let (cfg, _fb, warns) = parse_routing(Some(&v));
        assert!(warns.is_empty());
        assert_eq!(
            cfg.aliases.get("fast").map(String::as_str),
            Some("deepseek/deepseek-chat")
        );
        assert_eq!(
            cfg.aliases.get("smart").map(String::as_str),
            Some("openrouter/openai/gpt-4o")
        );
    }

    #[test]
    fn parses_fallback_into_raw_map() {
        let v = json!({ "fallback": { "fast": ["deepseek/deepseek-chat", "openrouter/openai/gpt-4o-mini"] } });
        let (cfg, fb, warns) = parse_routing(Some(&v));
        assert!(warns.is_empty());
        assert!(cfg.chains.is_empty());
        assert_eq!(fb.get("fast").unwrap().len(), 2);
        assert_eq!(fb.get("fast").unwrap()[0], "deepseek/deepseek-chat");
    }

    #[test]
    fn retry_defaults_and_overrides() {
        let (cfg, _fb, _w) = parse_routing(Some(&json!({ "retry": {} })));
        let r = cfg.retry.unwrap();
        assert_eq!(r.max_attempts, 3);
        assert_eq!(r.backoff_ms, 250);

        let (cfg2, _fb2, _w2) = parse_routing(Some(
            &json!({ "retry": { "maxAttempts": 5, "backoffMs": 100 } }),
        ));
        let r2 = cfg2.retry.unwrap();
        assert_eq!(r2.max_attempts, 5);
        assert_eq!(r2.backoff_ms, 100);
    }

    #[test]
    fn retry_zero_attempts_clamped_to_one() {
        let (cfg, _fb, _w) = parse_routing(Some(&json!({ "retry": { "maxAttempts": 0 } })));
        assert_eq!(cfg.retry.unwrap().max_attempts, 1);
    }

    #[test]
    fn malformed_alias_value_warns_and_skips() {
        let (cfg, _fb, warns) = parse_routing(Some(&json!({ "aliases": { "fast": 42 } })));
        assert!(cfg.aliases.is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("fast"));
    }

    #[test]
    fn malformed_fallback_value_warns_and_skips() {
        let (_cfg, fb, warns) =
            parse_routing(Some(&json!({ "fallback": { "fast": "not-an-array" } })));
        assert!(fb.is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("not an array"));
    }

    #[test]
    fn empty_fallback_targets_warns_and_skips() {
        let (_cfg, fb, warns) = parse_routing(Some(&json!({ "fallback": { "fast": [1, 2, 3] } })));
        assert!(fb.is_empty());
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("no string targets"));
    }
}
