//! Shared, refreshable official provider catalog. User settings and credentials are separate.
use fs2::FileExt;
use lingxi_llm_client::{presets, protocol as wire};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, RwLock, Weak},
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

/// Protocol and feature support compiled into this runtime. A publication can
/// require these facts but cannot grant the binary a new implementation.
pub const SCHEMA_VERSION: u32 = 2;
/// Capability identifiers understood by this compiled runtime.
pub const RUNTIME_CAPABILITIES: &[&str] = &[
    "protocol.anthropic_messages",
    "protocol.open_ai_responses",
    "protocol.open_ai_chat",
    "protocol.gemini_generate_content",
    "protocol.gemini_interactions",
    "protocol.bedrock_converse",
    "protocol.bedrock_anthropic",
    "protocol.vertex_gemini",
    "protocol.vertex_anthropic",
    "protocol.foundry_claude",
    "auth.api_key",
    "auth.bearer",
    "auth.oauth",
    "reasoning.effort.max",
    "catalog.execution_definition.v1",
];

/// Host lifecycle signals. All timing and retry decisions live in Rust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogTrigger {
    /// Initial application launch.
    Startup,
    /// The host became visible.
    Foreground,
    /// The host is no longer visible.
    Background,
    /// Foreground maintenance tick.
    Timer,
    /// A usable network path returned.
    NetworkRestored,
    /// The host lost its network path.
    NetworkLost,
    /// The provider/model settings page opened.
    Settings,
    /// The user requested an immediate refresh.
    Manual,
}

impl std::str::FromStr for CatalogTrigger {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "startup" => Ok(Self::Startup),
            "foreground" => Ok(Self::Foreground),
            "background" => Ok(Self::Background),
            "timer" => Ok(Self::Timer),
            "network_restored" => Ok(Self::NetworkRestored),
            "network_lost" => Ok(Self::NetworkLost),
            "settings" => Ok(Self::Settings),
            "manual" => Ok(Self::Manual),
            _ => Err("invalid provider catalog trigger".into()),
        }
    }
}

