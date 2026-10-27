//! `.mcpb` bundle install (Stage 3): unpack a zip plugin bundle with
//! path-traversal + too-many-files + zip-bomb guards, verify its content hash,
//! and normalize MCP-style bundles into a loadable plugin directory.
//!
//! A `.mcpb` is a zip archive. An upstream or LingXi plugin bundle keeps its
//! existing plugin metadata path; an MCP-style bundle roots a `manifest.json`
//! instead, which we translate into a minimal LingXi `plugin.json`.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Hard cap on archive entries (claude-code "Archive contains too many files").
const MAX_FILES: usize = 10_000;
/// Hard cap on total uncompressed bytes — zip-bomb guard.
const MAX_TOTAL_BYTES: u64 = 1 << 30; // 1 GiB
/// Native 2.1.295 V2 plugin-cache ZIP decoder limits (`mVe`/`ut` in
/// `chunk-x9cpkaxf.js`). Keep these separate from the MCPB/network policy
/// above: V2 local cache archives admit a larger entry count but also enforce
/// per-file and aggregate compression-ratio limits.
const NATIVE_PLUGIN_CACHE_MAX_FILES: usize = 100_000;
const NATIVE_PLUGIN_CACHE_MAX_FILE_BYTES: u64 = 512 << 20; // 512 MiB
const NATIVE_PLUGIN_CACHE_MAX_TOTAL_BYTES: u64 = 1 << 30; // 1 GiB
const NATIVE_PLUGIN_CACHE_MAX_RATIO: u64 = 50;

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

/// Extract a Native 2.1.295 V2 plugin-cache ZIP. This intentionally has a
/// separate policy from `.mcpb`/network archives: the native cache decoder
/// allows up to 100,000 entries, caps one entry at 512 MiB and the archive at
/// 1 GiB expanded, and rejects a total-expanded/archive-size ratio over 50:1.
/// The common guarded writer still enforces path containment and counts actual
/// decompressed bytes, so malformed size claims cannot escape the destination.
pub fn unpack_plugin_cache_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    unpack_zip_with_limits(
        bytes,
        dest,
        ZipLimits {
            max_files: NATIVE_PLUGIN_CACHE_MAX_FILES,
            max_file_bytes: NATIVE_PLUGIN_CACHE_MAX_FILE_BYTES,
            max_total_bytes: NATIVE_PLUGIN_CACHE_MAX_TOTAL_BYTES,
            max_ratio: Some(NATIVE_PLUGIN_CACHE_MAX_RATIO),
            native_methods_only: true,
        },
    )
}

/// [`unpack_mcpb`] with explicit limits (so tests can exercise the guards
/// without building a multi-gigabyte archive).
fn unpack_mcpb_limited(
    bytes: &[u8],
    dest: &Path,
    max_files: usize,
    max_total: u64,
) -> Result<(), String> {
    unpack_zip_with_limits(
        bytes,
        dest,
        ZipLimits {
            max_files,
            max_file_bytes: u64::MAX,
            max_total_bytes: max_total,
            max_ratio: None,
            native_methods_only: false,
        },
    )
}

#[derive(Clone, Copy)]
struct ZipLimits {
    max_files: usize,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_ratio: Option<u64>,
    native_methods_only: bool,
}

#[derive(Debug)]
struct NativeZipEntryMetadata {
    /// Ht's path name: UTF-8 flag means non-fatal UTF-8 with TextDecoder's
    /// leading-BOM suppression; otherwise each byte becomes one Latin-1 code
    /// point (`Ot`), not CP437.
    path_name: String,
    /// x4t indexes its mode object using Node Buffer.toString("utf8") on the
    /// raw central-directory name, independently of Ht's filename decoding.
    mode_key: String,
    unix_mode: Option<u32>,
}

fn decode_native_zip_path_name(raw_name: &[u8], flags: u16) -> String {
    if flags & (1 << 11) != 0 {
        let decoded = String::from_utf8_lossy(raw_name);
        decoded
            .strip_prefix('\u{feff}')
            .unwrap_or(decoded.as_ref())
            .to_owned()
    } else {
        raw_name.iter().map(|byte| char::from(*byte)).collect()
    }
}

