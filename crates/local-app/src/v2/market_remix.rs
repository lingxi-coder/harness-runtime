//! Verified Market archive input for trusted native authoring.
use super::{
    fingerprint_manifest_inventory, AppError, Manifest, MarketOrigin, ReleaseFile, MANIFEST_FILE,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Read},
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

const MAX_ARCHIVE: u64 = 25 * 1024 * 1024;
const MAX_FILE: u64 = 25 * 1024 * 1024;
const MAX_TOTAL: u64 = 100 * 1024 * 1024;
const MAX_FILES: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MarketFile {
    path: String,
    size_bytes: u64,
    sha256: String,
    executable: bool,
}

/// A parsed immutable Market release, including its request-owned archive.
pub struct MarketRemixInput {
    archive_relative: PathBuf,
    archive_bytes: u64,
    archive_sha256: String,
    manifest: Manifest,
    files: BTreeMap<String, MarketFile>,
    /// Expected digest after the normal Local App publication normalization.
    pub expected_digest: String,
    /// Origin records the exact Market release from which this source came.
    pub origin: MarketOrigin,
}

fn invalid(message: impl Into<String>) -> AppError {
    AppError::Invalid(message.into())
}
fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, AppError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("{key} is required")))
}

// The Market's canonical JSON is JSON.stringify semantics with recursively
// UTF-16-sorted object keys. In particular 1.0 serializes as 1 and -0 as 0.
fn canonical_json(value: &Value, out: &mut String) -> Result<(), AppError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let n = n
                .as_f64()
                .filter(|n| n.is_finite())
                .ok_or_else(|| invalid("invalid Market number"))?;
            if n == 0.0 {
                out.push('0');
            } else {
                out.push_str(ryu_js::Buffer::new().format_finite(n));
            }
        }
        Value::String(s) => out.push_str(&serde_json::to_string(s)?),
        Value::Array(values) => {
            out.push('[');
            for (i, value) in values.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                canonical_json(value, out)?;
            }
            out.push(']');
        }
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key)?);
                out.push(':');
                canonical_json(&values[key], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// Compute the Market protocol's canonical release checksum from its exact
/// captured distribution fields, before accessing the archive.
pub fn market_release_digest(
    archive_sha256: &str,
    manifest: &Value,
    files: &Value,
    dependencies: &Value,
) -> Result<String, AppError> {
    let signed = json!({"schema":2,"artifactSha256":archive_sha256,"manifest":manifest,
        "files":files,"dependencies":dependencies});
    let mut canonical = String::new();
    canonical_json(&signed, &mut canonical)?;
    Ok(sha256(canonical.as_bytes()))
}

fn valid_package_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.starts_with('/')
        && path.nfc().eq(path.chars())
        && !path
            .chars()
            .any(|c| matches!(c, '\\' | '%' | '?' | '#' | ':') || c.is_control())
        && path.split('/').count() <= 64
        && path
            .split('/')
            .all(|part| !part.is_empty() && part.len() <= 255 && part != "." && part != "..")
}

