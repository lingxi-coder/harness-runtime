//! `.mcpb` bundle install (Stage 3): unpack a zip plugin bundle with
//! path-traversal + too-many-files + zip-bomb guards, verify its content hash,
//! and normalize it into a loadable plugin directory.
//!
//! A `.mcpb` is a zip archive. The common bundle ships a plugin tree with
//! `.lingxi-plugin/plugin.json`; an MCP-style bundle roots a `manifest.json`
//! instead, which we translate into a minimal synthetic `plugin.json` so the
//! shared loader can read it.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Hard cap on archive entries (claude-code "Archive contains too many files").
const MAX_FILES: usize = 10_000;
/// Hard cap on total uncompressed bytes — zip-bomb guard.
const MAX_TOTAL_BYTES: u64 = 1 << 30; // 1 GiB

/// Group-write + other-write (`0o022`). claude-code's post-extraction sweep
/// tests `mode & 18` and, when set, re-chmods to `mode & 0o755` — oracle
/// `jko`/`O$` in `src_169588164.js`.
#[cfg(unix)]
const GROUP_OTHER_WRITE: u32 = 0o022;

/// Mode for the extraction directory itself: owner-only (`0o700`, oracle
/// `mode:448`), so another local user cannot read a plugin unpacked for this
/// session.
#[cfg(unix)]
pub const EXTRACT_DIR_MODE: u32 = 0o700;

/// Strip group/other write bits from `path`.
///
/// claude-code needs this because its unzip applies the archive's stored mode.
/// This port writes with [`std::fs::write`], which never consults the archive —
/// but that yields `0o666 & !umask`, so a permissive umask still produces a
/// world-writable plugin file. Same outcome, different cause; the sweep is what
/// makes the result independent of both.
#[cfg(unix)]
fn drop_group_other_write(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // `symlink_metadata` so a symlink planted in the archive cannot redirect the
    // chmod at a file outside `dest` (oracle opens with `O_NOFOLLOW` for the
    // same reason). Only regular files and directories are touched.
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mode = meta.permissions().mode();
    if mode & GROUP_OTHER_WRITE == 0 {
        return Ok(());
    }
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode(mode & !GROUP_OTHER_WRITE),
    )
}

#[cfg(not(unix))]
fn drop_group_other_write(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Create `dest` as the owner-only root of an extraction, removing whatever was
/// there before.
///
/// The removal is the 2.1.269 "stale files surviving re-extraction" half: an
/// unpack over an existing directory leaves any file the NEW archive does not
/// happen to overwrite, so a downgraded or tampered plugin keeps shipping code
/// from the previous version.
///
/// # Errors
/// Propagates the directory create/remove failure.
pub fn prepare_extract_dir(dest: &Path) -> std::io::Result<()> {
    // Clear whatever is there. A SYMLINK at `dest` is unlinked rather than
    // recursed into — `remove_dir_all` down a link would delete the link's
    // target tree, which is not ours to remove.
    match std::fs::symlink_metadata(dest) {
        Ok(meta) if meta.file_type().is_symlink() || meta.is_file() => std::fs::remove_file(dest)?,
        Ok(_) => std::fs::remove_dir_all(dest)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(dest)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(EXTRACT_DIR_MODE))?;
    }
    Ok(())
}

/// Lowercase hex SHA-256 of `bytes` (the bundle's only integrity check —
/// claude-code has no signature verification).
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Extract the zip `bytes` into `dest`, enforcing the entry-count, total-size,
/// and path-traversal guards. `dest` must already exist.
///
/// # Errors
/// Returns the byte-faithful failure detail on a malformed archive, traversal
/// attempt, too many files, or zip-bomb.
pub fn unpack_mcpb(bytes: &[u8], dest: &Path) -> Result<(), String> {
    unpack_mcpb_limited(bytes, dest, MAX_FILES, MAX_TOTAL_BYTES)
}

