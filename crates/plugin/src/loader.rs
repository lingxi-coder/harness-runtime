//! User-config loader.
//!
//! Resolves the `user_config` field values declared by a
//! [`crate::manifest::PluginManifest`] into the substitution map consumed by
//! [`crate::user_config`]:
//!
//! * **sensitive** fields are read from [`secret::CredentialManager`]'s
//!   plugin-secret storage (`pluginSecrets` parity) — NEVER from settings.json;
//! * **non-sensitive** fields are read from the settings-file
//!   `pluginConfigs[plugin].options` map, falling back to a field's declared
//!   `default`.
//!
//! A missing **required** field (sensitive: absent from secure storage;
//! non-sensitive: absent from both options and default) surfaces as
//! [`LoaderError::MissingRequired`]; a missing optional field is skipped.
//!
//! See spec §15.4.

use crate::manifest::PluginManifest;
use secret::CredentialManager;
use serde_json::{Map, Value};
use thiserror::Error;

/// Failure modes for [`resolve_user_config`].
#[derive(Debug, Clone, Error)]
pub enum LoaderError {
    /// A required field was not provided: for a non-sensitive field, absent from
    /// both `pluginConfigs[plugin].options` and the field's `default`; for a
    /// sensitive field, absent from secure storage.
    #[error("missing required user config field: {0}")]
    MissingRequired(String),
    /// Credential storage read failed.
    #[error("credential read failed: {0}")]
    Credential(String),
}