#[derive(Debug)]
struct Shared {
    snapshot: RwLock<Option<Arc<llm_runtime::BuiltinCatalog>>>,
    refresh: tokio::sync::Mutex<()>,
    timing: Mutex<Timing>,
    updates: tokio::sync::watch::Sender<u64>,
}
#[derive(Debug)]
struct Timing {
    foreground: bool,
    online: bool,
    last_success: Option<std::time::Instant>,
    retry_at: Option<std::time::Instant>,
    failures: usize,
    publication: Option<String>,
}
impl Default for Shared {
    fn default() -> Self {
        Self {
            snapshot: RwLock::new(None),
            refresh: tokio::sync::Mutex::new(()),
            timing: Mutex::new(Timing {
                foreground: true,
                online: true,
                last_success: None,
                retry_at: None,
                failures: 0,
                publication: None,
            }),
            updates: tokio::sync::watch::channel(0).0,
        }
    }
}
/// Clones and sibling sessions share one snapshot and one in-flight fetch.
#[derive(Clone, Debug)]
pub struct StartupCatalog {
    endpoint: String,
    platform: CatalogPlatform,
    app_version: String,
    cache_dir: PathBuf,
    shared: Arc<Shared>,
}
impl StartupCatalog {
    /// Construct or join the directory owner for one platform, version and home.
    pub fn new(platform: CatalogPlatform, app_version: impl Into<String>, home: &Path) -> Self {
        let app_version = app_version.into();
        let cache_dir = home.join("cache/provider-catalog-v2");
        static OWNERS: OnceLock<Mutex<BTreeMap<String, Weak<Shared>>>> = OnceLock::new();
        let key = format!(
            "{}\0{}\0{}",
            cache_dir.display(),
            platform.label(),
            app_version
        );
        let mut owners = OWNERS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        owners.retain(|_, owner| owner.strong_count() > 0);
        let shared = owners.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let owner = Arc::new(Shared::default());
            owners.insert(key, Arc::downgrade(&owner));
            owner
        });
        Self {
            endpoint: PUBLIC_ENDPOINT.into(),
            platform,
            app_version,
            cache_dir,
            shared,
        }
    }

    /// A stable snapshot for an individual runtime/task. Updates replace the
    /// shared Arc; existing owners retain their previous execution facts.
    pub fn snapshot(&self) -> Arc<llm_runtime::BuiltinCatalog> {
        if let Some(value) = self
            .shared
            .snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return value.clone();
        }
        let initial = Arc::new(
            self.cached()
                .map(|(_, p)| p.catalog)
                .unwrap_or_else(|| self.embedded_catalog()),
        );
        let mut current = self
            .shared
            .snapshot
            .write()
            .unwrap_or_else(|e| e.into_inner());
        current.get_or_insert(initial).clone()
    }
    /// Subscribe to validated publication changes.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.shared.updates.subscribe()
    }

    /// Read the current local snapshot without waiting for the network.
    pub async fn load(&self) -> Arc<llm_runtime::BuiltinCatalog> {
        self.snapshot()
    }

    /// One application-owned guard keeps scheduled checks alive. No network is
    /// performed while backgrounded; native lifecycle signals wake the service.
    pub fn prefetch(&self) -> CatalogRefreshGuard {
        let source = self.clone();
        CatalogRefreshGuard(tokio::spawn(async move {
            let _ = source.trigger(CatalogTrigger::Startup).await;
            let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 31;
            loop {
                tokio::time::sleep(Duration::from_secs(60 + jitter)).await;
                let _ = source.trigger(CatalogTrigger::Timer).await;
            }
        }))
    }

    /// Apply a lifecycle signal and perform a refresh only when policy allows.
    pub async fn trigger(&self, trigger: CatalogTrigger) -> Result<bool, String> {
        let _ = self.snapshot();
        {
            let mut timing = self.shared.timing.lock().unwrap_or_else(|e| e.into_inner());
            if trigger == CatalogTrigger::Background {
                timing.foreground = false;
                return Ok(false);
            }
            if trigger == CatalogTrigger::Foreground {
                timing.foreground = true;
            }
            if trigger == CatalogTrigger::NetworkLost {
                timing.online = false;
                return Ok(false);
            }
            if trigger == CatalogTrigger::NetworkRestored {
                timing.online = true;
            }
            if !timing.online {
                return Ok(false);
            }
            if !timing.foreground && trigger != CatalogTrigger::Manual {
                return Ok(false);
            }
            let now = std::time::Instant::now();
            let age = timing.last_success.map(|last| now.duration_since(last));
            let interval = if trigger == CatalogTrigger::Settings {
                900
            } else {
                3600
            };
            if trigger != CatalogTrigger::Manual {
                if timing.retry_at.is_some_and(|next| now < next)
                    && trigger != CatalogTrigger::NetworkRestored
                {
                    return Ok(false);
                }
                if timing.failures == 0
                    && age.is_some_and(|age| age < Duration::from_secs(interval))
                {
                    return Ok(false);
                }
            }
        }
        // Concurrent triggers join the first fetch instead of queuing downloads.
        let Ok(_guard) = self.shared.refresh.try_lock() else {
            return Ok(false);
        };
        fs::create_dir_all(&self.cache_dir).map_err(|e| e.to_string())?;
        let fetch_lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.cache_path().with_extension("fetch-lock"))
            .map_err(|e| e.to_string())?;
        if fetch_lock.try_lock_exclusive().is_err() {
            return Ok(false);
        }
        // A sibling process may already have refreshed the common cache.
        if trigger != CatalogTrigger::Manual {
            if let Some((saved, parsed)) = self
                .cached()
                .filter(|(saved, _)| unix_millis().saturating_sub(saved.checked_at) < 60_000)
            {
                let mut timing = self.shared.timing.lock().unwrap_or_else(|e| e.into_inner());
                let changed = timing.publication.as_ref() != Some(&saved.body_sha256);
                timing.publication = Some(saved.body_sha256);
                timing.last_success = Some(std::time::Instant::now());
                timing.failures = 0;
                timing.retry_at = None;
                drop(timing);
                *self
                    .shared
                    .snapshot
                    .write()
                    .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(parsed.catalog));
                if changed {
                    self.shared
                        .updates
                        .send_modify(|generation| *generation += 1);
                }
                return Ok(changed);
            }
        }
        let generation = *self.shared.updates.borrow();
        let result = self.fetch().await;
        let mut timing = self.shared.timing.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok((catalog, publication)) => {
                timing.last_success = Some(std::time::Instant::now());
                timing.failures = 0;
                timing.retry_at = None;
                let publication = Some(publication);
                let changed = timing.publication != publication;
                timing.publication = publication;
                drop(timing);
                let catalog = Arc::new(catalog);
                let mut current = self
                    .shared
                    .snapshot
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                *current = Some(catalog);
                if changed {
                    self.shared.updates.send_replace(generation + 1);
                }
                Ok(changed)
            }
            Err(error) => {
                let minutes = [1, 5, 15, 60][timing.failures.min(3)];
                timing.failures += 1;
                timing.retry_at =
                    Some(std::time::Instant::now() + Duration::from_secs(minutes * 60));
                Err(error)
            }
        }
    }

    /// Perform an explicit user refresh while preserving the last valid snapshot on failure.
    pub async fn refresh(&self) -> Result<llm_runtime::BuiltinCatalog, String> {
        self.trigger(CatalogTrigger::Manual).await?;
        Ok((*self.snapshot()).clone())
    }

    fn cache_path(&self) -> PathBuf {
        let key = format!(
            "{}\0{}\0{}\02\0{}",
            self.endpoint,
            self.platform.label(),
            self.app_version,
            RUNTIME_CAPABILITIES.join(",")
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

    /// Fetch and validate a complete publication, preserving its ETag and checksum.
    async fn fetch(&self) -> Result<(llm_runtime::BuiltinCatalog, String), String> {
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
            ("schemaVersion", "2"),
        ]);
        request = request.query(
            &RUNTIME_CAPABILITIES
                .iter()
                .map(|cap| ("capability", *cap))
                .collect::<Vec<_>>(),
        );
        if let Some(etag) = cached.as_ref().and_then(|(saved, _)| saved.etag.as_ref()) {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let mut response = request.send().await.map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return cached
                .map(|(mut saved, parsed)| {
                    saved.checked_at = unix_millis();
                    let _ = self.save(&saved, parsed.published_at);
                    (parsed.catalog, saved.body_sha256)
                })
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
            body_sha256: body_sha256.clone(),
            checked_at: unix_millis(),
        };
        if let Err(error) = self.save(&saved, parsed.published_at) {
            tracing::warn!(%error, "could not cache official model catalog");
        }
        if let Some((_, latest)) = self
            .cached()
            .filter(|(_, latest)| latest.published_at > parsed.published_at)
        {
            return Ok((
                latest.catalog,
                self.cached()
                    .expect("latest cached catalog exists")
                    .0
                    .body_sha256,
            ));
        }
        Ok((parsed.catalog, body_sha256))
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

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Reassembles cloud execution facts with the original host-owned user
/// configuration. Provider credentials are inherited from the running client.
#[derive(Debug)]
struct LiveRuntime {
    source: StartupCatalog,
    inputs: crate::AssembleInputs,
    region: llm_runtime::Region,
    cached: Mutex<Option<(u64, Arc<llm_runtime::ModelRuntime>)>>,
}
impl llm_runtime::client::ModelRuntimeSource for LiveRuntime {
    fn current(&self, template: &llm_runtime::ModelRuntime) -> Arc<llm_runtime::ModelRuntime> {
        let generation = *self.source.shared.updates.borrow();
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((revision, client)) = cached
            .as_ref()
            .filter(|(revision, _)| *revision == generation)
        {
            let _ = revision;
            return client.with_owner_state(template);
        }
        let mut inputs = self.inputs.clone();
        inputs.official_catalog = Some((*self.source.snapshot()).clone());
        let mut assembled = crate::assemble_for_region(inputs, self.region);
        if matches!(
            self.source.platform,
            CatalogPlatform::Android | CatalogPlatform::Ios
        ) {
            if let Some(enabled) = self
                .inputs
                .routing
                .as_ref()
                .and_then(|r| r.get("mobileEnabledProfiles"))
            {
                assembled.client_config.providers.retain(|p| {
                    enabled.as_array().is_some_and(|ids| {
                        ids.iter().any(|id| id.as_str() == Some(&p.profile_name))
                    })
                });
            }
        }
        match template.reconfigured(assembled.client_config) {
            Ok(client) => {
                let client = Arc::new(client);
                *cached = Some((generation, client.clone()));
                client.with_owner_state(template)
            }
            Err(error) => {
                tracing::error!(%error, "catalog execution projection failed");
                cached
                    .as_ref()
                    .map(|(_, client)| client.with_owner_state(template))
                    .unwrap_or_else(|| {
                        Arc::new(
                            template
                                .reconfigured(
                                    crate::assemble_for_region(self.inputs.clone(), self.region)
                                        .client_config,
                                )
                                .expect("initial execution catalog was already validated"),
                        )
                    })
            }
        }
    }
}
impl StartupCatalog {
    /// Project directory publications into executable profiles while retaining user settings.
    pub fn runtime_source(
        &self,
        inputs: crate::AssembleInputs,
        region: llm_runtime::Region,
    ) -> Arc<dyn llm_runtime::client::ModelRuntimeSource> {
        Arc::new(LiveRuntime {
            source: self.clone(),
            inputs,
            region,
            cached: Mutex::new(None),
        })
    }
}

/// Owns a bounded background startup fetch, never an application-global task.
pub struct CatalogRefreshGuard(
    /// Background work aborted when its host owner is dropped.
    pub tokio::task::JoinHandle<()>,
);
impl Drop for CatalogRefreshGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
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
    checked_at: u64,
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
    execution_definition: Definition,
    required_capabilities: Vec<String>,
    default_model_id: String,
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
    status: ModelStatus,
    required_capabilities: Vec<String>,
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
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ModelStatus {
    Active,
    Deprecated,
    Disabled,
}
fn supports(required: &[String]) -> bool {
    required
        .iter()
        .all(|c| RUNTIME_CAPABILITIES.contains(&c.as_str()))
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
    if doc.schema_version != SCHEMA_VERSION
        || doc.release_id.is_empty()
        || doc.providers.len() > 100
        || doc.models.len() > 5000
    {
        return Err("invalid catalog envelope".into());
    }
    let mut identities = BTreeSet::new();
    for p in &doc.providers {
        if p.id.is_empty()
            || p.id.len() > 64
            || !p
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            || !p.id.as_bytes()[0].is_ascii_alphanumeric()
            || !identities.insert(p.id.clone())
        {
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
        if !supports(&provider.required_capabilities) {
            continue;
        }
        let definition = &provider.execution_definition;
        if definition.format != "llm-client-preset-toml"
            || format!("{:x}", Sha256::digest(definition.source.as_bytes())) != definition.sha256
        {
            return Err("invalid execution definition hash or format".into());
        }
        let parsed = presets::parse_provider_definition(&provider.id, &definition.source)
            .map_err(|e| e.to_string())?;
        if !provider
            .required_capabilities
            .contains(&format!("protocol.{}", provider.protocol))
        {
            return Err("provider must require its compiled protocol capability".into());
        }
        if !parsed.declared_models.contains(&provider.default_model_id) {
            return Err("provider default is not declared".into());
        }
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
            if !supports(&row.required_capabilities)
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
            model.metadata.status = if !provider.enabled || row.status == ModelStatus::Disabled {
                Some("disabled".into())
            } else if row.status == ModelStatus::Deprecated {
                Some("deprecated".into())
            } else {
                None
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
        // Put the published default first only when it survives capability and
        // platform filtering. Never rewrite the user's configured selection.
        if let Some(index) = selected
            .iter()
            .position(|m| m.request_model == provider.default_model_id)
        {
            let default = selected.remove(index);
            selected.insert(0, default);
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
    llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: catalog.providers.clone(),
    })
    .map_err(|error| format!("official catalog cannot construct an execution registry: {error}"))?;
    Ok(ParsedCatalog {
        catalog,
        published_at: doc.published_at,
    })
}

impl StartupCatalog {
    fn embedded_catalog(&self) -> llm_runtime::BuiltinCatalog {
        parse_catalog(
            include_str!("../data/official-catalog.json"),
            self.platform,
            &self.app_version,
        )
        .expect("compiled official catalog must be validated")
        .catalog
    }
}

#[cfg(test)]
mod tests;
