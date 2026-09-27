use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSearchProvider {
    Auto,
    DuckDuckGo,
    Tavily,
    Brave,
    Searxng,
}

impl WebSearchProvider {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "auto" => Some(Self::Auto),
            "duckduckgo" => Some(Self::DuckDuckGo),
            "tavily" => Some(Self::Tavily),
            "brave" => Some(Self::Brave),
            "searxng" => Some(Self::Searxng),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::DuckDuckGo => "duckduckgo",
            Self::Tavily => "tavily",
            Self::Brave => "brave",
            Self::Searxng => "searxng",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchConfig {
    pub provider: WebSearchProvider,
    pub searxng_url: Option<String>,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            provider: WebSearchProvider::Auto,
            searxng_url: None,
        }
    }
}

impl WebSearchConfig {
    #[must_use]
    pub fn from_settings_json(v: &Value) -> Self {
        let Some(web_search) = v.get("webSearch").and_then(Value::as_object) else {
            return Self::default();
        };

        let provider = web_search
            .get("provider")
            .and_then(Value::as_str)
            .and_then(WebSearchProvider::parse)
            .unwrap_or(WebSearchProvider::Auto);

        let searxng_url = web_search
            .get("searxngUrl")
            .and_then(Value::as_str)
            .and_then(normalize_searxng_url);

        Self {
            provider,
            searxng_url,
        }
    }

    pub fn write_settings_json(&self, v: &mut Value) {
        let root = ensure_object(v);
        let web_search = root
            .entry("webSearch".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let web_search_obj = ensure_object(web_search);

        web_search_obj.insert(
            "provider".to_string(),
            Value::String(self.provider.as_str().to_string()),
        );

        match &self.searxng_url {
            Some(url) => {
                if let Some(normalized) = normalize_searxng_url(url) {
                    web_search_obj.insert("searxngUrl".to_string(), Value::String(normalized));
                } else {
                    web_search_obj.remove("searxngUrl");
                }
            }
            None => {
                web_search_obj.remove("searxngUrl");
            }
        }
    }
}

fn ensure_object(v: &mut Value) -> &mut Map<String, Value> {
    if !v.is_object() {
        *v = Value::Object(Map::new());
    }
    v.as_object_mut().expect("value set to object")
}

fn normalize_searxng_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/').trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_round_trips_settings_strings() {
        assert_eq!(
            WebSearchProvider::parse("auto"),
            Some(WebSearchProvider::Auto)
        );
        assert_eq!(
            WebSearchProvider::parse("duckduckgo"),
            Some(WebSearchProvider::DuckDuckGo)
        );
        assert_eq!(WebSearchProvider::Tavily.as_str(), "tavily");
    }

    #[test]
    fn reads_nested_settings_shape() {
        let v =
            json!({ "webSearch": { "provider": "brave", "searxngUrl": "https://search.local" } });
        let cfg = WebSearchConfig::from_settings_json(&v);
        assert_eq!(cfg.provider, WebSearchProvider::Brave);
        assert_eq!(cfg.searxng_url.as_deref(), Some("https://search.local"));
    }

    #[test]
    fn invalid_provider_falls_back_to_default_auto() {
        let v = json!({ "webSearch": { "provider": "not-a-real-provider" } });
        let cfg = WebSearchConfig::from_settings_json(&v);
        assert_eq!(cfg, WebSearchConfig::default());
    }

    #[test]
    fn searxng_url_none_removes_field_on_write() {
        let mut v = json!({ "theme": "dark", "webSearch": { "provider": "searxng", "searxngUrl": "https://old.example/" } });
        WebSearchConfig {
            provider: WebSearchProvider::Auto,
            searxng_url: None,
        }
        .write_settings_json(&mut v);
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["webSearch"]["provider"], "auto");
        assert!(v["webSearch"].get("searxngUrl").is_none());
    }

    #[test]
    fn searxng_url_round_trips_trimmed_and_without_trailing_slash() {
        let v = json!({ "webSearch": { "provider": "searxng", "searxngUrl": "  https://search.local/  " } });
        let cfg = WebSearchConfig::from_settings_json(&v);
        assert_eq!(cfg.provider, WebSearchProvider::Searxng);
        assert_eq!(cfg.searxng_url.as_deref(), Some("https://search.local"));

        let mut out = json!({});
        cfg.write_settings_json(&mut out);
        assert_eq!(out["webSearch"]["searxngUrl"], "https://search.local");
    }

    #[test]
    fn writes_nested_settings_shape_preserving_other_keys() {
        let mut v = json!({ "theme": "dark" });
        WebSearchConfig {
            provider: WebSearchProvider::Searxng,
            searxng_url: Some("https://s.example".into()),
        }
        .write_settings_json(&mut v);
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["webSearch"]["provider"], "searxng");
        assert_eq!(v["webSearch"]["searxngUrl"], "https://s.example");
    }
}