/// [`unpack_mcpb`] with explicit limits (so tests can exercise the guards
/// without building a multi-gigabyte archive).
fn unpack_mcpb_limited(
    bytes: &[u8],
    dest: &Path,
    max_files: usize,
    max_total: u64,
) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("Failed to extract MCPB {}: {e}", dest.display()))?;
    if zip.len() > max_files {
        // Binary: `Archive contains too many files: ${fileCount} (max: ${MAX_FILE_COUNT})`.
        return Err(format!(
            "Archive contains too many files: {} (max: {max_files})",
            zip.len()
        ));
    }
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("Failed to extract MCPB {}: {e}", dest.display()))?;
        // `enclosed_name` returns None for absolute paths / `..` traversal; the
        // explicit component + containment checks below are belt-and-suspenders.
        let rel = entry
            .enclosed_name()
            .filter(|p| {
                !p.components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            })
            .ok_or_else(|| format!("Path traversal attempt detected: {}", entry.name()))?;
        let out = dest.join(&rel);
        if !out.starts_with(dest) {
            return Err(format!("Path traversal attempt detected: {}", entry.name()));
        }
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            drop_group_other_write(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Zip-bomb guard on the ACTUAL decompressed bytes: `entry.size()` is
            // the attacker-controlled central-directory claim and can lie (a 10-
            // byte claim can deflate-expand to gigabytes), so DO NOT trust it.
            // Read through a `take()` bounded at the remaining byte budget so
            // decompression aborts mid-stream instead of materializing the whole
            // payload, and count the real bytes produced.
            let remaining = max_total.saturating_sub(total);
            // Pre-allocate at most the smaller of the claim and the budget so a
            // lying large claim cannot force a huge up-front allocation either.
            let cap = usize::try_from(entry.size().min(remaining)).unwrap_or(0);
            let mut buf = Vec::with_capacity(cap);
            // `remaining + 1` so a payload exactly at the cap reads one extra
            // byte and trips the check below (never silently truncates).
            entry
                .by_ref()
                .take(remaining + 1)
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            if buf.len() as u64 > remaining {
                return Err(format!(
                    "Archive total size is too large: more than {max_total} bytes. This may be a zip bomb."
                ));
            }
            total += buf.len() as u64;
            std::fs::write(&out, &buf).map_err(|e| e.to_string())?;
            drop_group_other_write(&out).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Ensure the extracted bundle at `dir` has a `.lingxi-plugin/plugin.json` the
/// shared loader can read. If it is missing but a root `manifest.json` (MCPB
/// schema) is present, translate the `name`/`version` into a minimal synthetic
/// `plugin.json`.
///
/// # Errors
/// Returns a byte-faithful detail when neither manifest is present/valid.
pub fn ensure_plugin_manifest(dir: &Path) -> Result<(), String> {
    let plugin_json = dir.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json");
    if plugin_json.exists() {
        return Ok(());
    }
    let manifest_path = dir.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|_| format!("MCPB manifest invalid at {}", dir.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("Invalid JSON in manifest.json: {e}"))?;
    let name = json
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Manifest validation failed: missing name".to_string())?;
    let version = json
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("0.0.0");
    std::fs::create_dir_all(dir.join(branding::PLUGIN_MANIFEST_DIR)).map_err(|e| e.to_string())?;
    std::fs::write(
        &plugin_json,
        serde_json::json!({ "name": name, "version": version }).to_string(),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------- §14 row 1: the `mcpServers` MCPB/`.dxt` union arm ----------
//
// A `mcpServers` entry may itself be a relative path or URL ending in
// `.mcpb`/`.dxt` (oracle `st`), naming an MCP-style bundle whose root
// `manifest.json` (a DIFFERENT, richer schema than the plugin-bundle
// `manifest.json` [`ensure_plugin_manifest`] translates — this one is the
// public MCPB/DXT extension manifest, oracle `PMt`/`McpbManifestSchema`)
// describes exactly one MCP server. Byte-source: `getMcpConfigForManifest`
// (oracle `x`, re-exported from the extension-authoring module) plus its
// template-substitution helper `m`, both recovered verbatim from the
// 2.1.251 Mach-O.
//
// What this module implements: parsing a (lenient — see [`McpbManifest`])
// subset of that manifest, applying the `darwin` `platform_overrides` merge
// (on every host, per the oracle — see [`generate_mcp_config`]), the
// `hasRequiredConfigMissing` gate, and the `${__dirname}` /
// `${pathSeparator}` / `${/}` / `${HOME}` / `${DESKTOP}` / `${DOCUMENTS}` /
// `${DOWNLOADS}` / `${user_config.KEY}` template substitution —
// [`generate_mcp_config`] is a direct, byte-faithful port of oracle `x`+`m`,
// and [`system_dirs_in`] of the `systemDirs` its one call site feeds it.
//
// What is NOT modelled: a persisted per-MCPB `user_config` store (oracle
// reads one via `dye(repository, manifest.name, …)`; this crate's discovery
// path has no such store wired in), so [`generate_mcp_config`] always
// evaluates the "missing required config" gate against an EMPTY provided
// config — faithful to the oracle's own semantics (`hasRequiredConfigMissing`
// reads only the *provided* value, never the declared `default`) for a
// plugin that has never been interactively configured, and the common case
// (no `user_config`, or only optional/defaulted fields) still resolves in
// full. A bundle with a `required` field is conservatively skipped rather
// than guessed at.

/// A subset of the public MCPB/DXT extension `manifest.json` schema (oracle
/// `PMt`/`McpbManifestSchema`) — only the fields [`generate_mcp_config`]
/// needs. Deliberately more lenient than the oracle schema (which requires
/// `version`/`description`/`author`/`server` and one of `dxt_version`/
/// `manifest_version`): the oracle's own runtime caller re-checks `!pe.server`
/// AFTER its parse succeeds (`uct`, 2.1.251 Mach-O @159489593), meaning the
/// schema's `server` requirement is not actually enforced on this path either
/// — the only field this port treats as load-bearing is `name` (used as the
/// resulting server's map key and in every diagnostic).
#[derive(Debug, Clone, Deserialize)]
pub struct McpbManifest {
    /// The extension's own name — becomes the generated MCP server's map key
    /// (oracle `Pit`: `{[C.manifest.name]: C.mcpConfig}`).
    pub name: String,
    /// The server declaration. Oracle-required but read leniently here (see
    /// above) — absence produces the oracle's own diagnostic rather than a
    /// parse failure.
    #[serde(default)]
    pub server: Option<McpbServer>,
    /// User-configurable fields this extension declares (oracle `fmr`,
    /// keyed by field name). Only `required` and `default` are read.
    #[serde(rename = "user_config", default)]
    pub user_config: HashMap<String, McpbUserConfigOption>,
}

/// `manifest.server` (oracle `cmr`/`McpbManifestServerSchema`): only
/// `mcp_config` is read here — `type`/`entry_point` steer the manifest
/// AUTHORING tools (`bmr`/`Tmr`/scaffold defaults), not config generation.
#[derive(Debug, Clone, Deserialize)]
pub struct McpbServer {
    /// The (possibly `platform_overrides`-carrying) MCP server config,
    /// kept as a raw JSON tree so [`generate_mcp_config`]'s template
    /// substitution can walk it generically exactly like oracle `m()`
    /// does (the schema constrains it to `{command, args, env,
    /// platform_overrides}`, but nothing here depends on that shape beyond
    /// `platform_overrides` itself).
    #[serde(rename = "mcp_config")]
    pub mcp_config: serde_json::Value,
}

/// One `manifest.user_config` entry (oracle `fmr`) — only the two fields
/// [`generate_mcp_config`]'s gate and template-substitution steps read.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpbUserConfigOption {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

/// Oracle `d()`/`hasRequiredConfigMissing`: true when any `required`
/// `user_config` field has no PROVIDED value. Oracle reads the value from the
/// caller-supplied/stored config only — a declared `default` is never
/// consulted here (it is applied later, only as a template-substitution
/// value) — so with this port's always-empty provided config, this is
/// equivalent to "does the manifest declare any required field at all".
fn has_required_config_missing(manifest: &McpbManifest) -> bool {
    manifest.user_config.values().any(|field| field.required)
}

/// One template-substitution value (oracle `p`/`g` map entries): either a
/// scalar (already `String`-coerced, matching JS `String(f)` /
/// `f?"true":"false"`) or an array (`f.map(String)`), which only ever
/// participates in the array-splice special case of [`substitute`].
#[derive(Debug, Clone)]
enum TemplateVar {
    Scalar(String),
    Array(Vec<String>),
}

/// JS `String(value)` coercion for a JSON scalar (oracle's `String(f)` /
/// `f?"true":"false"` branch of the `user_config.*` seeding loop).
fn json_scalar_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => (if *b { "true" } else { "false" }).to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

impl TemplateVar {
    fn from_user_config_value(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Array(items) => {
                Self::Array(items.iter().map(json_scalar_to_string).collect())
            }
            other => Self::Scalar(json_scalar_to_string(other)),
        }
    }
}

/// A JSON string is exactly one `${user_config.KEY}` token (oracle's
/// `/^\$\{user_config\.[^}]+\}$/` whole-string test) — returns the inner
/// `user_config.KEY` content (oracle's SECOND regex capture,
/// `/^\$\{([^}]+)\}$/`), which doubles as the [`TemplateVar`] map key.
fn as_whole_user_config_token(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("${")?.strip_suffix('}')?;
    (inner.starts_with("user_config.") && inner.len() > "user_config.".len()).then_some(inner)
}

/// Oracle `m(e,i)`: recursively substitute `${VAR}` tokens through a JSON
/// tree. A plain string gets each KNOWN var's token replaced in place (an
/// array-valued var is skipped with a warning, matching oracle's
/// `console.warn` — a `${VAR}` referring to an array value inside a larger
/// string has no defined flattening). An array element whose ENTIRE content
/// is one `${user_config.KEY}` token is spliced instead: an array-valued var
/// flattens its items into place; a scalar-valued var contributes one item;
/// an unrecognized key is kept as the literal token (none of these three
/// go through the generic string-substitution path). Every other array
/// element, and every object value, recurses normally. Numbers/bools/
/// null pass through unchanged.
fn substitute(value: &serde_json::Value, vars: &HashMap<String, TemplateVar>) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(substitute_string(s, vars)),
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                if let serde_json::Value::String(s) = item {
                    if let Some(key) = as_whole_user_config_token(s) {
                        match vars.get(key) {
                            Some(TemplateVar::Array(values)) => {
                                out.extend(values.iter().cloned().map(serde_json::Value::String))
                            }
                            Some(TemplateVar::Scalar(v)) => {
                                out.push(serde_json::Value::String(v.clone()));
                            }
                            None => out.push(item.clone()),
                        }
                        continue;
                    }
                }
                out.push(substitute(item, vars));
            }
            serde_json::Value::Array(out)
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), substitute(v, vars));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Replace every occurrence of each KNOWN var's `${name}` token in `s`
/// (oracle's per-string loop over `Object.entries(i)`, `String.replace`
/// with the JS `g`/global flag — `str::replace` already replaces every
/// match of a literal needle, so no regex is needed for this direction).
/// An array-valued var is left untouched wherever its token appears in a
/// string context (oracle: array values are not string-replaceable).
fn substitute_string(s: &str, vars: &HashMap<String, TemplateVar>) -> String {
    let mut out = s.to_string();
    for (key, val) in vars {
        let token = format!("${{{key}}}");
        if let TemplateVar::Scalar(v) = val {
            if out.contains(&token) {
                out = out.replace(&token, v);
            }
        }
    }
    out
}

