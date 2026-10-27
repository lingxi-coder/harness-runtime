//! Strict, portable source-package contract. Host permissions are never manifest fields.
use super::AppError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Filename of the app package manifest.
pub const MANIFEST_FILE: &str = "lingxi-app.json";
/// Capabilities understood by the app manifest.
pub const KNOWN_CAPABILITIES: &[&str] = &[
    "process",
    "files",
    "clipboard",
    "notifications",
    "camera",
    "microphone",
    "photos",
    "share",
    "location",
    "calendar",
    "contacts",
    "haptics",
    "network",
    "agent",
    "background",
];
/// Portable app package manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    /// Manifest schema version.
    pub schema: u32,
    /// Name of this item.
    pub name: String,
    /// Short description.
    #[serde(default)]
    pub summary: String,
    /// Entry point used to open the app.
    pub entry: Entry,
    /// Commands used to install dependencies.
    #[serde(default)]
    pub install: Vec<Step>,
    /// Commands used to build the app.
    #[serde(default)]
    pub build: Vec<Step>,
    /// Processes declared by the app.
    #[serde(default)]
    pub processes: Vec<Process>,
    /// Declared capabilities.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Supported host targets.
    pub targets: Vec<Target>,
    /// Data schema compatibility declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_schema: Option<DataSchema>,
    /// Remix policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remix: Option<Remix>,
    /// Package file declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<PackageFiles>,
    /// Agent capabilities requested by this release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentRequirements>,
}
/// Entry point of a local app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Entry {
    /// A static file entry.
    Static {
        /// Path associated with this item.
        path: String,
    },
    /// An entry served by a declared process.
    Server {
        /// Name of the serving process.
        process: String,
        /// Path associated with this item.
        #[serde(default)]
        path: Option<String>,
    },
}
/// Install or build command declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    /// Command to execute.
    pub command: String,
    /// Working directory for the command.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment variables for the command.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Virtual environment used by the command.
    #[serde(default)]
    pub venv: Option<String>,
    /// Python version or interpreter selector requested by the command.
    #[serde(default)]
    pub python: Option<String>,
}
/// App service process declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Process {
    /// Name of this item.
    pub name: String,
    /// Command to execute.
    pub command: String,
    /// Working directory for the command.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment variables for the command.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Virtual environment used by the command.
    #[serde(default)]
    pub venv: Option<String>,
    /// Python version or interpreter selector requested by the command.
    #[serde(default)]
    pub python: Option<String>,
    /// Names of prerequisite processes.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Process readiness condition.
    #[serde(default)]
    pub ready: Option<Readiness>,
}
/// Conditions used to determine process readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Readiness {
    /// Path associated with this item.
    #[serde(default)]
    pub path: Option<String>,
    /// Structured output, when present.
    #[serde(default)]
    pub output: Option<String>,
    /// Timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
/// Runtime target supported by an app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    /// Target platform identifier.
    pub platform: String,
    /// Runtime environment identifier.
    pub environment: String,
    /// Supported machine architecture, when restricted.
    #[serde(default)]
    pub architecture: Option<String>,
    /// Minimum supported runtime semantic version.
    #[serde(default)]
    pub runtime_version: Option<String>,
    /// Required runtime ABI, when restricted.
    #[serde(default)]
    pub runtime_abi: Option<String>,
    /// Supported distribution channels.
    #[serde(default)]
    pub channels: Vec<String>,
    /// Declared capabilities.
    #[serde(default)]
    pub capabilities: Vec<String>,
}
/// App data schema version and compatibility range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DataSchema {
    /// Schema version.
    pub version: u32,
    /// Earliest compatible data schema version.
    pub compatible_from: u32,
}
/// Policy governing remixing of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Remix {
    /// Whether remixing is allowed.
    pub allowed: bool,
    /// Declared remix license.
    #[serde(default)]
    pub license: Option<String>,
}
/// Files and lock files included in a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageFiles {
    /// Files included in the release or package.
    pub files: Vec<String>,
    /// Paths to packaged dependency lock files.
    #[serde(default)]
    pub dependency_locks: Vec<String>,
}
/// Agent tools and skills declared by an app release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentRequirements {
    /// Tool names required by the app's Agent tasks.
    pub tools: Vec<String>,
    /// Release-local Skill instructions the native Agent may load. Each ID is
    /// stable within the immutable release and points to a packaged file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
}
/// Release-local skill declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSkill {
    /// Stable identifier.
    pub id: String,
    /// Path associated with this item.
    pub path: String,
}