/// Read the raw central-directory names, flags, and Unix creator/mode fields
/// used by Native's Ht/x4t pair. `ZipFile::name()` cannot provide these exact
/// semantics: it applies its own filename decoding, whereas Ht uses the raw
/// bytes and UTF-8 flag only.
fn read_native_zip_entry_metadata(
    bytes: &[u8],
    expected_entries: usize,
) -> Result<Vec<NativeZipEntryMetadata>, String> {
    const EOCD: u32 = 0x0605_4b50;
    const ZIP64_LOCATOR: u32 = 0x0706_4b50;
    const ZIP64_EOCD: u32 = 0x0606_4b50;
    const CENTRAL_HEADER: u32 = 0x0201_4b50;

    fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
        Some(u16::from_le_bytes(
            bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
        ))
    }
    fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
        ))
    }

    let mut end = bytes
        .len()
        .checked_sub(22)
        .ok_or_else(|| "Invalid ZIP end-of-central-directory".to_string())?;
    let min_end = bytes.len().saturating_sub(22 + 65_535);
    loop {
        if u32_at(bytes, end) == Some(EOCD) {
            break;
        }
        if end <= min_end {
            return Err("Invalid ZIP end-of-central-directory".to_string());
        }
        end -= 1;
    }

    // Native uses the entries-on-this-disk count and central offset, then
    // follows the adjacent ZIP64 locator for sentinel values.
    let mut count =
        usize::from(u16_at(bytes, end + 8).ok_or_else(|| "Invalid ZIP entry count".to_string())?);
    let mut central = usize::try_from(
        u32_at(bytes, end + 16)
            .ok_or_else(|| "Invalid ZIP central-directory offset".to_string())?,
    )
    .map_err(|_| "ZIP central-directory offset is not representable".to_string())?;
    if count == usize::from(u16::MAX) || central == u32::MAX as usize {
        let locator = end
            .checked_sub(20)
            .ok_or_else(|| "Invalid ZIP64 locator".to_string())?;
        if u32_at(bytes, locator) != Some(ZIP64_LOCATOR) {
            return Err("Invalid ZIP64 locator".to_string());
        }
        let zip64_end = usize::try_from(
            u32_at(bytes, locator + 8).ok_or_else(|| "Invalid ZIP64 end record".to_string())?,
        )
        .map_err(|_| "ZIP64 end record offset is not representable".to_string())?;
        let zip64_record_is_valid = zip64_end
            .checked_add(56)
            .is_some_and(|limit| limit <= bytes.len())
            && u32_at(bytes, zip64_end) == Some(ZIP64_EOCD);
        if !zip64_record_is_valid {
            return Err("Invalid ZIP64 end record".to_string());
        }
        count = usize::try_from(
            u32_at(bytes, zip64_end + 32).ok_or_else(|| "Invalid ZIP64 entry count".to_string())?,
        )
        .map_err(|_| "ZIP64 entry count is not representable".to_string())?;
        central = usize::try_from(
            u32_at(bytes, zip64_end + 48)
                .ok_or_else(|| "Invalid ZIP64 central-directory offset".to_string())?,
        )
        .map_err(|_| "ZIP64 central-directory offset is not representable".to_string())?;
    }
    if count != expected_entries {
        return Err("ZIP entry-count metadata mismatch".to_string());
    }

    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        if u32_at(bytes, central) != Some(CENTRAL_HEADER) {
            return Err("Invalid ZIP central-directory entry".to_string());
        }
        let made_by =
            u16_at(bytes, central + 4).ok_or_else(|| "Invalid ZIP creator field".to_string())?;
        let flags = u16_at(bytes, central + 8).ok_or_else(|| "Invalid ZIP flags".to_string())?;
        let name_len = usize::from(
            u16_at(bytes, central + 28).ok_or_else(|| "Invalid ZIP filename length".to_string())?,
        );
        let extra_len = usize::from(
            u16_at(bytes, central + 30).ok_or_else(|| "Invalid ZIP extra length".to_string())?,
        );
        let comment_len = usize::from(
            u16_at(bytes, central + 32).ok_or_else(|| "Invalid ZIP comment length".to_string())?,
        );
        let external_attributes = u32_at(bytes, central + 38)
            .ok_or_else(|| "Invalid ZIP external attributes".to_string())?;
        let mode = if made_by >> 8 == 3 {
            let mode = external_attributes >> 16;
            (mode != 0).then_some(mode)
        } else {
            None
        };
        let name_start = central
            .checked_add(46)
            .ok_or_else(|| "ZIP filename offset overflow".to_string())?;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| "ZIP filename offset overflow".to_string())?;
        let raw_name = bytes
            .get(name_start..name_end)
            .ok_or_else(|| "Invalid ZIP filename bytes".to_string())?;
        entries.push(NativeZipEntryMetadata {
            path_name: decode_native_zip_path_name(raw_name, flags),
            mode_key: String::from_utf8_lossy(raw_name).into_owned(),
            unix_mode: mode,
        });
        central = central
            .checked_add(46)
            .and_then(|offset| offset.checked_add(name_len))
            .and_then(|offset| offset.checked_add(extra_len))
            .and_then(|offset| offset.checked_add(comment_len))
            .ok_or_else(|| "ZIP central-directory offset overflow".to_string())?;
    }
    Ok(entries)
}