impl MarketRemixInput {
    /// Validate the full captured distribution and the exact private staging
    /// path. This does not access the archive and is safe before a replay.
    pub fn parse(request: &Value, config_home: &Path) -> Result<Self, AppError> {
        let object = request
            .as_object()
            .ok_or_else(|| invalid("request must be an object"))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "op" | "requestId"
                    | "archivePath"
                    | "archiveSha256"
                    | "archiveBytes"
                    | "manifest"
                    | "files"
                    | "dependencies"
                    | "marketOrigin"
            )
        }) || required(request, "op")? != "importRemixSource"
        {
            return Err(invalid("invalid Market remix request"));
        }
        let request_id = required(request, "requestId")?;
        if !uuid::Uuid::parse_str(request_id).is_ok_and(|id| id.to_string() == request_id) {
            return Err(invalid("invalid Market remix requestId"));
        }
        if !config_home.is_absolute() {
            return Err(invalid("invalid configuration root"));
        }
        let relative = PathBuf::from("local-app-imports")
            .join(format!("market-{request_id}"))
            .join("source.zip");
        let archive_path = config_home.join(&relative);
        if archive_path.to_str() != Some(required(request, "archivePath")?) {
            return Err(invalid(
                "archivePath must be the request-owned staging file",
            ));
        }
        let archive_sha256 = required(request, "archiveSha256")?.to_owned();
        let archive_bytes = request
            .get("archiveBytes")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0 && *n <= MAX_ARCHIVE)
            .ok_or_else(|| invalid("invalid Market archive size"))?;
        if !valid_digest(&archive_sha256) {
            return Err(invalid("invalid Market archive digest"));
        }
        let manifest_value = request
            .get("manifest")
            .ok_or_else(|| invalid("manifest is required"))?;
        let manifest: Manifest = serde_json::from_value(manifest_value.clone())?;
        manifest.validate()?;
        if !manifest
            .remix
            .as_ref()
            .is_some_and(|r| r.allowed && r.license.as_ref().is_some_and(|s| !s.trim().is_empty()))
        {
            return Err(invalid("this release does not authorize remixing"));
        }
        let files_value = request
            .get("files")
            .ok_or_else(|| invalid("files are required"))?;
        let dependencies_value = request
            .get("dependencies")
            .ok_or_else(|| invalid("dependencies are required"))?;
        let files: Vec<MarketFile> = serde_json::from_value(files_value.clone())?;
        let dependencies: Vec<MarketFile> = serde_json::from_value(dependencies_value.clone())?;
        if files.is_empty() || files.len() > MAX_FILES || dependencies.len() > MAX_FILES {
            return Err(invalid("invalid Market file inventory"));
        }
        let mut by_path = BTreeMap::new();
        let mut folded = BTreeSet::new();
        let mut total = 0u64;
        for file in files {
            if !valid_package_path(&file.path)
                || !valid_digest(&file.sha256)
                || file.size_bytes > MAX_FILE
            {
                return Err(invalid("invalid Market file"));
            }
            total = total
                .checked_add(file.size_bytes)
                .ok_or_else(|| invalid("Market size overflow"))?;
            if total > MAX_TOTAL
                || !folded.insert(file.path.to_lowercase())
                || by_path.insert(file.path.clone(), file).is_some()
            {
                return Err(invalid("duplicate or oversized Market inventory"));
            }
        }
        if !by_path.contains_key(MANIFEST_FILE) || by_path[MANIFEST_FILE].executable {
            return Err(invalid("invalid Market manifest inventory"));
        }
        let mut dependency_paths = BTreeSet::new();
        for dependency in dependencies {
            if !dependency_paths.insert(dependency.path.clone())
                || by_path.get(&dependency.path) != Some(&dependency)
            {
                return Err(invalid("Market dependency inventory mismatch"));
            }
        }
        let origin = request
            .get("marketOrigin")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid("marketOrigin is required"))?;
        if origin
            .keys()
            .any(|key| !matches!(key.as_str(), "appId" | "releaseId" | "digest"))
        {
            return Err(invalid("invalid Market origin"));
        }
        let origin_value = &request["marketOrigin"];
        let app_id = required(origin_value, "appId")?.to_owned();
        let release_id = required(origin_value, "releaseId")?.to_owned();
        let digest = required(origin_value, "digest")?.to_owned();
        if app_id.len() > 128 || release_id.len() > 128 || !valid_digest(&digest) {
            return Err(invalid("invalid Market origin"));
        }
        if market_release_digest(
            &archive_sha256,
            manifest_value,
            files_value,
            dependencies_value,
        )? != digest
        {
            return Err(invalid("Market release checksum mismatch"));
        }
        let inventory: Vec<_> = by_path
            .values()
            .map(|file| ReleaseFile {
                path: file.path.clone(),
                sha256: file.sha256.clone(),
                bytes: file.size_bytes,
                executable: file.executable,
            })
            .collect();
        let expected_digest = fingerprint_manifest_inventory(&manifest, &inventory)?;
        Ok(Self {
            archive_relative: relative,
            archive_bytes,
            archive_sha256,
            manifest,
            files: by_path,
            expected_digest,
            origin: MarketOrigin {
                remixed_from: Some(app_id.clone()),
                app_id,
                release_id,
                digest,
            },
        })
    }

    /// Verify the pinned archive, then extract only the approved inventory into
    /// a new no-follow source directory. A failed attempt never changes an App.
    pub fn extract(&self, config_home: &Path, destination: &Path) -> Result<(), AppError> {
        let config_pin =
            rooted_fs::root_identity(config_home).map_err(|e| invalid(e.to_string()))?;
        let mut file = match rooted_fs::open_read_file_pinned(
            config_home,
            &self.archive_relative,
            Some(&config_pin),
        ) {
            Ok(file) => file,
            Err(rooted_fs::FsError::NotFound(_)) => return Err(AppError::ArchiveMissing),
            Err(error) => return Err(invalid(format!("invalid Market archive: {error}"))),
        };
        rooted_fs::validate_open_file_single_link(&file).map_err(|e| invalid(e.to_string()))?;
        if !file.metadata()?.is_file() {
            return Err(invalid("Market archive must be a regular file"));
        }
        let identity =
            rooted_fs::opened_file_identity(&file).map_err(|e| invalid(e.to_string()))?;
        if file.metadata()?.len() != self.archive_bytes {
            return Err(invalid("Market archive size mismatch"));
        }
        let mut archive = Vec::new();
        file.by_ref()
            .take(self.archive_bytes + 1)
            .read_to_end(&mut archive)?;
        if archive.len() as u64 != self.archive_bytes || sha256(&archive) != self.archive_sha256 {
            return Err(invalid("Market archive checksum mismatch"));
        }
        let current = rooted_fs::open_read_file_pinned(
            config_home,
            &self.archive_relative,
            Some(&config_pin),
        )
        .map_err(|e| invalid(format!("Market archive changed: {e}")))?;
        if rooted_fs::opened_file_identity(&current).map_err(|e| invalid(e.to_string()))?
            != identity
        {
            return Err(invalid("Market archive changed"));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("source parent missing"))?;
        let leaf = destination
            .file_name()
            .ok_or_else(|| invalid("source name missing"))?;
        let parent_pin = rooted_fs::root_identity(parent).map_err(|e| invalid(e.to_string()))?;
        let opened =
            rooted_fs::create_directory_new_pinned(parent, Path::new(leaf), 0o700, &parent_pin)
                .map_err(|e| invalid(format!("cannot create remix source: {e}")))?;
        let source_pin =
            rooted_fs::opened_file_identity(&opened).map_err(|e| invalid(e.to_string()))?;
        let mut zip = zip::ZipArchive::new(Cursor::new(&archive))
            .map_err(|e| invalid(format!("invalid Market ZIP: {e}")))?;
        if zip.len() > MAX_FILES * 2 {
            return Err(invalid("too many Market ZIP entries"));
        }
        let mut seen = BTreeSet::new();
        let mut seen_folded = BTreeSet::new();
        let mut all_entries = BTreeSet::new();
        let mut all_entries_folded = BTreeSet::new();
        let mut actual_manifest = None;
        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|e| invalid(format!("invalid Market ZIP entry: {e}")))?;
            let raw_name = std::str::from_utf8(entry.name_raw())
                .map_err(|_| invalid("Market ZIP name is not UTF-8"))?
                .to_owned();
            let name = raw_name.strip_suffix('/').unwrap_or(&raw_name);
            if !valid_package_path(name)
                || entry.encrypted()
                || !matches!(
                    entry.compression(),
                    zip::CompressionMethod::Stored | zip::CompressionMethod::Deflated
                )
            {
                return Err(invalid("unsupported Market ZIP entry"));
            }
            if !all_entries.insert(name.to_owned())
                || !all_entries_folded.insert(name.to_lowercase())
            {
                return Err(invalid("duplicate Market ZIP entry"));
            }
            let mode = entry.unix_mode().unwrap_or(0) & 0o170000;
            if mode != 0 && mode != if entry.is_dir() { 0o040000 } else { 0o100000 } {
                return Err(invalid("Market ZIP contains a non-regular entry"));
            }
            // Bind the central name to the local header as well.
            let offset = usize::try_from(entry.header_start())
                .map_err(|_| invalid("invalid Market ZIP offset"))?;
            let header = archive
                .get(offset..offset.saturating_add(30))
                .ok_or_else(|| invalid("truncated Market ZIP header"))?;
            if &header[..4] != b"PK\x03\x04" {
                return Err(invalid("invalid Market ZIP local header"));
            }
            let flags = u16::from_le_bytes([header[6], header[7]]);
            let method = u16::from_le_bytes([header[8], header[9]]);
            let expected_method = match entry.compression() {
                zip::CompressionMethod::Stored => 0,
                zip::CompressionMethod::Deflated => 8,
                _ => unreachable!(),
            };
            if flags & 1 != 0 || method != expected_method {
                return Err(invalid("Market ZIP local header mismatch"));
            }
            let name_len = u16::from_le_bytes([header[26], header[27]]) as usize;
            let name_start = offset + 30;
            if archive.get(name_start..name_start.saturating_add(name_len))
                != Some(entry.name_raw())
            {
                return Err(invalid("Market ZIP entry name mismatch"));
            }
            if entry.is_dir() {
                continue;
            }
            let expected = self
                .files
                .get(name)
                .ok_or_else(|| invalid("unexpected Market ZIP file"))?;
            if !seen.insert(name.to_owned())
                || !seen_folded.insert(name.to_lowercase())
                || entry.size() != expected.size_bytes
            {
                return Err(invalid("duplicate or mismatched Market ZIP file"));
            }
            let mut bytes = Vec::new();
            entry
                .by_ref()
                .take(expected.size_bytes + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| invalid(format!("cannot read Market ZIP file: {e}")))?;
            if bytes.len() as u64 != expected.size_bytes || sha256(&bytes) != expected.sha256 {
                return Err(invalid("Market ZIP file checksum mismatch"));
            }
            if name == MANIFEST_FILE {
                actual_manifest = Some(Manifest::parse(
                    std::str::from_utf8(&bytes).map_err(|_| invalid("manifest must be UTF-8"))?,
                )?);
            }
            let options = rooted_fs::AtomicWriteOptions {
                overwrite: false,
                create_parents: true,
                dir_mode: 0o700,
                file_mode: if expected.executable { 0o700 } else { 0o600 },
            };
            rooted_fs::atomic_write_pinned(
                destination,
                Path::new(name),
                &bytes,
                options,
                &source_pin,
            )
            .map_err(|e| invalid(format!("cannot write remix source: {e}")))?;
        }
        if seen.len() != self.files.len()
            || actual_manifest.as_ref() != Some(&self.manifest)
            || rooted_fs::root_identity(config_home).ok().as_ref() != Some(&config_pin)
            || rooted_fs::root_identity(parent).ok().as_ref() != Some(&parent_pin)
            || rooted_fs::root_identity(destination).ok().as_ref() != Some(&source_pin)
        {
            return Err(invalid(
                "Market source inventory, manifest or identity changed",
            ));
        }
        Ok(())
    }

    /// Recheck every approved byte before publication, including after an
    /// interrupted extraction has already received its source proof.
    pub fn verify_source(&self, source: &Path) -> Result<(), AppError> {
        fn walk(
            root: &Path,
            current: &Path,
            output: &mut BTreeSet<String>,
            depth: usize,
            remaining: &mut usize,
        ) -> Result<(), AppError> {
            if depth > 64 {
                return Err(invalid("remix source is too deep"));
            }
            for entry in std::fs::read_dir(current)? {
                *remaining = remaining
                    .checked_sub(1)
                    .ok_or_else(|| invalid("remix source has too many entries"))?;
                let entry = entry?;
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    return Err(invalid("remix source contains a symlink"));
                }
                if kind.is_dir() {
                    walk(root, &entry.path(), output, depth + 1, remaining)?;
                } else if kind.is_file() {
                    let name = entry
                        .path()
                        .strip_prefix(root)
                        .map_err(|_| invalid("remix source escaped root"))?
                        .to_str()
                        .ok_or_else(|| invalid("remix source path is not UTF-8"))?
                        .replace('\\', "/");
                    if !valid_package_path(&name)
                        || !output.insert(name)
                        || output.len() > MAX_FILES
                    {
                        return Err(invalid("invalid remix source inventory"));
                    }
                } else {
                    return Err(invalid("remix source contains a special file"));
                }
            }
            Ok(())
        }
        let pin = rooted_fs::root_identity(source).map_err(|e| invalid(e.to_string()))?;
        let mut names = BTreeSet::new();
        let mut remaining = MAX_FILES * 2;
        walk(source, source, &mut names, 0, &mut remaining)?;
        if names.len() != self.files.len()
            || names.iter().any(|name| !self.files.contains_key(name))
        {
            return Err(invalid("remix source inventory changed"));
        }
        for (name, expected) in &self.files {
            let mut file = rooted_fs::open_read_file_pinned(source, Path::new(name), Some(&pin))
                .map_err(|e| invalid(format!("invalid remix source file: {e}")))?;
            rooted_fs::validate_open_file_single_link(&file).map_err(|e| invalid(e.to_string()))?;
            let metadata = file.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if (metadata.permissions().mode() & 0o111 != 0) != expected.executable {
                    return Err(invalid("remix source permissions changed"));
                }
            }
            if metadata.len() != expected.size_bytes {
                return Err(invalid("remix source size changed"));
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take(expected.size_bytes + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != expected.size_bytes || sha256(&bytes) != expected.sha256 {
                return Err(invalid("remix source checksum changed"));
            }
        }
        if rooted_fs::root_identity(source).ok().as_ref() != Some(&pin) {
            return Err(invalid("remix source identity changed"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn request_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Value) {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let request_id = uuid::Uuid::new_v4().to_string();
        let archive_path = config
            .join("local-app-imports")
            .join(format!("market-{request_id}"))
            .join("source.zip");
        std::fs::create_dir_all(archive_path.parent().unwrap()).unwrap();
        let manifest = json!({"schema":2,"name":"Market fixture","summary":null,
            "entry":{"type":"static","path":"index.html"},
            "targets":[{"platform":"ios","environment":"web"}],
            "remix":{"allowed":true,"license":"MIT"}});
        // Summary is a string in the typed manifest; retain a nullable optional
        // field to verify null participates in the Market signature.
        let mut manifest = manifest;
        manifest["summary"] = json!("");
        manifest["dataSchema"] = Value::Null;
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let html = b"<!doctype html><html>market</html>";
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o600);
        writer.start_file("lingxi-app.json", options).unwrap();
        writer.write_all(&manifest_bytes).unwrap();
        writer.start_file("index.html", options).unwrap();
        writer.write_all(html).unwrap();
        let archive = writer.finish().unwrap().into_inner();
        std::fs::write(&archive_path, &archive).unwrap();
        let files = json!([
            {"path":"lingxi-app.json","sizeBytes":manifest_bytes.len(),"sha256":sha256(&manifest_bytes),"executable":false},
            {"path":"index.html","sizeBytes":html.len(),"sha256":sha256(html),"executable":false}
        ]);
        let dependencies = json!([]);
        let archive_sha = sha256(&archive);
        let digest = market_release_digest(&archive_sha, &manifest, &files, &dependencies).unwrap();
        let request = json!({"op":"importRemixSource","requestId":request_id,"archivePath":archive_path,
            "archiveSha256":archive_sha,"archiveBytes":archive.len(),"manifest":manifest,"files":files,
            "dependencies":dependencies,"marketOrigin":{"appId":"market-app","releaseId":"v1","digest":digest}});
        let destination = root.path().join("source");
        (root, config, destination, request)
    }

    #[test]
    fn market_canonical_json_matches_javascript_numbers_and_nulls() {
        let value = json!({"z":null,"one":1.0,"zero":-0.0,"a":[true,3.5],
            "big":9007199254740993u64,"\u{e000}":1,"\u{1f600}":2});
        let mut result = String::new();
        canonical_json(&value, &mut result).unwrap();
        assert_eq!(result, "{\"a\":[true,3.5],\"big\":9007199254740992,\"one\":1,\"z\":null,\"zero\":0,\"😀\":2,\"\u{e000}\":1}");
    }

    #[test]
    fn market_remix_archive_verifies_extracts_and_rechecks_source() {
        let (_root, config, destination, request) = request_fixture();
        let input = MarketRemixInput::parse(&request, &config).unwrap();
        input.extract(&config, &destination).unwrap();
        input.verify_source(&destination).unwrap();
        assert_eq!(
            std::fs::read(destination.join("index.html")).unwrap(),
            b"<!doctype html><html>market</html>"
        );
        std::fs::write(destination.join("index.html"), b"changed").unwrap();
        assert!(input.verify_source(&destination).is_err());
    }

    #[test]
    fn market_remix_requires_original_archive_and_rejects_unsigned_changes() {
        let (_root, config, destination, request) = request_fixture();
        let input = MarketRemixInput::parse(&request, &config).unwrap();
        let mut changed = request.clone();
        changed["manifest"]["name"] = json!("tampered");
        assert!(MarketRemixInput::parse(&changed, &config).is_err());
        changed = request.clone();
        changed["archivePath"] = json!(config.join("arbitrary.zip"));
        assert!(MarketRemixInput::parse(&changed, &config).is_err());
        std::fs::remove_file(request["archivePath"].as_str().unwrap()).unwrap();
        assert!(matches!(
            input.extract(&config, &destination),
            Err(AppError::ArchiveMissing)
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn market_remix_rejects_signed_archive_with_traversal_or_extra_entries() {
        let (root, config, destination, mut request) = request_fixture();
        let path = PathBuf::from(request["archivePath"].as_str().unwrap());
        let original = std::fs::read(&path).unwrap();
        let mut reader = zip::ZipArchive::new(Cursor::new(original)).unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o600);
        for index in 0..reader.len() {
            let mut entry = reader.by_index(index).unwrap();
            let name = std::str::from_utf8(entry.name_raw()).unwrap().to_owned();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            writer.start_file(name, options).unwrap();
            writer.write_all(&bytes).unwrap();
        }
        writer.start_file("../escape", options).unwrap();
        writer.write_all(b"bad").unwrap();
        let archive = writer.finish().unwrap().into_inner();
        std::fs::write(&path, &archive).unwrap();
        let hash = sha256(&archive);
        request["archiveSha256"] = json!(hash);
        request["archiveBytes"] = json!(archive.len());
        request["marketOrigin"]["digest"] = json!(market_release_digest(
            request["archiveSha256"].as_str().unwrap(),
            &request["manifest"],
            &request["files"],
            &request["dependencies"]
        )
        .unwrap());
        let input = MarketRemixInput::parse(&request, &config).unwrap();
        assert!(input.extract(&config, &destination).is_err());
        assert!(!root.path().join("escape").exists());
    }

    #[cfg(unix)]
    #[test]
    fn market_remix_rejects_symlinked_staging_archive() {
        let (_root, config, destination, request) = request_fixture();
        let input = MarketRemixInput::parse(&request, &config).unwrap();
        let path = PathBuf::from(request["archivePath"].as_str().unwrap());
        let saved = path.with_extension("saved");
        std::fs::rename(&path, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &path).unwrap();
        assert!(input.extract(&config, &destination).is_err());
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn market_remix_does_not_report_a_broken_symlink_ancestor_as_missing() {
        let (_root, config, destination, request) = request_fixture();
        let input = MarketRemixInput::parse(&request, &config).unwrap();
        let path = PathBuf::from(request["archivePath"].as_str().unwrap());
        let parent = path.parent().unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(parent).unwrap();
        std::os::unix::fs::symlink(config.join("missing-target"), parent).unwrap();
        assert!(!matches!(
            input.extract(&config, &destination),
            Err(AppError::ArchiveMissing)
        ));
        assert!(!destination.exists());
    }
}