/// JS truthiness for a `platform_overrides.darwin` field (oracle `c.x||s.x`):
/// `undefined`, `null`, `false`, `0` and `""` all fall back to the base
/// value. An empty array/object is truthy in JS, so both are kept.
fn is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// The platform arm oracle `JHe`'s `switch(t)` selects (@159481081). `wsl`
/// shares the `linux` arm, and `unknown` shares the `macos`/default arm, so
/// three variants cover every branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemDirPlatform {
    Windows,
    Linux,
    MacOs,
}

/// Oracle `JHe(e)` (@159481081) with its inputs made explicit:
/// `{HOME, DESKTOP, DOCUMENTS, DOWNLOADS}`, in that order.
///
/// - `windows`: the three subdirectories hang off `USERPROFILE || homedir`,
///   while `HOME` stays the home dir itself.
/// - `linux`/`wsl`: `XDG_DESKTOP_DIR` / `XDG_DOCUMENTS_DIR` /
///   `XDG_DOWNLOAD_DIR` override the defaults when set.
/// - `macos`/default: plain `join(homedir, …)`.
fn system_dirs_in(
    platform: SystemDirPlatform,
    home: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<(&'static str, String)> {
    let show = |p: &Path| p.to_string_lossy().into_owned();
    let under = |base: &Path, leaf: &str| show(&base.join(leaf));
    let home_s = show(home);
    match platform {
        SystemDirPlatform::Windows => {
            let base = env("USERPROFILE").map_or_else(|| home.to_path_buf(), PathBuf::from);
            vec![
                ("HOME", home_s),
                ("DESKTOP", under(&base, "Desktop")),
                ("DOCUMENTS", under(&base, "Documents")),
                ("DOWNLOADS", under(&base, "Downloads")),
            ]
        }
        SystemDirPlatform::Linux => vec![
            ("HOME", home_s),
            (
                "DESKTOP",
                env("XDG_DESKTOP_DIR").unwrap_or_else(|| under(home, "Desktop")),
            ),
            (
                "DOCUMENTS",
                env("XDG_DOCUMENTS_DIR").unwrap_or_else(|| under(home, "Documents")),
            ),
            (
                "DOWNLOADS",
                env("XDG_DOWNLOAD_DIR").unwrap_or_else(|| under(home, "Downloads")),
            ),
        ],
        SystemDirPlatform::MacOs => vec![
            ("HOME", home_s),
            ("DESKTOP", under(home, "Desktop")),
            ("DOCUMENTS", under(home, "Documents")),
            ("DOWNLOADS", under(home, "Downloads")),
        ],
    }
}

/// `os.homedir()`: `USERPROFILE` on Windows, `$HOME` elsewhere (this repo's
/// established convention — `migrations::global_config` reads `HOME` the same
/// way).
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// [`system_dirs_in`] bound to the running host. Yields nothing when the home
/// directory cannot be resolved at all (the oracle's `os.homedir()` always
/// returns one; here an unresolvable home simply leaves the tokens alone
/// rather than substituting a bogus root).
fn system_dirs() -> Vec<(&'static str, String)> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let platform = if cfg!(windows) {
        SystemDirPlatform::Windows
    } else if cfg!(target_os = "macos") {
        SystemDirPlatform::MacOs
    } else {
        SystemDirPlatform::Linux
    };
    system_dirs_in(platform, &home, &|k| std::env::var(k).ok())
}

