//! Public catalog delivery. No settings or credential store is written here.
use fs2::FileExt;
use lingxi_llm_client::{presets, protocol as wire};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Credential-free project-owned catalog endpoint.
pub const PUBLIC_ENDPOINT: &str =
    "https://us-central1-lingxi-5e4b2.cloudfunctions.net/catalogApi/v1/model-catalog";
const MAX_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// Application surface used by publication/version filtering.
pub enum CatalogPlatform {
    /// Electron desktop host.
    Desktop,
    /// CLI and TUI host.
    Cli,
    /// Android native host.
    Android,
    /// iOS native host.
    Ios,
}
impl CatalogPlatform {
    fn label(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Cli => "cli",
            Self::Android => "android",
            Self::Ios => "ios",
        }
    }
}

/// One host-owned startup recipe. Clones share a settled snapshot, so settings
/// refreshes and sibling sessions cannot silently revert to embedded defaults.
#[derive(Clone, Debug)]
pub struct StartupCatalog {
    endpoint: String,
    platform: CatalogPlatform,
    app_version: String,
    cache_dir: PathBuf,
    settled: Arc<tokio::sync::OnceCell<Arc<llm_runtime::BuiltinCatalog>>>,
}
impl StartupCatalog {
    /// Bind a catalog request and its private cache to this application owner.
    pub fn new(platform: CatalogPlatform, app_version: impl Into<String>, home: &Path) -> Self {
        Self {
            endpoint: PUBLIC_ENDPOINT.into(),
            platform,
            app_version: app_version.into(),
            cache_dir: home.join("cache/model-catalog"),
            settled: Arc::default(),
        }
    }

    /// Settle the local snapshot while unrelated host startup runs. Dropping
    /// the guard cancels this settling task; network refresh is always bounded.
    pub fn prefetch(&self) -> CatalogRefreshGuard {
        let source = self.clone();
        CatalogRefreshGuard(tokio::spawn(async move {
            source.load().await;
        }))
    }

    /// Settle once before constructing a runtime. Existing runtimes keep their
    /// immutable request snapshot; no in-flight route or credential is changed.
    pub async fn load(&self) -> Arc<llm_runtime::BuiltinCatalog> {
        self.settled
            .get_or_init(|| async {
                let catalog = self
                    .cached()
                    .map(|(_, parsed)| parsed.catalog)
                    .unwrap_or_else(llm_runtime::builtin_presets);
                // Neither cold nor cached startup waits for the network. The
                // selected snapshot remains immutable for this runtime owner.
                let source = self.clone();
                tokio::spawn(async move {
                    if let Err(error) = source.refresh().await {
                        tracing::warn!(%error, "background official model catalog refresh failed");
                    }
                });
                Arc::new(catalog)
            })
            .await
            .clone()
    }

    fn cache_path(&self) -> PathBuf {
        let key = format!(
            "{}\0{}\0{}\01",
            self.endpoint,
            self.platform.label(),
            self.app_version
        );
        self.cache_dir
            .join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
    }

    fn parse(&self, body: &str) -> Result<ParsedCatalog, String> {
        parse_catalog(body, self.platform, &self.app_version)
    }