/// Resolve `manifest.user_config` into the `${user_config.KEY}` substitution map
/// (a JSON object keyed by the bare field name).
///
/// `plugin_key` is the plugin identity secrets are namespaced under (the
/// installed `name@marketplace` id, or the bare `name` for a local plugin);
/// `options` is the plugin's `pluginConfigs[plugin].options` map at the active
/// settings scope (empty when the plugin has no persisted non-sensitive config).
///
/// Sensitive fields are read live from [`CredentialManager::get_plugin_secret`]
/// (so a freshly stored secret takes effect without a restart); non-sensitive
/// fields come from `options`, then the field `default`. Missing required fields
/// error; missing optional fields are skipped (NOT inserted as `Null`).
pub async fn resolve_user_config(
    manifest: &PluginManifest,
    plugin_key: &str,
    options: &Map<String, Value>,
    credentials: &CredentialManager,
) -> Result<Value, LoaderError> {
    let Some(schema) = &manifest.user_config else {
        return Ok(Value::Object(Map::new()));
    };
    let mut out = Map::new();
    for (key, field) in &schema.fields {
        if field.sensitive {
            // Secrets live ONLY in secure storage, keyed by (plugin, field).
            match credentials
                .get_plugin_secret(plugin_key, key)
                .await
                .map_err(|e| LoaderError::Credential(e.to_string()))?
            {
                Some(secret) => {
                    out.insert(key.clone(), Value::String(secret.expose_secret().clone()));
                }
                None if field.required => return Err(LoaderError::MissingRequired(key.clone())),
                None => {}
            }
        } else {
            // Non-sensitive: pluginConfigs options, then the declared default.
            let value = options.get(key).cloned().or_else(|| field.default.clone());
            match value {
                Some(v) => {
                    out.insert(key.clone(), v);
                }
                None if field.required => return Err(LoaderError::MissingRequired(key.clone())),
                None => {}
            }
        }
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{UserConfigField, UserConfigSchema};
    use crate::source::PluginSource;
    use crate::trust::PluginTrustLevel;
    use async_trait::async_trait;
    use platform_api::{
        Clock, HttpTransport, SecureStorage, SecureStorageBackend, SecureStorageError,
    };
    use protocol::{PluginId, SecureStorageData};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::SystemTime;

    #[derive(Default)]
    struct MemStorage {
        map: StdMutex<HashMap<(String, String), SecureStorageData>>,
    }

    #[async_trait]
    impl SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), data);
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn delete(&self, service: &str, account: &str) -> Result<(), SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(&self, service: &str) -> Result<Vec<String>, SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|(s, _)| s == service)
                .map(|(_, a)| a.clone())
                .collect())
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::PlainText
        }
    }

    struct FixedClock;
    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }
    }

    struct NoHttp;
    #[async_trait]
    impl HttpTransport for NoHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            panic!("no http");
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            panic!("no http");
        }
    }

    fn creds() -> (Arc<MemStorage>, CredentialManager) {
        let storage = Arc::new(MemStorage::default());
        let cm = CredentialManager::new(
            storage.clone() as Arc<dyn SecureStorage>,
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        );
        (storage, cm)
    }

    fn manifest_with(fields: Vec<(&str, UserConfigField)>) -> PluginManifest {
        let mut map = HashMap::new();
        for (k, f) in fields {
            map.insert(k.to_string(), f);
        }
        PluginManifest {
            id: PluginId::new(),
            name: "weather".into(),
            display_name: None,
            default_enabled: true,
            version: "1.0.0".into(),
            description: String::new(),
            author: None,
            author_email: None,
            author_url: None,
            homepage: None,
            source: PluginSource::BuiltIn,
            components: Default::default(),
            trust_level: PluginTrustLevel::UserTrusted,
            depends_on: Vec::new(),
            dependencies: Vec::new(),
            user_config: Some(UserConfigSchema { fields: map }),
            channels: Vec::new(),
            settings: HashMap::new(),
            settings_declared: false,
            keywords: Vec::new(),
            license: None,
            repository: None,
            metadata: None,
        }
    }

    fn field(sensitive: bool, required: bool, default: Option<Value>) -> UserConfigField {
        UserConfigField {
            description: String::new(),
            sensitive,
            required,
            default,
            ..UserConfigField::default()
        }
    }

    #[tokio::test]
    async fn none_schema_yields_empty_object() {
        let (_s, cm) = creds();
        let mut m = manifest_with(vec![]);
        m.user_config = None;
        let out = resolve_user_config(&m, "weather@acme", &Map::new(), &cm)
            .await
            .unwrap();
        assert_eq!(out, Value::Object(Map::new()));
    }

    #[tokio::test]
    async fn sensitive_resolves_from_secure_storage() {
        let (_s, cm) = creds();
        cm.set_plugin_secret("weather@acme", "API_KEY", "sk-live")
            .await
            .unwrap();
        let m = manifest_with(vec![("API_KEY", field(true, true, None))]);
        let out = resolve_user_config(&m, "weather@acme", &Map::new(), &cm)
            .await
            .unwrap();
        assert_eq!(out["API_KEY"], Value::String("sk-live".into()));
    }

    #[tokio::test]
    async fn required_sensitive_missing_errors_not_null() {
        let (_s, cm) = creds();
        let m = manifest_with(vec![("API_KEY", field(true, true, None))]);
        let err = resolve_user_config(&m, "weather@acme", &Map::new(), &cm)
            .await
            .unwrap_err();
        assert!(matches!(err, LoaderError::MissingRequired(k) if k == "API_KEY"));
    }

    #[tokio::test]
    async fn optional_sensitive_missing_is_skipped_not_null() {
        let (_s, cm) = creds();
        let m = manifest_with(vec![("API_KEY", field(true, false, None))]);
        let out = resolve_user_config(&m, "weather@acme", &Map::new(), &cm)
            .await
            .unwrap();
        // Absent optional secret is SKIPPED (not inserted as Null).
        assert!(out.as_object().unwrap().get("API_KEY").is_none());
    }

    #[tokio::test]
    async fn nonsensitive_reads_from_options_not_manifest_settings() {
        let (_s, cm) = creds();
        let m = manifest_with(vec![("REGION", field(false, true, None))]);
        let mut opts = Map::new();
        opts.insert("REGION".into(), Value::String("us-east".into()));
        let out = resolve_user_config(&m, "weather@acme", &opts, &cm)
            .await
            .unwrap();
        assert_eq!(out["REGION"], Value::String("us-east".into()));
    }

    #[tokio::test]
    async fn nonsensitive_falls_back_to_default() {
        let (_s, cm) = creds();
        let m = manifest_with(vec![("PORT", field(false, true, Some(Value::from(8080))))]);
        let out = resolve_user_config(&m, "weather@acme", &Map::new(), &cm)
            .await
            .unwrap();
        assert_eq!(out["PORT"], Value::from(8080));
    }

    #[tokio::test]
    async fn required_nonsensitive_missing_errors() {
        let (_s, cm) = creds();
        let m = manifest_with(vec![("REGION", field(false, true, None))]);
        let err = resolve_user_config(&m, "weather@acme", &Map::new(), &cm)
            .await
            .unwrap_err();
        assert!(matches!(err, LoaderError::MissingRequired(k) if k == "REGION"));
    }
}