fn native_enclosed_name(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') || native_has_parent_like_segment(name) {
        return None;
    }
    let path = Path::new(name);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
    {
        return None;
    }
    Some(path.to_path_buf())
}

fn native_has_parent_like_segment(name: &str) -> bool {
    name.split(|ch| ch == '/' || ch == '\\').any(|segment| {
        segment == ".."
            || segment
                .strip_prefix(".. ")
                .is_some_and(|suffix| suffix.chars().all(|ch| ch == ' ' || ch == '.'))
    })
}

#[cfg(unix)]
fn apply_native_unix_mode(path: &Path, mode: Option<u32>) {
    use std::os::unix::fs::PermissionsExt;

    if let Some(mode) = mode.filter(|mode| mode & 0o111 != 0) {
        // Native's iH catches chmod failures and masks to 0o755. The archive
        // entry has already been written as a regular file, so this never
        // follows an archive-declared symlink.
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o755));
    }
}

#[cfg(not(unix))]
fn apply_native_unix_mode(_path: &Path, _mode: Option<u32>) {}

fn unpack_zip_with_limits(bytes: &[u8], dest: &Path, limits: ZipLimits) -> Result<(), String> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("Failed to extract MCPB {}: {e}", dest.display()))?;
    if zip.len() > limits.max_files {
        // Binary: `Archive contains too many files: ${fileCount} (max: ${MAX_FILE_COUNT})`.
        return Err(format!(
            "Archive contains too many files: {} (max: {})",
            zip.len(),
            limits.max_files
        ));
    }
    let native_entries = if limits.native_methods_only {
        Some(read_native_zip_entry_metadata(bytes, zip.len())?)
    } else {
        None
    };
    let native_modes_by_name = native_entries.as_ref().map(|entries| {
        let mut modes = HashMap::new();
        for entry in entries {
            if let Some(mode) = entry.unix_mode {
                // x4t assigns into an object by the Buffer-decoded name, so a
                // later duplicate key replaces the earlier mode.
                modes.insert(entry.mode_key.clone(), mode);
            }
        }
        modes
    });
    let compressed_bytes = u64::try_from(bytes.len())
        .map_err(|_| "ZIP archive length is not representable".to_string())?;
    let mut total_claimed: u64 = 0;
    let mut total_actual: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| format!("Failed to extract MCPB {}: {e}", dest.display()))?;
        let native_metadata = native_entries.as_ref().and_then(|entries| entries.get(i));
        let entry_name = match native_metadata {
            Some(metadata) => metadata.path_name.clone(),
            None => entry
                .name()
                .map_err(|e| format!("Failed to extract MCPB {}: {e}", dest.display()))?
                .into_owned(),
        };
        // Native x4t builds a name-keyed object from raw bytes decoded as
        // UTF-8, then Ft looks up the Ht-decoded path name in that object.
        // Do not substitute central-entry index matching here.
        let native_unix_mode = native_modes_by_name
            .as_ref()
            .and_then(|modes| modes.get(&entry_name))
            .copied();
        let claimed_size = entry.size();
        if limits.native_methods_only && claimed_size > limits.max_file_bytes {
            return Err(format!(
                "ZIP entry is too large: {} bytes (max: {})",
                claimed_size, limits.max_file_bytes
            ));
        }
        if limits.native_methods_only {
            total_claimed = total_claimed
                .checked_add(claimed_size)
                .ok_or_else(|| "ZIP total uncompressed size overflow".to_string())?;
            if total_claimed > limits.max_total_bytes {
                return Err(format!(
                    "Archive total size is too large: {} bytes (max: {})",
                    total_claimed, limits.max_total_bytes
                ));
            }
            if let Some(max_ratio) = limits.max_ratio {
                let max_expanded = compressed_bytes
                    .checked_mul(max_ratio)
                    .ok_or_else(|| "ZIP compression-ratio limit overflow".to_string())?;
                if total_claimed > max_expanded {
                    return Err(format!(
                        "Suspicious compression ratio detected (max: {max_ratio}:1)"
                    ));
                }
            }
        }
        if limits.native_methods_only
            && !matches!(
                entry.compression(),
                zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
            )
        {
            return Err(format!(
                "Unsupported compression method {:?}",
                entry.compression()
            ));
        }
        // `enclosed_name` returns None for absolute paths / `..` traversal; the
        // explicit component + containment checks below are belt-and-suspenders.
        let rel = native_metadata
            .map(|_| native_enclosed_name(&entry_name))
            .unwrap_or_else(|| entry.enclosed_name())
            .filter(|p| {
                !p.components().any(|c| {
                    matches!(
                        c,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            })
            .ok_or_else(|| format!("Path traversal attempt detected: {entry_name}"))?;
        let out = dest.join(&rel);
        if !out.starts_with(dest) {
            return Err(format!("Path traversal attempt detected: {entry_name}"));
        }
        // Native's V2 ZIP decoder treats a name as a directory only when it
        // ends in `/` (`iH`); zip-rs also treats a trailing `\\` as a
        // directory. Keep that broader behavior for MCPB, but do not use it
        // for the Native cache format.
        let is_directory = if limits.native_methods_only {
            entry_name.ends_with('/')
        } else {
            entry.is_dir()
        };
        if is_directory {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            if !limits.native_methods_only {
                drop_group_other_write(&out).map_err(|e| e.to_string())?;
            }
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            // Bound against both the remaining archive budget and this entry's
            // declared size; then verify the actual output did not exceed its
            // claim. Native's inflater similarly uses declared size as its
            // maxOutputLength while the cache decoder enforces aggregate caps.
            let remaining = limits.max_total_bytes.saturating_sub(total_actual);
            let read_cap = if limits.native_methods_only {
                remaining.min(claimed_size)
            } else {
                remaining
            };
            let cap = usize::try_from(read_cap).unwrap_or(usize::MAX);
            let mut buf = Vec::with_capacity(cap.min(1 << 20));
            entry
                .by_ref()
                .take(read_cap.saturating_add(1))
                .read_to_end(&mut buf)
                .map_err(|e| e.to_string())?;
            if buf.len() as u64 > read_cap
                || limits.native_methods_only && buf.len() as u64 > claimed_size
            {
                return Err(if limits.native_methods_only {
                    format!(
                        "ZIP entry expanded beyond its declared or allowed size: {}",
                        entry_name
                    )
                } else {
                    format!(
                        "Archive total size is too large: more than {} bytes. This may be a zip bomb.",
                        limits.max_total_bytes
                    )
                });
            }
            total_actual = total_actual
                .checked_add(buf.len() as u64)
                .ok_or_else(|| "ZIP actual expanded size overflow".to_string())?;
            if total_actual > limits.max_total_bytes {
                return Err(format!(
                    "Archive total size is too large: more than {} bytes",
                    limits.max_total_bytes
                ));
            }
            std::fs::write(&out, &buf).map_err(|e| e.to_string())?;
            if limits.native_methods_only {
                apply_native_unix_mode(&out, native_unix_mode);
            } else {
                drop_group_other_write(&out).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Ensure the extracted bundle at `dir` has a supported plugin manifest the
/// shared loader can read. If both supported paths are absent but a root
/// `manifest.json` (MCPB schema) is present, translate the `name`/`version`
/// into a minimal synthetic LingXi `plugin.json`.
///
/// # Errors
/// Returns a byte-faithful detail when neither manifest is present/valid.
pub fn ensure_plugin_manifest(dir: &Path) -> Result<(), String> {
    let plugin_json = branding::plugin_manifest_path(dir)
        .map_err(|error| format!("Could not inspect plugin manifest path: {error}"))?;
    if std::fs::symlink_metadata(&plugin_json).is_ok() {
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

    #[test]
    fn ensure_manifest_preserves_current_upstream_plugin_json() {
        let root = scratch("upstream-plugin");
        let manifest_dir = root.join(branding::CLAUDE_PLUGIN_MANIFEST_DIR);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        let raw = br#"{"name":"webmcp","version":"0.10.0"}"#;
        std::fs::write(manifest_dir.join("plugin.json"), raw).unwrap();

        ensure_plugin_manifest(&root).unwrap();

        assert_eq!(
            std::fs::read(manifest_dir.join("plugin.json")).unwrap(),
            raw
        );
        assert!(!root.join(branding::PLUGIN_MANIFEST_DIR).exists());
        let _ = std::fs::remove_dir_all(root);
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

    fn set_central_unix_mode(zip: &mut [u8], name: &str, mode: u32) {
        let mut central = zip
            .windows(4)
            .position(|bytes| bytes == b"PK\x01\x02")
            .expect("central-directory file header");
        loop {
            assert_eq!(&zip[central..central + 4], b"PK\x01\x02");
            let name_len = usize::from(u16::from_le_bytes([zip[central + 28], zip[central + 29]]));
            let extra_len = usize::from(u16::from_le_bytes([zip[central + 30], zip[central + 31]]));
            let comment_len =
                usize::from(u16::from_le_bytes([zip[central + 32], zip[central + 33]]));
            let name_start = central + 46;
            let name_end = name_start + name_len;
            if zip.get(name_start..name_end) == Some(name.as_bytes()) {
                let made_by_unix = ((3_u16) << 8) | 20;
                zip[central + 4..central + 6].copy_from_slice(&made_by_unix.to_le_bytes());
                zip[central + 38..central + 42].copy_from_slice(&(mode << 16).to_le_bytes());
                return;
            }
            central = central + 46 + name_len + extra_len + comment_len;
            assert!(central + 4 <= zip.len(), "central entry `{name}` exists");
        }
    }

    fn replace_zip_name_and_utf8_flag(
        zip: &mut [u8],
        old_name: &[u8],
        new_name: &[u8],
        utf8_flag: bool,
    ) {
        assert_eq!(old_name.len(), new_name.len());
        let mut central = zip
            .windows(4)
            .position(|bytes| bytes == b"PK\x01\x02")
            .expect("central-directory file header");
        loop {
            assert_eq!(&zip[central..central + 4], b"PK\x01\x02");
            let flags = u16::from_le_bytes([zip[central + 8], zip[central + 9]]);
            let name_len = usize::from(u16::from_le_bytes([zip[central + 28], zip[central + 29]]));
            let extra_len = usize::from(u16::from_le_bytes([zip[central + 30], zip[central + 31]]));
            let comment_len =
                usize::from(u16::from_le_bytes([zip[central + 32], zip[central + 33]]));
            let name_start = central + 46;
            let name_end = name_start + name_len;
            if zip.get(name_start..name_end) == Some(old_name) {
                assert_eq!(name_len, new_name.len());
                let flags = if utf8_flag {
                    flags | (1 << 11)
                } else {
                    flags & !(1 << 11)
                };
                zip[central + 8..central + 10].copy_from_slice(&flags.to_le_bytes());
                zip[name_start..name_end].copy_from_slice(new_name);

                let local = usize::try_from(u32::from_le_bytes(
                    zip[central + 42..central + 46].try_into().unwrap(),
                ))
                .unwrap();
                assert_eq!(&zip[local..local + 4], b"PK\x03\x04");
                let local_flags = u16::from_le_bytes([zip[local + 6], zip[local + 7]]);
                let local_flags = if utf8_flag {
                    local_flags | (1 << 11)
                } else {
                    local_flags & !(1 << 11)
                };
                zip[local + 6..local + 8].copy_from_slice(&local_flags.to_le_bytes());
                let local_name_len =
                    usize::from(u16::from_le_bytes([zip[local + 26], zip[local + 27]]));
                assert_eq!(local_name_len, new_name.len());
                let local_name_start = local + 30;
                zip[local_name_start..local_name_start + local_name_len].copy_from_slice(new_name);
                return;
            }
            central = central + 46 + name_len + extra_len + comment_len;
            assert!(central + 4 <= zip.len(), "central entry exists");
        }
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

    #[test]
    fn native_cache_ratio_limit_is_separate_from_mcpb_limits() {
        use std::io::Write;

        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            writer
                .start_file::<_, ()>(
                    "payload.bin",
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(&vec![0; 32 * 1024]).unwrap();
            writer.finish().unwrap();
        }
        assert!(
            32 * 1024 > buf.len() * NATIVE_PLUGIN_CACHE_MAX_RATIO as usize,
            "fixture must exceed the native 50:1 aggregate ratio"
        );

        let cache_dest = tempfile::tempdir().unwrap();
        let err = unpack_plugin_cache_zip(&buf, cache_dest.path()).unwrap_err();
        assert!(err.contains("compression ratio"), "got: {err}");

        // MCPB's own format policy remains the existing 10k/1 GiB guard; it
        // does not inherit V2's cache-only ratio rule.
        let mcpb_dest = tempfile::tempdir().unwrap();
        unpack_mcpb(&buf, mcpb_dest.path()).unwrap();
        assert_eq!(
            std::fs::metadata(mcpb_dest.path().join("payload.bin"))
                .unwrap()
                .len(),
            32 * 1024
        );
    }

    #[test]
    fn native_cache_policy_enforces_individual_entry_cap_before_extracting() {
        let buf = zip_with(&[("payload.bin", b"0123456789abcdef")]);
        let dest = tempfile::tempdir().unwrap();
        let err = unpack_zip_with_limits(
            &buf,
            dest.path(),
            ZipLimits {
                max_files: 100_000,
                max_file_bytes: 8,
                max_total_bytes: 1024,
                max_ratio: Some(50),
                native_methods_only: true,
            },
        )
        .unwrap_err();
        assert!(err.contains("entry is too large"), "got: {err}");
        assert!(!dest.path().join("payload.bin").exists());
    }

    #[test]
    fn native_cache_decodes_unflagged_filename_as_latin1() {
        let mut archive = zip_with(&[("cafe.txt", b"latin1 name")]);
        replace_zip_name_and_utf8_flag(&mut archive, b"cafe.txt", b"caf\xe9.txt", false);

        let dest = tempfile::tempdir().unwrap();
        unpack_plugin_cache_zip(&archive, dest.path()).unwrap();
        let native_name = dest.path().join("caf\u{e9}.txt");
        assert_eq!(std::fs::read(&native_name).unwrap(), b"latin1 name");
        assert!(!dest.path().join("caf\u{0398}.txt").exists());
    }

    #[test]
    fn native_cache_rejects_parent_segments_using_both_separators_and_dot_space_guard() {
        for name in ["a/../b.txt", r"a\..\b.txt", "a/.. /b.txt", r"a\.. .\b.txt"] {
            let archive = zip_with(&[(name, b"must not be written")]);
            let dest = tempfile::tempdir().unwrap();
            let error = unpack_plugin_cache_zip(&archive, dest.path()).unwrap_err();
            assert!(error.contains("Path traversal"), "name={name:?}: {error}");
            assert!(std::fs::read_dir(dest.path()).unwrap().next().is_none());
        }
        assert!(native_enclosed_name(r"safe\relative.txt").is_some());
        assert!(native_enclosed_name(".../relative.txt").is_some());
    }

    #[test]
    fn native_cache_utf8_flag_uses_nonfatal_decode_and_suppresses_leading_bom() {
        let mut archive = zip_with(&[("\u{feff}bom.txt", b"bom"), ("badx.txt", b"invalid")]);
        replace_zip_name_and_utf8_flag(&mut archive, b"badx.txt", b"bad\xff.txt", true);

        let dest = tempfile::tempdir().unwrap();
        unpack_plugin_cache_zip(&archive, dest.path()).unwrap();
        assert_eq!(std::fs::read(dest.path().join("bom.txt")).unwrap(), b"bom");
        assert!(!dest.path().join("\u{feff}bom.txt").exists());
        assert_eq!(
            std::fs::read(dest.path().join("bad\u{fffd}.txt")).unwrap(),
            b"invalid"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_cache_unflagged_latin1_name_does_not_match_utf8_mode_map_key() {
        use std::os::unix::fs::PermissionsExt;

        let mut archive = zip_with(&[("rawxx.bin", b"mode lookup")]);
        set_central_unix_mode(&mut archive, "rawxx.bin", 0o100755);
        replace_zip_name_and_utf8_flag(&mut archive, b"rawxx.bin", b"raw\xc3\xa9.bin", false);

        let metadata = read_native_zip_entry_metadata(&archive, 1).unwrap();
        assert_eq!(metadata[0].path_name, "raw\u{c3}\u{a9}.bin");
        assert_eq!(metadata[0].mode_key, "raw\u{e9}.bin");

        let dest = tempfile::tempdir().unwrap();
        unpack_plugin_cache_zip(&archive, dest.path()).unwrap();
        let extracted = dest.path().join("raw\u{c3}\u{a9}.bin");
        assert_eq!(std::fs::read(&extracted).unwrap(), b"mode lookup");
        let mode = std::fs::metadata(extracted).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o111,
            0,
            "x4t's UTF-8 key does not match Ht's Latin-1 path name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_cache_uses_forward_slash_directories_and_materializes_symlink_records_as_files() {
        use std::os::unix::fs::PermissionsExt;

        let backslash_name = zip_with(&[("payload\\", b"regular bytes")]);
        let backslash_dest = tempfile::tempdir().unwrap();
        unpack_plugin_cache_zip(&backslash_name, backslash_dest.path()).unwrap();
        let backslash_file = backslash_dest.path().join("payload\\");
        assert!(backslash_file.is_file());
        assert_eq!(std::fs::read(&backslash_file).unwrap(), b"regular bytes");

        let mut unix_modes = zip_with(&[
            ("executable", b"exec bytes"),
            ("non-executable", b"plain bytes"),
            ("linked-payload", b"still regular bytes"),
        ]);
        set_central_unix_mode(&mut unix_modes, "executable", 0o100755);
        set_central_unix_mode(&mut unix_modes, "non-executable", 0o100640);
        set_central_unix_mode(&mut unix_modes, "linked-payload", 0o120777);

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&unix_modes)).unwrap();
        assert!(archive.by_index(2).unwrap().is_symlink());
        drop(archive);

        let modes_dest = tempfile::tempdir().unwrap();
        let default_file = modes_dest.path().join("default-mode-probe");
        std::fs::write(&default_file, b"baseline").unwrap();
        let default_mode = {
            std::fs::metadata(default_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        unpack_plugin_cache_zip(&unix_modes, modes_dest.path()).unwrap();
        let executable_mode = std::fs::metadata(modes_dest.path().join("executable"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(executable_mode, 0o755);
        let non_executable_mode = std::fs::metadata(modes_dest.path().join("non-executable"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(non_executable_mode, default_mode);

        let output = modes_dest.path().join("linked-payload");
        assert!(std::fs::symlink_metadata(&output)
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(std::fs::read(&output).unwrap(), b"still regular bytes");
        let symlink_mode = std::fs::metadata(&output).unwrap().permissions().mode() & 0o777;
        assert_eq!(symlink_mode, 0o755);
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