    fn cached(&self) -> Option<(Cached, ParsedCatalog)> {
        let file = fs::File::open(self.cache_path()).ok()?;
        let mut bytes = Vec::new();
        file.take((MAX_BODY * 2 + 4096 + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_BODY * 2 + 4096 {
            return None;
        }
        let saved: Cached = serde_json::from_slice(&bytes).ok()?;
        if format!("{:x}", Sha256::digest(saved.body.as_bytes())) != saved.body_sha256 {
            return None;
        }
        if saved.endpoint != self.endpoint
            || saved.platform != self.platform
            || saved.app_version != self.app_version
        {
            return None;
        }
        let parsed = self.parse(&saved.body).ok()?;
        Some((saved, parsed))
    }

    /// Fetch, validate and cache a publication for the next runtime owner.
    /// This explicit prewarm operation never replaces an already settled snapshot.
    pub async fn refresh(&self) -> Result<llm_runtime::BuiltinCatalog, String> {
        semver::Version::parse(&self.app_version).map_err(|_| "invalid application version")?;
        let url = reqwest::Url::parse(&self.endpoint).map_err(|_| "invalid catalog endpoint")?;
        if url.scheme() != "https"
            && !(cfg!(test) && url.host_str().is_some_and(|h| h == "127.0.0.1"))
        {
            return Err("catalog endpoint requires HTTPS".into());
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("catalog endpoint must not contain credentials".into());
        }
        let cached = self.cached();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        let mut request = client.get(url).query(&[
            ("platform", self.platform.label()),
            ("appVersion", self.app_version.as_str()),
            ("schemaVersion", "1"),
        ]);
        if let Some(etag) = cached.as_ref().and_then(|(saved, _)| saved.etag.as_ref()) {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let mut response = request.send().await.map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return cached
                .map(|(_, parsed)| parsed.catalog)
                .ok_or_else(|| "304 without valid cache".into());
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(format!("catalog HTTP {}", response.status()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err("catalog exceeds size limit".into());
        }
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .filter(|h| h.len() <= 256)
            .map(str::to_owned);
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if chunk.len() > MAX_BODY - bytes.len() {
                return Err("catalog exceeds size limit".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes).map_err(|_| "catalog is not UTF-8")?;
        let parsed = self.parse(&body)?;
        if let Some((_, previous)) = &cached {
            if parsed.published_at < previous.published_at {
                return Err("catalog publication regressed".into());
            }
        }
        let body_sha256 = format!("{:x}", Sha256::digest(body.as_bytes()));
        let saved = Cached {
            endpoint: self.endpoint.clone(),
            platform: self.platform,
            app_version: self.app_version.clone(),
            etag,
            body,
            body_sha256,
        };
        if let Err(error) = self.save(&saved, parsed.published_at) {
            tracing::warn!(%error, "could not cache official model catalog");
        }
        if let Some((_, latest)) = self
            .cached()
            .filter(|(_, latest)| latest.published_at > parsed.published_at)
        {
            return Ok(latest.catalog);
        }
        Ok(parsed.catalog)
    }

    fn save(&self, saved: &Cached, published_at: u64) -> Result<(), String> {
        fs::create_dir_all(&self.cache_dir).map_err(|e| e.to_string())?;
        let target = self.cache_path();
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(target.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        lock.try_lock_exclusive().map_err(|e| e.to_string())?;
        if self
            .cached()
            .is_some_and(|(_, parsed)| parsed.published_at > published_at)
        {
            return Ok(());
        }
        let temp = self.cache_dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp).map_err(|e| e.to_string())?;
            let bytes = serde_json::to_vec(saved).map_err(|e| e.to_string())?;
            file.write_all(&bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&temp, &target).map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
}

/// Owns a bounded background startup fetch, never an application-global task.
pub struct CatalogRefreshGuard(tokio::task::JoinHandle<()>);
impl Drop for CatalogRefreshGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Choose a current route only when a previously built-in default was retired
/// by the official snapshot. Custom models and ambiguous live routes are kept.
pub fn retired_default(
    configured: &str,
    providers: &[llm_runtime::ProviderProfile],
) -> Option<(String, String)> {
    let matches = |profile: &llm_runtime::ProviderProfile| {
        let qualified = configured.strip_prefix(&format!("{}/", profile.profile_name));
        profile.models.iter().any(|model| {
            std::iter::once(&model.request_model)
                .chain(std::iter::once(&model.display_model))
                .chain(model.aliases.iter())
                .any(|id| id == configured || qualified == Some(id.as_str()))
        })
    };
    if providers.iter().any(matches) {
        return None;
    }
    let embedded = llm_runtime::builtin_presets();
    let previous = embedded.providers.iter().find(|profile| matches(profile))?;
    let provider = providers
        .iter()
        .find(|p| p.profile_name == previous.profile_name && !p.models.is_empty())
        .or_else(|| {
            providers
                .iter()
                .find(|p| p.profile_name != "openai-chatgpt" && !p.models.is_empty())
        })
        .or_else(|| providers.iter().find(|p| !p.models.is_empty()))?;
    Some((
        provider.models[0].request_model.clone(),
        provider.profile_name.clone(),
    ))
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cached {
    endpoint: String,
    platform: CatalogPlatform,
    app_version: String,
    etag: Option<String>,
    body: String,
    body_sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Document {
    schema_version: u32,
    release_id: String,
    published_at: u64,
    providers: Vec<Provider>,
    models: Vec<Model>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Provider {
    id: String,
    name: String,
    description: String,
    base_url: String,
    protocol: String,
    enabled: bool,
    order: u32,
    regions: Vec<wire::Region>,
    execution_definition: Option<Definition>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    format: String,
    source: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Model {
    provider_id: String,
    id: String,
    name: String,
    description: String,
    enabled: bool,
    order: u32,
    platforms: Vec<CatalogPlatform>,
    min_app_version: String,
    max_app_version: Option<String>,
    context_window: u64,
    max_output_tokens: u64,
    capabilities: Capabilities,
    input_modalities: Vec<String>,
    output_modalities: Vec<String>,
    pricing: Option<Price>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    tools: bool,
    vision: bool,
    reasoning: bool,
    audio: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Price {
    currency: String,
    input_per_million: f64,
    output_per_million: f64,
    cache_read_per_million: Option<f64>,
    cache_write_per_million: Option<f64>,
}
struct ParsedCatalog {
    catalog: llm_runtime::BuiltinCatalog,
    published_at: u64,
}

fn parse_catalog(
    body: &str,
    platform: CatalogPlatform,
    app_version: &str,
) -> Result<ParsedCatalog, String> {
    if body.len() > MAX_BODY {
        return Err("catalog exceeds size limit".into());
    }
    let app = semver::Version::parse(app_version).map_err(|_| "invalid application version")?;
    let mut doc: Document = serde_json::from_str(body).map_err(|e| e.to_string())?;
    if doc.schema_version != 1
        || doc.release_id.is_empty()
        || doc.providers.len() > 100
        || doc.models.len() > 5000
    {
        return Err("invalid catalog envelope".into());
    }
    let mut identities = BTreeSet::new();
    for p in &doc.providers {
        if p.id.is_empty() || !identities.insert(p.id.clone()) {
            return Err("duplicate or empty provider identity".into());
        }
    }
    let mut model_ids = BTreeSet::new();
    for model in &doc.models {
        if !identities.contains(&model.provider_id)
            || !model_ids.insert((model.provider_id.clone(), model.id.clone()))
        {
            return Err("invalid model identity".into());
        }
        if model.context_window == 0
            || model.context_window > 100_000_000
            || model.max_output_tokens == 0
            || model.max_output_tokens > model.context_window
        {
            return Err("invalid model token limits".into());
        }
        let min =
            semver::Version::parse(&model.min_app_version).map_err(|_| "invalid version range")?;
        let max = model
            .max_app_version
            .as_deref()
            .map(semver::Version::parse)
            .transpose()
            .map_err(|_| "invalid version range")?;
        if max
            .as_ref()
            .is_some_and(|max| min.cmp_precedence(max).is_gt())
        {
            return Err("invalid version range".into());
        }
        if let Some(price) = &model.pricing {
            if !matches!(price.currency.as_str(), "USD" | "CNY")
                || [
                    Some(price.input_per_million),
                    Some(price.output_per_million),
                    price.cache_read_per_million,
                    price.cache_write_per_million,
                ]
                .into_iter()
                .flatten()
                .any(|n| !n.is_finite() || n < 0.0)
            {
                return Err("invalid model price".into());
            }
        }
    }
    doc.providers
        .sort_by(|a, b| a.order.cmp(&b.order).then(a.id.cmp(&b.id)));
    doc.models
        .sort_by(|a, b| a.order.cmp(&b.order).then(a.id.cmp(&b.id)));
    let mut rows: BTreeMap<&str, Vec<&Model>> = BTreeMap::new();
    for model in &doc.models {
        rows.entry(&model.provider_id).or_default().push(model);
    }
    let mut profiles = Vec::new();
    for provider in &doc.providers {
        if !provider.enabled {
            continue;
        }
        let parsed = match &provider.execution_definition {
            Some(definition) => {
                if definition.format != "llm-client-preset-toml"
                    || format!("{:x}", Sha256::digest(definition.source.as_bytes()))
                        != definition.sha256
                {
                    return Err("invalid execution definition hash or format".into());
                }
                presets::parse_provider_definition(&provider.id, &definition.source)
                    .map_err(|e| e.to_string())?
            }
            None => presets::builtin_provider_definition(&provider.id)
                .map_err(|e| e.to_string())?
                .ok_or("new provider requires a complete execution definition")?,
        };
        let mut profile = parsed.profile;
        let protocol = serde_json::to_value(profile.protocol).map_err(|e| e.to_string())?;
        if protocol.as_str() != Some(provider.protocol.as_str()) {
            return Err("provider protocol conflicts with its execution definition".into());
        }
        let url = reqwest::Url::parse(&provider.base_url).map_err(|_| "invalid provider URL")?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err("invalid provider URL".into());
        }
        profile.base_url = provider.base_url.clone();
        profile.regions = provider.regions.clone();
        profile.info.display_name = Some(provider.name.clone());
        profile.info.description = Some(provider.description.clone());
        let mut selected = Vec::new();
        for row in rows.get(provider.id.as_str()).into_iter().flatten() {
            if !parsed.declared_models.contains(&row.id) {
                return Err(format!(
                    "model {}/{} requires an execution definition",
                    provider.id, row.id
                ));
            }
            let min = semver::Version::parse(&row.min_app_version).map_err(|e| e.to_string())?;
            let max = row
                .max_app_version
                .as_deref()
                .map(semver::Version::parse)
                .transpose()
                .map_err(|e| e.to_string())?;
            // Build metadata does not affect SemVer precedence.
            if !row.enabled
                || !row.platforms.contains(&platform)
                || app.cmp_precedence(&min).is_lt()
                || max
                    .as_ref()
                    .is_some_and(|max| app.cmp_precedence(max).is_gt())
            {
                continue;
            }
            // A declared service-only model is not promoted to a chat route.
            let Some(mut model) = profile
                .models
                .iter()
                .find(|m| m.request_model == row.id)
                .cloned()
            else {
                continue;
            };
            model.display_model = row.name.clone();
            model.description = Some(row.description.clone());
            model.metadata.context_window_tokens = Some(row.context_window);
            model.metadata.context_window_source = wire::ContextWindowSource::Catalog;
            model.metadata.max_output_tokens = Some(row.max_output_tokens);
            model.metadata.input_modalities = row.input_modalities.clone();
            model.metadata.output_modalities = row.output_modalities.clone();
            let support = model
                .capability_support
                .get_or_insert_with(Default::default);
            let state = |enabled| {
                if enabled {
                    wire::CapabilitySupport::Supported
                } else {
                    wire::CapabilitySupport::Unsupported
                }
            };
            support.tools = state(row.capabilities.tools);
            support.vision = state(row.capabilities.vision);
            support.reasoning = state(row.capabilities.reasoning);
            // Audio service declarations remain in the complete SDK profile.
            let _ = row.capabilities.audio;
            if let Some(price) = &row.pricing {
                let rates = model.pricing.get_or_insert_with(Default::default);
                rates.currency = Some(price.currency.clone());
                rates.input_per_million = Some(price.input_per_million);
                rates.output_per_million = Some(price.output_per_million);
                rates.cache_read_per_million = price.cache_read_per_million;
                rates.cache_write_per_million = price.cache_write_per_million;
            } else {
                model.pricing = None;
            }
            selected.push(model);
        }
        profile.models = selected;
        profiles.push(profile);
    }
    let mut catalog = llm_runtime::catalog_from_profiles(profiles);
    for provider in &mut catalog.providers {
        for model in &mut provider.models {
            model.metadata.catalog_offered = true;
        }
    }
    // Sign-in is a host-owned connection, independent of the public OpenAI API
    // inventory and its platform/version availability rules.
    catalog
        .providers
        .retain(|p| p.profile_name != "openai-chatgpt");
    catalog.providers.extend(
        llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .filter(|p| p.profile_name == "openai-chatgpt"),
    );
    llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers.clone(),
    })
    .map_err(|error| format!("official catalog cannot construct an execution registry: {error}"))?;
    Ok(ParsedCatalog {
        catalog,
        published_at: doc.published_at,
    })
}

#[cfg(test)]
mod tests;