/// Validate that a path is a contained relative path.
pub fn relative_path(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.contains(['\\', '\0', ':'])
        || value.starts_with('/')
        || value
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        return Err(AppError::Invalid(format!(
            "expected contained relative path: {value}"
        )));
    }
    Ok(())
}
fn capabilities(values: &[String]) -> Result<(), AppError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !KNOWN_CAPABILITIES.contains(&value.as_str()) || !seen.insert(value) {
            return Err(AppError::Invalid(format!(
                "unknown or duplicate capability: {value}"
            )));
        }
    }
    Ok(())
}
fn token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn check_step(
    command: &str,
    cwd: &Option<String>,
    env: &BTreeMap<String, String>,
    venv: &Option<String>,
    python: &Option<String>,
) -> Result<(), AppError> {
    if command.trim().is_empty() || command.len() > 65536 || command.contains('\0') {
        return Err(AppError::Invalid("invalid command".into()));
    }
    for p in [cwd, venv].into_iter().flatten() {
        if p != "." {
            relative_path(p)?;
        }
    }
    if python.as_ref().is_some_and(|p| !token(p)) {
        return Err(AppError::Invalid(
            "python must be a version or interpreter selector, not a command/path".into(),
        ));
    }
    for (key, value) in env {
        if key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || key.as_bytes()[0].is_ascii_digit()
            || value.contains('\0')
        {
            return Err(AppError::Invalid("invalid environment entry".into()));
        }
    }
    Ok(())
}
impl Manifest {
    /// Public source packages cannot carry process environment credentials.
    /// The host must obtain runtime secrets separately, outside this manifest.
    pub fn validate_public_export(&self) -> Result<(), AppError> {
        self.validate()?;
        if self.package.is_none() {
            return Err(AppError::Invalid(
                "public export requires explicit package.files".into(),
            ));
        }
        for env in self
            .install
            .iter()
            .chain(&self.build)
            .map(|step| &step.env)
            .chain(self.processes.iter().map(|process| &process.env))
        {
            for key in env.keys() {
                let upper = key.to_ascii_uppercase();
                if [
                    "TOKEN",
                    "API_KEY",
                    "APIKEY",
                    "SECRET",
                    "PASSWORD",
                    "PRIVATE_KEY",
                    "CREDENTIAL",
                    "AUTHORIZATION",
                    "COOKIE",
                ]
                .iter()
                .any(|needle| upper.contains(needle))
                {
                    return Err(AppError::Invalid(format!(
                        "public manifest cannot contain credential environment variable {key}"
                    )));
                }
            }
        }
        Ok(())
    }
    /// Parse and validate a manifest.
    pub fn parse(json: &str) -> Result<Self, AppError> {
        let value: Self = serde_json::from_str(json)?;
        value.validate()?;
        Ok(value)
    }
    /// Return whether opening the manifest requires execution.
    pub fn requires_execution(&self) -> bool {
        self.capabilities
            .iter()
            .any(|capability| capability == "process")
            || !self.install.is_empty()
            || !self.build.is_empty()
            || !self.processes.is_empty()
            || matches!(self.entry, Entry::Server { .. })
    }
    /// Return the manifest data schema version.
    pub fn schema_version(&self) -> u32 {
        self.data_schema.as_ref().map_or(1, |s| s.version)
    }
    /// Returns whether the manifest accepts the current data schema.
    pub fn accepts_data_schema(&self, current: u32) -> bool {
        self.data_schema.as_ref().map_or(current == 1, |s| {
            current >= s.compatible_from && current <= s.version
        })
    }
    /// Validate this value.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema != 2 {
            return Err(AppError::Invalid(
                "only manifest schema 2 is supported".into(),
            ));
        }
        if self.name.trim().is_empty()
            || self.name.chars().count() > 80
            || self.summary.chars().count() > 1000
        {
            return Err(AppError::Invalid("invalid name or summary length".into()));
        }
        capabilities(&self.capabilities)?;
        if self.targets.is_empty() {
            return Err(AppError::Invalid("at least one target is required".into()));
        }
        for target in &self.targets {
            target.validate()?;
        }
        if let Some(schema) = &self.data_schema {
            if schema.version == 0
                || schema.compatible_from == 0
                || schema.compatible_from > schema.version
            {
                return Err(AppError::Invalid("invalid dataSchema range".into()));
            }
        }
        if let Some(remix) = &self.remix {
            if remix.allowed && remix.license.as_ref().is_none_or(|x| x.trim().is_empty()) {
                return Err(AppError::Invalid(
                    "remix permission requires a license".into(),
                ));
            }
        }
        if let Some(agent) = &self.agent {
            if !self.capabilities.iter().any(|s| s == "agent")
                || agent
                    .tools
                    .iter()
                    .any(|s| s.trim().is_empty() || s.len() > 256 || s.contains('\0'))
            {
                return Err(AppError::Invalid(
                    "agent requirements need agent capability and nonempty tools".into(),
                ));
            }
            let mut skills = BTreeSet::new();
            for skill in &agent.skills {
                if !token(&skill.id)
                    || !skills.insert(&skill.id)
                    || skill.path != format!("skills/{}/SKILL.md", skill.id)
                {
                    return Err(AppError::Invalid(
                        "Agent Skills require unique IDs and a packaged skills/<id>/SKILL.md path"
                            .into(),
                    ));
                }
                relative_path(&skill.path)?;
                if self
                    .package
                    .as_ref()
                    .is_some_and(|package| !package.files.contains(&skill.path))
                {
                    return Err(AppError::Invalid(
                        "declared Agent Skill instructions must be included in package.files"
                            .into(),
                    ));
                }
            }
        }
        for step in self.install.iter().chain(&self.build) {
            check_step(
                &step.command,
                &step.cwd,
                &step.env,
                &step.venv,
                &step.python,
            )?;
        }
        let names: BTreeSet<_> = self.processes.iter().map(|p| p.name.as_str()).collect();
        if names.len() != self.processes.len() {
            return Err(AppError::Invalid("duplicate process name".into()));
        }
        let environment_names: BTreeSet<_> = self
            .processes
            .iter()
            .map(|process| process.name.to_ascii_uppercase().replace(['-', '.'], "_"))
            .collect();
        if environment_names.len() != self.processes.len() {
            return Err(AppError::Invalid(
                "process names must have distinct service environment identifiers".into(),
            ));
        }
        for process in &self.processes {
            if !token(&process.name) {
                return Err(AppError::Invalid("invalid process name".into()));
            }
            check_step(
                &process.command,
                &process.cwd,
                &process.env,
                &process.venv,
                &process.python,
            )?;
            for dep in &process.depends_on {
                if !names.contains(dep.as_str()) || dep == &process.name {
                    return Err(AppError::Invalid(format!("invalid dependency {dep}")));
                }
            }
            if let Some(ready) = &process.ready {
                if ready.timeout_ms.is_some_and(|n| n == 0 || n > 600000)
                    || ready
                        .output
                        .as_ref()
                        .is_some_and(|v| v.is_empty() || v.len() > 4096)
                    || ready.path.as_ref().is_some_and(|p| {
                        !p.starts_with('/') || p.starts_with("//") || p.contains(['\r', '\n', '\0'])
                    })
                {
                    return Err(AppError::Invalid("invalid readiness".into()));
                }
            }
        }
        let mut resolved = BTreeSet::new();
        while resolved.len() < self.processes.len() {
            let before = resolved.len();
            for p in &self.processes {
                if p.depends_on.iter().all(|d| resolved.contains(d)) {
                    resolved.insert(p.name.clone());
                }
            }
            if before == resolved.len() {
                return Err(AppError::Invalid("process dependency cycle".into()));
            }
        }
        match &self.entry {
            Entry::Static { path } => relative_path(path)?,
            Entry::Server { process, path } => {
                if !names.contains(process.as_str()) {
                    return Err(AppError::Invalid("entry process does not exist".into()));
                }
                if path.as_ref().is_some_and(|p| {
                    !p.starts_with('/') || p.starts_with("//") || p.contains(['\r', '\n', '\0'])
                }) {
                    return Err(AppError::Invalid(
                        "server entry must be a local URL path".into(),
                    ));
                }
            }
        }
        if let Some(package) = &self.package {
            let mut seen = BTreeSet::new();
            for p in &package.files {
                relative_path(p)?;
                if !seen.insert(p) {
                    return Err(AppError::Invalid("duplicate package file".into()));
                }
            }
            for p in &package.dependency_locks {
                relative_path(p)?;
                if !seen.contains(p) {
                    return Err(AppError::Invalid(format!(
                        "dependency lock must be in package.files: {p}"
                    )));
                }
            }
        }
        Ok(())
    }
    /// Check this target against the host target.
    pub fn compatible_with(&self, host: &Target) -> Result<bool, AppError> {
        host.validate()?;
        if self.requires_execution() && !host.capabilities.iter().any(|c| c == "process") {
            return Ok(false);
        }
        Ok(self.targets.iter().any(|target| {
            target.matches(host)
                && self
                    .capabilities
                    .iter()
                    .all(|c| host.capabilities.contains(c))
        }))
    }
}
impl Target {
    /// Validate this value.
    pub fn validate(&self) -> Result<(), AppError> {
        let environments: &[&str] = match self.platform.as_str() {
            "ios" | "android" => &["web", "mobile-linux"],
            "macos" | "windows" | "linux" => &["web", "native"],
            "web" => &["web"],
            _ => {
                return Err(AppError::Invalid(format!(
                    "unknown platform {}",
                    self.platform
                )));
            }
        };
        if !environments.contains(&self.environment.as_str()) {
            return Err(AppError::Invalid(format!(
                "incompatible target environment {}",
                self.environment
            )));
        }
        if self
            .architecture
            .as_ref()
            .is_some_and(|a| !["aarch64", "x86_64", "armv7", "x86"].contains(&a.as_str()))
        {
            return Err(AppError::Invalid("unknown target architecture".into()));
        }
        if let Some(v) = &self.runtime_version {
            semver::Version::parse(v).map_err(|_| {
                AppError::Invalid("runtimeVersion must be a minimum semantic version".into())
            })?;
        }
        if self.runtime_abi.as_ref().is_some_and(|s| !token(s))
            || self.channels.iter().any(|s| !token(s))
        {
            return Err(AppError::Invalid("invalid runtime ABI or channel".into()));
        }
        capabilities(&self.capabilities)
    }
    fn matches(&self, host: &Self) -> bool {
        self.platform == host.platform
            && self.environment == host.environment
            && self
                .architecture
                .as_ref()
                .is_none_or(|a| host.architecture.as_ref() == Some(a))
            && self
                .runtime_abi
                .as_ref()
                .is_none_or(|a| host.runtime_abi.as_ref() == Some(a))
            && self.runtime_version.as_ref().is_none_or(|v| {
                host.runtime_version
                    .as_ref()
                    .and_then(|x| semver::Version::parse(x).ok())
                    .is_some_and(|h| semver::Version::parse(v).is_ok_and(|min| h >= min))
            })
            && (self.channels.is_empty() || host.channels.iter().any(|c| self.channels.contains(c)))
            && self
                .capabilities
                .iter()
                .all(|c| host.capabilities.contains(c))
    }
}