/// Oracle `x()`/`getMcpConfigForManifest`: generate the MCP server config an
/// MCPB `manifest.server.mcp_config` resolves to once extracted at
/// `extension_path`. Returns `None` when the manifest has no `server` (or no
/// `mcp_config`) at all, or when a `required` `user_config` field has no
/// provided value (see module docs for why the provided config is always
/// empty in this port) — both cases the caller should treat as "this MCPB
/// source contributes no server", matching the oracle's own skip-with-warning
/// behaviour rather than an error.
#[must_use]
pub fn generate_mcp_config(
    manifest: &McpbManifest,
    extension_path: &Path,
) -> Option<serde_json::Value> {
    let server = manifest.server.as_ref()?;
    let mut config = server.mcp_config.clone();

    // Oracle @171172343, verbatim: `if(a.platform_overrides){if("darwin"in
    // a.platform_overrides){let c=a.platform_overrides.darwin;
    // s.command=c.command||s.command,s.args=c.args||s.args,
    // s.env=c.env||s.env}}`. Two things this is NOT: it is not gated on the
    // host platform (there is no `process.platform` test anywhere in `x`, so
    // the `darwin` block is merged on Linux and Windows too — `win32`/`linux`
    // keys are simply never read at load time), and it is not an
    // unconditional insert — `||` is a JS falsy fallback, so an override of
    // `""`, `null` or `false` keeps the base value.
    if let Some(darwin) = config
        .get("platform_overrides")
        .and_then(|overrides| overrides.get("darwin"))
        .cloned()
    {
        if let Some(obj) = config.as_object_mut() {
            for key in ["command", "args", "env"] {
                if let Some(v) = darwin.get(key).filter(|v| is_truthy(v)) {
                    obj.insert(key.to_string(), v.clone());
                }
            }
        }
    }

    if has_required_config_missing(manifest) {
        return None;
    }

    let mut vars: HashMap<String, TemplateVar> = HashMap::new();
    vars.insert(
        "__dirname".to_string(),
        TemplateVar::Scalar(extension_path.to_string_lossy().into_owned()),
    );
    // Oracle hardcodes `pathSeparator:"/"` at the ONE call site
    // (`uct`→`MY`) regardless of host OS — MCPB bundles are authored
    // portably and always use `/` here, never `std::path::MAIN_SEPARATOR`.
    vars.insert(
        "pathSeparator".to_string(),
        TemplateVar::Scalar("/".to_string()),
    );
    vars.insert("/".to_string(), TemplateVar::Scalar("/".to_string()));
    // Oracle `p={__dirname:r,pathSeparator:t,"/":t,...n}` where `n` is the
    // `systemDirs` argument, and the one call site (`MY` @159485465) always
    // passes `JHe()`. Without these, `${DOCUMENTS}`/`${DESKTOP}`/
    // `${DOWNLOADS}` reach the spawned server as literal tokens — the
    // downstream `${VAR}` process-env expansion cannot rescue them because
    // they are never environment variables.
    for (key, value) in system_dirs() {
        vars.insert(key.to_string(), TemplateVar::Scalar(value));
    }
    for (key, field) in &manifest.user_config {
        if let Some(default) = &field.default {
            vars.insert(
                format!("user_config.{key}"),
                TemplateVar::from_user_config_value(default),
            );
        }
    }

    Some(substitute(&config, &vars))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("lingxi-mcpb-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for (name, body) in entries {
                w.start_file::<_, ()>(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        buf
    }

    /// 2.1.269 — "plugin archives extracted for a session being readable by
    /// other local users". A permissive umask alone is enough to produce a
    /// group/other-writable plugin tree here, because `fs::write` uses
    /// `0o666 & !umask`.
    #[cfg(unix)]
    #[test]
    fn extraction_root_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dest = scratch("perms-dir");
        prepare_extract_dir(&dest).unwrap();
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "extraction root must be owner-only, got {mode:o}"
        );
    }

    /// `fs::write` PRESERVES an existing file's mode, so unpacking over a
    /// world-writable path of the same name leaves it world-writable unless the
    /// per-entry sweep runs. (A permissive umask reaches the same state on a
    /// fresh file; this shape is testable without touching the process umask.)
    #[cfg(unix)]
    #[test]
    fn unpacking_strips_group_and_other_write_from_extracted_files() {
        use std::os::unix::fs::PermissionsExt;
        let dest = scratch("perms-file");
        std::fs::create_dir_all(&dest).unwrap();
        let victim = dest.join("b.txt");
        std::fs::write(&victim, b"old").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o022,
            0o022,
            "precondition: the path starts group/other-writable"
        );

        unpack_mcpb(&zip_with(&[("b.txt", b"hi")]), &dest).unwrap();

        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o022,
            0,
            "group/other write must be stripped, got {mode:o}"
        );
    }

    /// 2.1.269 — "stale files surviving re-extraction". A file the NEW archive
    /// does not ship must not outlive the old extraction.
    #[test]
    fn re_extraction_does_not_leave_files_the_new_archive_dropped() {
        let dest = scratch("stale");
        prepare_extract_dir(&dest).unwrap();
        unpack_mcpb(&zip_with(&[("old.js", b"stale code")]), &dest).unwrap();
        assert!(dest.join("old.js").exists());

        // Re-extract a DIFFERENT archive that no longer ships `old.js`.
        prepare_extract_dir(&dest).unwrap();
        unpack_mcpb(&zip_with(&[("new.js", b"fresh")]), &dest).unwrap();

        assert!(dest.join("new.js").exists(), "the new file is written");
        assert!(
            !dest.join("old.js").exists(),
            "a file the new archive dropped must not survive re-extraction"
        );
    }

    /// `prepare_extract_dir` must UNLINK a symlink at `dest`, never recurse into
    /// it — `remove_dir_all` through a link would delete the link's target tree.
    #[cfg(unix)]
    #[test]
    fn preparing_a_symlinked_dest_does_not_delete_the_link_target() {
        let base = scratch("symlink-dest");
        std::fs::create_dir_all(&base).unwrap();
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("keep.txt"), b"KEEP").unwrap();
        let dest = base.join("link");
        std::os::unix::fs::symlink(&real, &dest).unwrap();

        prepare_extract_dir(&dest).unwrap();

        assert!(
            real.join("keep.txt").exists(),
            "the symlink target tree must be untouched"
        );
        assert!(
            !std::fs::symlink_metadata(&dest)
                .unwrap()
                .file_type()
                .is_symlink(),
            "dest is now a real directory"
        );
    }

    #[test]
    fn sha256_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// The total-size guard counts ACTUAL decompressed bytes (via a take-bounded
    /// read), so it triggers regardless of what the entry header claims — the
    /// zip-bomb-via-lying-header escape the adversarial verify found is closed.
    #[test]
    fn total_size_guard_counts_real_bytes_not_the_header_claim() {
        use std::io::Write;
        // An HONEST 1000-byte entry; the bounded read counts the real bytes, so a
        // tiny 100-byte cap must reject it (the header claim is irrelevant).
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            w.start_file("big.bin", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(&vec![0u8; 1000]).unwrap();
            w.finish().unwrap();
        }
        let tmp = tempfile::tempdir().unwrap();
        let err = unpack_mcpb_limited(&buf, tmp.path(), 10_000, 100).unwrap_err();
        assert!(
            err.contains("Archive total size is too large"),
            "got: {err}"
        );
    }

    /// Migrated from `plugin/tests/materialize.rs`'s
    /// `install_mcpb_arm_rejects_path_traversal` (spec §25d): that test drove
    /// this guard through `PluginManager::install`'s now-deleted `.mcpb`
    /// install arm. `unpack_mcpb` itself is still very much live — it is the
    /// same function `discovery.rs`'s `mcpServers` `.mcpb`/`.dxt` loading
    /// path calls — so the guard is exercised directly against its real,
    /// still-used entry point instead.
    #[test]
    fn unpack_rejects_path_traversal_entries() {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            w.start_file("../../escape.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(b"pwned").unwrap();
            w.finish().unwrap();
        }
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let err = unpack_mcpb(&buf, &dest).unwrap_err();
        assert!(
            err.contains("Path traversal attempt detected"),
            "got: {err}"
        );
        assert!(
            !tmp.path().join("escape.txt").exists(),
            "no file escaped the extract dir"
        );
    }

    #[test]
    fn too_many_files_guard_trips() {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for i in 0..5 {
                w.start_file(
                    format!("f{i}.txt"),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
                w.write_all(b"x").unwrap();
            }
            w.finish().unwrap();
        }
        let tmp = tempfile::tempdir().unwrap();
        let err = unpack_mcpb_limited(&buf, tmp.path(), 2, 1 << 30).unwrap_err();
        assert!(
            err.contains("Archive contains too many files"),
            "got: {err}"
        );
    }

    fn manifest_from(json: serde_json::Value) -> McpbManifest {
        serde_json::from_value(json).unwrap()
    }

    /// `${__dirname}` and the hardcoded `${pathSeparator}`/`${/}` tokens are
    /// substituted into `command`/`args`/`env` (oracle `x`+`m`).
    #[test]
    fn dirname_and_path_separator_tokens_are_substituted() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {
                "mcp_config": {
                    "command": "node",
                    "args": ["${__dirname}/server/index.js"],
                    "env": {"SEP": "${pathSeparator}", "SLASH": "${/}"}
                }
            }
        }));
        let generated =
            generate_mcp_config(&manifest, Path::new("/plugins/demo/.mcpb-cache/abc")).unwrap();
        assert_eq!(
            generated["args"][0],
            "/plugins/demo/.mcpb-cache/abc/server/index.js"
        );
        assert_eq!(generated["env"]["SEP"], "/");
        assert_eq!(generated["env"]["SLASH"], "/");
    }

    /// A `required` `user_config` field with no provided value (this port
    /// never has a persisted store to provide one) skips config generation
    /// entirely — even when the field also declares a `default` (oracle
    /// `hasRequiredConfigMissing` reads only the provided value, never the
    /// default).
    #[test]
    fn required_user_config_with_no_provided_value_skips_generation() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {"mcp_config": {"command": "node", "args": []}},
            "user_config": {"apiKey": {"required": true, "default": "unused"}}
        }));
        assert!(
            generate_mcp_config(&manifest, Path::new("/x")).is_none(),
            "a required field with no provided value must skip generation, default or not"
        );
    }

    /// An OPTIONAL `user_config` field's `default` still seeds the
    /// `${user_config.KEY}` substitution map, so a manifest with only
    /// optional/defaulted fields resolves in full without a store.
    #[test]
    fn optional_user_config_default_seeds_template_substitution() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {
                "mcp_config": {
                    "command": "node",
                    "args": ["--root=${user_config.workspace}"]
                }
            },
            "user_config": {"workspace": {"required": false, "default": "/tmp/ws"}}
        }));
        let generated = generate_mcp_config(&manifest, Path::new("/x")).unwrap();
        assert_eq!(generated["args"][0], "--root=/tmp/ws");
    }

    /// An array element that is EXACTLY one `${user_config.KEY}` token
    /// splices an array-valued default in place (oracle `m`'s array-context
    /// special case), rather than stringifying the whole array into that one
    /// slot.
    #[test]
    fn whole_token_array_element_splices_an_array_valued_default() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {
                "mcp_config": {
                    "command": "node",
                    "args": ["serve", "${user_config.extraFlags}", "--done"]
                }
            },
            "user_config": {"extraFlags": {"default": ["--verbose", "--json"]}}
        }));
        let generated = generate_mcp_config(&manifest, Path::new("/x")).unwrap();
        assert_eq!(
            generated["args"],
            serde_json::json!(["serve", "--verbose", "--json", "--done"])
        );
    }

    /// A manifest with no `server` at all (the oracle's own defensive
    /// `if(!pe.server)` runtime guard) yields `None`, not a parse error.
    #[test]
    fn manifest_with_no_server_yields_none() {
        let manifest = manifest_from(serde_json::json!({"name": "demo"}));
        assert!(generate_mcp_config(&manifest, Path::new("/x")).is_none());
    }

    /// Oracle `getMcpConfigForManifest` @171172343, verbatim:
    /// `if(a.platform_overrides){if("darwin"in a.platform_overrides){let
    /// c=a.platform_overrides.darwin;s.command=c.command||s.command,
    /// s.args=c.args||s.args,s.env=c.env||s.env}}`. There is no
    /// `process.platform` test anywhere in the function — the merge runs on
    /// EVERY platform, gated only on the `darwin` key being present.
    #[test]
    fn darwin_platform_override_applies_on_every_platform() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {
                "mcp_config": {
                    "command": "node",
                    "args": ["server/index.js"],
                    "platform_overrides": {
                        "darwin": {"command": "node-darwin", "args": ["mac-server.js"]}
                    }
                }
            }
        }));
        let generated = generate_mcp_config(&manifest, Path::new("/x")).unwrap();
        assert_eq!(generated["command"], "node-darwin");
        assert_eq!(generated["args"], serde_json::json!(["mac-server.js"]));
    }

    /// `c.command||s.command` is a JS falsy fallback: an empty string, `null`
    /// or `false` in the override keeps the BASE value rather than clobbering
    /// it. An absent key falls back for the same reason.
    #[test]
    fn falsy_darwin_platform_override_values_fall_back_to_the_base() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {
                "mcp_config": {
                    "command": "node",
                    "args": ["server/index.js"],
                    "env": {"A": "1"},
                    "platform_overrides": {
                        "darwin": {"command": "", "args": null}
                    }
                }
            }
        }));
        let generated = generate_mcp_config(&manifest, Path::new("/x")).unwrap();
        assert_eq!(
            generated["command"], "node",
            "an empty-string override is falsy and must keep the base command"
        );
        assert_eq!(
            generated["args"],
            serde_json::json!(["server/index.js"]),
            "a null override is falsy and must keep the base args"
        );
        assert_eq!(generated["env"], serde_json::json!({"A": "1"}));
    }

    /// Oracle `JHe` @159481081: `{HOME, DESKTOP, DOCUMENTS, DOWNLOADS}` with
    /// a per-platform base — windows resolves the three subdirectories under
    /// `USERPROFILE` (falling back to the home dir) while `HOME` stays the
    /// home dir itself; linux/wsl honour `XDG_*`; macos/default joins the
    /// home dir.
    #[test]
    fn system_dirs_match_the_oracle_per_platform() {
        let home = Path::new("/home/me");
        let none = |_: &str| None;

        let mac = system_dirs_in(SystemDirPlatform::MacOs, home, &none);
        assert_eq!(
            mac,
            vec![
                ("HOME", "/home/me".to_string()),
                ("DESKTOP", "/home/me/Desktop".to_string()),
                ("DOCUMENTS", "/home/me/Documents".to_string()),
                ("DOWNLOADS", "/home/me/Downloads".to_string()),
            ]
        );

        let xdg = |k: &str| match k {
            "XDG_DOCUMENTS_DIR" => Some("/data/docs".to_string()),
            _ => None,
        };
        let linux = system_dirs_in(SystemDirPlatform::Linux, home, &xdg);
        assert_eq!(linux[2], ("DOCUMENTS", "/data/docs".to_string()));
        assert_eq!(linux[1], ("DESKTOP", "/home/me/Desktop".to_string()));

        let profile = |k: &str| match k {
            "USERPROFILE" => Some("/users/other".to_string()),
            _ => None,
        };
        let win = system_dirs_in(SystemDirPlatform::Windows, home, &profile);
        assert_eq!(
            win[0],
            ("HOME", "/home/me".to_string()),
            "windows keeps HOME as the home dir, not USERPROFILE"
        );
        assert_eq!(win[3], ("DOWNLOADS", "/users/other/Downloads".to_string()));
    }

    /// Oracle `x()` builds `p={__dirname:r,pathSeparator:t,"/":t,...n}` where
    /// `n` IS `systemDirs` (`JHe()` at the one call site, `MY` @159485465).
    /// Without them `${DOCUMENTS}` survives verbatim into the spawned
    /// server's argv — `expand_env_vars_in_string` cannot rescue it because
    /// DOCUMENTS/DESKTOP/DOWNLOADS are never process env vars.
    #[test]
    fn system_dir_tokens_are_substituted_into_the_generated_config() {
        let manifest = manifest_from(serde_json::json!({
            "name": "demo",
            "server": {
                "mcp_config": {
                    "command": "node",
                    "args": ["--vault", "${DOCUMENTS}/vault", "${DESKTOP}", "${DOWNLOADS}"],
                    "env": {"H": "${HOME}"}
                }
            }
        }));
        let generated = generate_mcp_config(&manifest, Path::new("/x")).unwrap();
        let rendered = generated.to_string();
        assert!(
            !rendered.contains("${DOCUMENTS}")
                && !rendered.contains("${DESKTOP}")
                && !rendered.contains("${DOWNLOADS}")
                && !rendered.contains("${HOME}"),
            "systemDirs tokens must be substituted, got {rendered}"
        );
        let args = generated["args"].as_array().unwrap();
        assert!(
            args[1].as_str().unwrap().ends_with("/Documents/vault"),
            "got {:?}",
            args[1]
        );
        assert!(
            args[2].as_str().unwrap().ends_with("/Desktop"),
            "got {:?}",
            args[2]
        );
    }
}
