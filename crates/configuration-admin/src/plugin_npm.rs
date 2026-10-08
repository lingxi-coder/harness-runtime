//! Registry-only npm plugin materialization. Claude Code 2.1.286 separates
//! package fetching from dependency installation: `npm install <spec>` can
//! resolve git dependencies even with `--ignore-scripts`.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256, Sha384, Sha512};

const MAX_ARCHIVE: usize = 256 * 1024 * 1024;
const MAX_UNPACKED: usize = 512 * 1024 * 1024;
const MAX_MANIFEST: usize = 32 * 1024 * 1024;
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
const DEPENDENCY_KEYS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

fn package_name(name: &str) -> bool {
    let segment = |s: &str| {
        s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if name.contains("..") {
        return false;
    }
    if let Some(scoped) = name.strip_prefix('@') {
        scoped
            .split_once('/')
            .is_some_and(|(scope, name)| segment(scope) && segment(name))
    } else {
        segment(name)
    }
}

// A registry version may be a semver range or dist-tag, never an npm exotic
// spec. Spaces are meaningful in ranges (>=1 <2 and 1.0.0 - 2.0.0).
fn registry_version(version: &str) -> bool {
    let lower = version.to_ascii_lowercase();
    if lower.ends_with(".tgz") || lower.ends_with(".tar.gz") || lower.ends_with(".tar") {
        return false;
    }
    if version.len() <= 128
        && version
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return true;
    }
    static RANGE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        let number = r"[vV]?(?:[0-9]+|[xX*])(?:\.(?:[0-9]+|[xX*])){0,2}(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?";
        regex::Regex::new(&format!(
            r"^(?:(?:[<>=~^]{{1,2}}\s*)?{number})(?:\s+(?:[<>=~^]{{1,2}}\s*)?{number})*$"
        ))
        .unwrap()
    });
    static ENDPOINT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"^[vV]?(?:[0-9]+|[xX*])(?:\.(?:[0-9]+|[xX*])){0,2}(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$",
        ).unwrap()
    });
    version.split("||").all(|part| {
        let part = part.trim();
        if let Some((lower, upper)) = part.split_once(" - ") {
            // npm ranges allow partial hyphen endpoints (1.2 - 2.3), not
            // only exact semver::Version values with three components.
            return ENDPOINT.is_match(lower.trim()) && ENDPOINT.is_match(upper.trim());
        }
        RANGE.is_match(part)
    })
}

fn split_package_spec<'a>(
    package: &'a str,
    version: Option<&'a str>,
) -> Result<(&'a str, &'a str), String> {
    let (name, inline) = package[package.starts_with('@') as usize..]
        .find('@')
        .map(|offset| offset + usize::from(package.starts_with('@')))
        .map(|at| (&package[..at], Some(&package[at + 1..])))
        .unwrap_or((package, None));
    if !package_name(name) || (inline.is_some() && version.is_some_and(|v| !v.is_empty())) {
        return Err(format!(
            "invalid npm plugin source: {package}. An npm plugin source must name a registry package (name or name@version) or link to a tarball file. For a plugin in a git repository, use a \"github\", \"url\" or \"git-subdir\" source."
        ));
    }
    let version = version
        .filter(|v| !v.is_empty())
        .or(inline)
        .unwrap_or("latest");
    if !registry_version(version) {
        return Err(format!(
            "{name} was not resolved: \"{version}\" is not a version, a semver range or a dist-tag"
        ));
    }
    Ok((name, version))
}

fn download_url(url: &str, http_origin: Option<&str>) -> Result<reqwest::Url, String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| "is not an http or https link".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("is not an http or https link".to_string());
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '#' | '\\'))
    {
        return Err("contains a \"#\", whitespace, a backslash or a control character".to_string());
    }
    let host = parsed.host_str().unwrap_or_default().trim_end_matches('.');
    let gitlab_api = host == "gitlab.com"
        && parsed.path().starts_with("/api/v4/")
        && parsed.path().contains("/-/");
    if [
        "github.com",
        "gist.github.com",
        "gitlab.com",
        "bitbucket.org",
        "git.sr.ht",
    ]
    .contains(&host)
        && !gitlab_api
    {
        return Err("is on GitHub, GitLab, Bitbucket or SourceHut, where npm may fetch it as a git repository and run its setup script".to_string());
    }
    if parsed.path().starts_with("//") {
        return Err("starts its path with \"//\"".to_string());
    }
    if parsed.scheme() == "http"
        && http_origin != Some(parsed.origin().ascii_serialization().as_str())
    {
        return Err("is an unencrypted http link on a host other than your npm registry, so npm could send your saved registry token to it in the clear".to_string());
    }
    Ok(parsed)
}

fn validate_download(
    url: &str,
    registry: Option<&str>,
    http_origin: Option<&str>,
) -> Result<(), String> {
    let parsed = download_url(url, http_origin)?;
    // npm may rewrite tarball URLs to the registry override. Validate that
    // interpretation too, before npm sees the address.
    if let Some(registry) = registry {
        let moved = reqwest::Url::parse(registry)
            .and_then(|base| base.join(parsed.path()))
            .map_err(|_| "the npm registry set for it is not a valid address".to_string())?;
        download_url(moved.as_str(), http_origin)?;
    }
    Ok(())
}

trait NpmRunner {
    fn run(&mut self, cwd: &Path, args: &[String], timeout: Duration) -> Result<String, String>;
}
struct SystemNpm;
impl NpmRunner for SystemNpm {
    fn run(&mut self, cwd: &Path, args: &[String], timeout: Duration) -> Result<String, String> {
        // No .npmrc or package.json from the unpacked package is present here.
        let mut child = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
            .arg(format!("--git={}", cwd.join("git-is-disabled").display()))
            .args(args)
            .current_dir(cwd)
            .env("npm_config_ignore_scripts", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start npm for plugin source: {e}"))?;
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let read_output = |mut stream: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut saved = Vec::new();
                let mut buf = [0; 8192];
                loop {
                    let count = stream.read(&mut buf)?;
                    if count == 0 {
                        break;
                    }
                    let keep = count.min((MAX_OUTPUT + 1).saturating_sub(saved.len()));
                    saved.extend_from_slice(&buf[..keep]);
                }
                Ok::<_, std::io::Error>(saved)
            })
        };
        let stdout = read_output(Box::new(stdout));
        let stderr = read_output(Box::new(stderr));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(match result {
                        Err(error) => format!("failed to wait for npm: {error}"),
                        _ => format!(
                            "npm plugin source operation timed out after {}s",
                            timeout.as_secs()
                        ),
                    });
                }
            }
        };
        let stdout = stdout
            .join()
            .map_err(|_| "npm stdout reader failed")?
            .map_err(|e| e.to_string())?;
        let stderr = stderr
            .join()
            .map_err(|_| "npm stderr reader failed")?
            .map_err(|e| e.to_string())?;
        if stdout.len() > MAX_OUTPUT || stderr.len() > MAX_OUTPUT {
            return Err("npm output exceeds 8 MiB".to_string());
        }
        if !status.success() {
            return Err(format!(
                "failed to install npm plugin source: {}",
                String::from_utf8_lossy(&stderr)
                    .trim()
                    .chars()
                    .take(500)
                    .collect::<String>()
            ));
        }
        String::from_utf8(stdout).map_err(|_| "npm output is not UTF-8".to_string())
    }
}

fn registry_args(registry: Option<&str>, name: Option<&str>) -> Vec<String> {
    let Some(registry) = registry else {
        return Vec::new();
    };
    let mut args = vec!["--registry".to_string(), registry.to_string()];
    if let Some(scope) = name
        .and_then(|name| name.strip_prefix('@'))
        .and_then(|name| name.split_once('/'))
        .map(|(scope, _)| scope)
    {
        args.push(format!("--@{scope}:registry={registry}"));
    }
    args
}

pub(crate) fn materialize(
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    root: &Path,
) -> Result<PathBuf, String> {
    materialize_with(package, version, registry, root, &mut SystemNpm)
}

fn materialize_with(
    package: &str,
    version: Option<&str>,
    registry: Option<&str>,
    root: &Path,
    npm: &mut impl NpmRunner,
) -> Result<PathBuf, String> {
    let direct_url = package.starts_with("https://") || package.starts_with("http://");
    let spec = if direct_url && version.is_none() {
        None
    } else {
        Some(split_package_spec(package, version)?)
    };
    let download = root.join("npm-download");
    fs::create_dir_all(&download).map_err(|e| e.to_string())?;
    let needs_http =
        package.starts_with("http:") || registry.is_some_and(|r| r.starts_with("http:"));
    let mut http_origin = if needs_http {
        npm.run(
            &download,
            &[
                "config".into(),
                "get".into(),
                "registry".into(),
                "--workspaces=false".into(),
            ],
            Duration::from_secs(30),
        )
        .ok()
        .and_then(|s| reqwest::Url::parse(s.trim()).ok())
        .filter(|u| u.scheme() == "http")
        .map(|u| u.origin().ascii_serialization())
    } else {
        None
    };
    if let Some(registry) = registry {
        download_url(registry, http_origin.as_deref())?;
    }
    let resolution = if let Some((name, version)) = spec {
        let mut args = vec!["view".into(), "--json".into(), "--workspaces=false".into()];
        args.extend(registry_args(registry, Some(name)));
        args.extend([
            "--".into(),
            format!("{name}@{version}"),
            "name".into(),
            "version".into(),
            "dist.tarball".into(),
            "dist.integrity".into(),
            "dist.shasum".into(),
            "dist-tags.latest".into(),
        ]);
        let result = npm.run(&download, &args, Duration::from_secs(60))?;
        let mut value: Value =
            serde_json::from_str(&result).map_err(|_| "npm view returned invalid JSON")?;
        if let Some(values) = value.as_array_mut() {
            values.sort_by_key(|v| {
                v["version"]
                    .as_str()
                    .and_then(|v| semver::Version::parse(v).ok())
            });
            // npm view already filters the range; prefer its latest dist-tag
            // when that version is among the matches, as upstream urr does.
            let latest = values
                .first()
                .and_then(|v| v["dist-tags.latest"].as_str())
                .map(str::to_string);
            value = latest
                .and_then(|latest| {
                    values
                        .iter()
                        .find(|v| v["version"].as_str() == Some(latest.as_str()))
                        .cloned()
                })
                .or_else(|| values.pop())
                .unwrap_or(Value::Null);
        }
        Some(value)
    } else {
        None
    };
    let tarball = resolution
        .as_ref()
        .map(|r| {
            r["dist.tarball"]
                .as_str()
                .ok_or("npm view returned no tarball")
        })
        .transpose()?
        .unwrap_or(package);
    if tarball.starts_with("http:") && !needs_http {
        http_origin = npm
            .run(
                &download,
                &[
                    "config".into(),
                    "get".into(),
                    "registry".into(),
                    "--workspaces=false".into(),
                ],
                Duration::from_secs(30),
            )
            .ok()
            .and_then(|s| reqwest::Url::parse(s.trim()).ok())
            .filter(|url| url.scheme() == "http")
            .map(|url| url.origin().ascii_serialization());
    }
    validate_download(tarball, registry, http_origin.as_deref())?;
    let mut args = vec![
        "pack".into(),
        "--ignore-scripts".into(),
        "--json".into(),
        "--loglevel=error".into(),
        "--workspaces=false".into(),
    ];
    args.extend(registry_args(registry, None));
    args.extend(["--".into(), tarball.into()]);
    let packed: Value =
        serde_json::from_str(&npm.run(&download, &args, Duration::from_secs(300))?)
            .map_err(|_| "npm pack returned invalid JSON")?;
    let filename = packed[0]["filename"]
        .as_str()
        .filter(|name| {
            !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)))
                && !name.contains(['/', '\\', ':'])
        })
        .ok_or("npm pack returned no safe tarball filename")?;
    let bytes = read_regular(&download.join(filename), MAX_ARCHIVE)?;
    if let Some(resolution) = &resolution {
        verify_integrity(&bytes, resolution)?;
    }
    let target = root.join("package");
    unpack_tarball(&bytes, &target)?;
    // A plugin without a lockfile has no automatic dependency install, just
    // as upstream Rue does. Never let npm resolve the package's live manifest.
    if let Err(error) = install_dependencies(&target, root, npm) {
        tracing::warn!(%error, "Skipped installing this plugin's dependencies");
    }
    Ok(target)
}

fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() > limit as u64 {
        return Err(format!(
            "{} is not a regular file of at most {} MB",
            path.display(),
            limit / 1024 / 1024
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("npm file exceeds size limit".to_string());
    }
    Ok(bytes)
}

fn verify_integrity(bytes: &[u8], resolution: &Value) -> Result<(), String> {
    if let Some(integrity) = resolution["dist.integrity"].as_str() {
        let matches: Vec<_> = integrity.split_whitespace().collect();
        for algorithm in ["sha512", "sha384", "sha256", "sha1"] {
            if !matches
                .iter()
                .any(|m| m.starts_with(&format!("{algorithm}-")))
            {
                continue;
            }
            let digest = match algorithm {
                "sha512" => Sha512::digest(bytes).to_vec(),
                "sha384" => Sha384::digest(bytes).to_vec(),
                "sha256" => Sha256::digest(bytes).to_vec(),
                _ => sha1::Sha1::digest(bytes).to_vec(),
            };
            let expected = format!(
                "{algorithm}-{}",
                base64::engine::general_purpose::STANDARD.encode(digest)
            );
            return matches
                .contains(&expected.as_str())
                .then_some(())
                .ok_or_else(|| {
                    "the downloaded tarball does not match the integrity the registry reported"
                        .to_string()
                });
        }
        return Err("the registry reported an unsupported integrity hash".to_string());
    }
    if let Some(shasum) = resolution["dist.shasum"].as_str() {
        if format!("{:x}", sha1::Sha1::digest(bytes)) != shasum.to_ascii_lowercase() {
            return Err(
                "the downloaded tarball does not match the shasum the registry reported"
                    .to_string(),
            );
        }
    }
    Ok(())
}

fn tar_number(bytes: &[u8]) -> Result<usize, String> {
    if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        if bytes[0] & 0x40 != 0 {
            return Err("tar entry declares a negative size".to_string());
        }
        return bytes[1..]
            .iter()
            .try_fold(usize::from(bytes[0] & 0x7f), |n, b| {
                n.checked_mul(256)
                    .and_then(|n| n.checked_add(usize::from(*b)))
                    .ok_or_else(|| "tar number overflows".to_string())
            });
    }
    let raw = std::str::from_utf8(bytes)
        .map_err(|_| "invalid tar number")?
        .trim_matches(['\0', ' ']);
    if raw.is_empty() {
        Ok(0)
    } else {
        usize::from_str_radix(raw, 8).map_err(|_| "invalid tar number".to_string())
    }
}
fn tar_string(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes.split(|b| *b == 0).next().unwrap_or_default())
        .map_err(|_| "tar path is not UTF-8".to_string())
}
fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', ':'])
        && !path.split('/').any(|p| p == "..")
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn pax_path(bytes: &[u8]) -> Result<Option<String>, String> {
    let mut rest = bytes;
    let mut path = None;
    while !rest.is_empty() {
        let split = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or("invalid tar PAX record")?;
        let len: usize = std::str::from_utf8(&rest[..split])
            .map_err(|_| "invalid tar PAX record")?
            .parse()
            .map_err(|_| "invalid tar PAX record")?;
        if len <= split + 1 || len > rest.len() || rest[len - 1] != b'\n' {
            return Err("invalid tar PAX record".to_string());
        }
        if let Some(value) = rest[split + 1..len - 1].strip_prefix(b"path=") {
            path = Some(
                std::str::from_utf8(value)
                    .map_err(|_| "tar path is not UTF-8")?
                    .to_string(),
            );
        }
        rest = &rest[len..];
    }
    Ok(path)
}

fn unpack_tarball(bytes: &[u8], target: &Path) -> Result<(), String> {
    let mut archive = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .take(MAX_UNPACKED as u64 + 1)
        .read_to_end(&mut archive)
        .map_err(|_| "The npm package tarball is not valid gzip data and was not installed")?;
    if archive.len() > MAX_UNPACKED {
        return Err(
            "The npm package unpacks to more than 512 MB and was not installed".to_string(),
        );
    }
    let mut offset: usize = 0;
    let mut next_path = None;
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let checksum = header
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if (148..156).contains(&i) {
                    32
                } else {
                    usize::from(*b)
                }
            })
            .sum::<usize>();
        if checksum != tar_number(&header[148..156])? {
            return Err("The npm package was not installed: a header checksum does not match (corrupt tarball)".to_string());
        }
        let size = tar_number(&header[124..136])?;
        if size > MAX_ARCHIVE {
            return Err("npm tar entry is larger than 256 MB".to_string());
        }
        let start = offset + 512;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= archive.len())
            .ok_or("tar entry runs past the end of the archive (truncated tarball)")?;
        let data = &archive[start..end];
        offset = start + size.div_ceil(512) * 512;
        let kind = header[156];
        match kind {
            b'x' => {
                next_path = pax_path(data)?.or(next_path);
                continue;
            }
            b'L' => {
                next_path = Some(tar_string(data)?.to_string());
                continue;
            }
            b'g' | b'K' => continue,
            b'1' | b'2' => {
                return Err(
                    "The npm package was not installed: links are not allowed in a plugin package"
                        .to_string(),
                );
            }
            0 | b'0' | b'7' | b'5' => {}
            _ => {
                return Err(
                    "The npm package was not installed: it contains an entry of unsupported type"
                        .to_string(),
                );
            }
        }
        let name = tar_string(&header[..100])?;
        let prefix = if header[257..].starts_with(b"ustar") {
            tar_string(&header[345..500])?
        } else {
            ""
        };
        let name = next_path.take().unwrap_or_else(|| {
            if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            }
        });
        let name = name.strip_prefix("./").unwrap_or(&name);
        let Some((_, path)) = name.split_once('/') else {
            continue;
        };
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            continue;
        }
        if !safe_relative(path) {
            return Err("The npm package was not installed: it contains an entry outside the package folder".to_string());
        }
        if entries.len() >= 100_000 {
            return Err(
                "The npm package was not installed: it contains more than 100000 entries"
                    .to_string(),
            );
        }
        if !seen.insert(path.to_string()) {
            return Err(
                "The npm package was not installed: it contains the same path twice".to_string(),
            );
        }
        entries.push((
            path.to_string(),
            data,
            kind == b'5',
            tar_number(&header[100..108])?,
        ));
    }
    if !entries.iter().any(|(_, _, directory, _)| !directory) {
        return Err("The npm package was not installed: it contains no files".to_string());
    }
    // Validate every header, path and entry type before writing any file.
    fs::create_dir(target).map_err(|e| e.to_string())?;
    for (path, data, directory, mode) in entries {
        let path = target.join(path);
        if directory {
            fs::create_dir_all(path).map_err(|e| e.to_string())?;
            continue;
        }
        fs::create_dir_all(path.parent().ok_or("tar entry has no parent")?)
            .map_err(|e| e.to_string())?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                format!(
                    "The npm package was not installed: cannot create {}: {e}",
                    path.display()
                )
            })?;
        file.write_all(data).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        if mode & 0o111 != 0 {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode((mode & 0o755) as u32))
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    Ok(())
}

fn dependency_spec(spec: &str) -> bool {
    if let Some(alias) = spec.strip_prefix("npm:") {
        return alias
            .rsplit_once('@')
            .is_some_and(|(name, version)| package_name(name) && registry_version(version));
    }
    registry_version(spec)
}
fn dependencies(value: &Value, keys: &[&str], label: &str) -> Result<Map<String, Value>, String> {
    let mut result = Map::new();
    for key in keys {
        let Some(deps) = value.get(*key) else {
            continue;
        };
        let deps = deps
            .as_object()
            .ok_or_else(|| format!("{label} has an unreadable {key} list"))?;
        for (name, spec) in deps {
            if !package_name(name) || !spec.as_str().is_some_and(dependency_spec) {
                return Err(format!(
                    "{label} has a dependency that is not a registry package and version range: {name}"
                ));
            }
        }
        result.insert((*key).to_string(), Value::Object(deps.clone()));
    }
    Ok(result)
}
fn normalize_optional(deps: &mut Map<String, Value>) {
    let optional: Vec<_> = deps
        .get("optionalDependencies")
        .and_then(Value::as_object)
        .map(|deps| deps.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(deps) = deps.get_mut("dependencies").and_then(Value::as_object_mut) {
        for name in optional {
            deps.remove(&name);
        }
    }
}
fn validate_bin(value: &Value) -> bool {
    let safe_bin = |mut path: &str| {
        while let Some(relative) = path.strip_prefix("./") {
            path = relative;
        }
        safe_relative(path)
    };
    if let Some(path) = value.as_str() {
        return safe_bin(path);
    }
    value.as_object().is_some_and(|bins| {
        bins.iter().all(|(name, path)| {
            !name.is_empty()
                && name != "."
                && name != ".."
                && !name.contains(['/', '\\', ':'])
                && path.as_str().is_some_and(safe_bin)
        })
    })
}
fn integrity_valid(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        !s.is_empty()
            && s.split(' ').all(|hash| {
                hash.split_once('-').is_some_and(|(kind, data)| {
                    ["sha512", "sha384", "sha256", "sha1"].contains(&kind)
                        && !data.is_empty()
                        && base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .is_ok()
                })
            })
    })
}
fn peer_meta(value: &Value) -> Option<Value> {
    value.as_object().map(|meta| {
        Value::Object(
            meta.iter()
                .filter(|(name, value)| package_name(name) && value.is_object())
                .map(|(name, value)| (name.clone(), json!({"optional": value["optional"] == true})))
                .collect(),
        )
    })
}

fn sanitized_lock(
    manifest: &Value,
    lock: &Value,
    label: &str,
    http_origin: Option<&str>,
) -> Result<(Value, Value), String> {
    if !manifest.is_object() || !lock.is_object() {
        return Err("package.json and lockfile must be JSON objects".to_string());
    }
    if !matches!(lock["lockfileVersion"].as_u64(), Some(2 | 3)) {
        return Err(format!(
            "its {label} is in an old format; regenerate it with npm 7 or later"
        ));
    }
    if manifest.get("overrides").is_some() {
        return Err(
            "its package.json sets overrides, which this install does not apply".to_string(),
        );
    }
    let packages = lock["packages"]
        .as_object()
        .filter(|packages| packages.get("").is_some_and(Value::is_object))
        .ok_or_else(|| format!("its {label} has no packages section with the plugin in it"))?;
    let root = dependencies(&packages[""], &DEPENDENCY_KEYS, label)?;
    let mut declared = dependencies(manifest, &DEPENDENCY_KEYS, "its package.json")?;
    normalize_optional(&mut declared);
    for key in DEPENDENCY_KEYS {
        let a = root.get(key).and_then(Value::as_object);
        let b = declared.get(key).and_then(Value::as_object);
        if a.cloned().unwrap_or_default() != b.cloned().unwrap_or_default() {
            return Err(format!(
                "its package.json and {label} list different dependencies"
            ));
        }
    }
    let mut package = root.clone();
    package.insert("name".into(), json!("plugin-dependencies"));
    package.insert("version".into(), json!("0.0.0"));
    if let Some(meta) = peer_meta(&manifest["peerDependenciesMeta"]) {
        package.insert("peerDependenciesMeta".into(), meta);
    }
    let package = Value::Object(package);
    let mut sanitized = Map::new();
    sanitized.insert("".into(), package.clone());
    for (path, value) in packages {
        if path.is_empty() {
            continue;
        }
        let label = format!("its {label} entry \"{path}\"");
        if !path
            .strip_prefix("node_modules/")
            .is_some_and(|p| p.split("/node_modules/").all(package_name))
        {
            return Err(format!("{label} is not a package in node_modules"));
        }
        if !value.is_object() {
            return Err(format!("{label} is not an object"));
        }
        if value.get("link").is_some() || value.get("inBundle").is_some() {
            return Err(format!("{label} is a folder link or a bundled copy"));
        }
        if !value["version"]
            .as_str()
            .is_some_and(|v| semver::Version::parse(v).is_ok())
        {
            return Err(format!("{label} has no exact version"));
        }
        let mut entry = dependencies(
            value,
            &["dependencies", "optionalDependencies", "peerDependencies"],
            &label,
        )?;
        entry.insert("version".into(), value["version"].clone());
        if let Some(name) = value.get("name") {
            if !name.as_str().is_some_and(package_name) {
                return Err(format!(
                    "{label} has a name that is not an npm package name"
                ));
            }
            entry.insert("name".into(), name.clone());
        }
        if let Some(resolved) = value.get("resolved") {
            let url = resolved
                .as_str()
                .ok_or_else(|| format!("{label} has a download link that is not text"))?;
            download_url(url, http_origin)
                .map_err(|reason| format!("{label} has a download link that {reason}"))?;
            if value.get("integrity").is_none() {
                return Err(format!(
                    "{label} has no integrity hash for its download link"
                ));
            }
            entry.insert("resolved".into(), resolved.clone());
        }
        if let Some(integrity) = value.get("integrity") {
            if !integrity_valid(integrity) {
                return Err(format!("{label} has an unreadable integrity hash"));
            }
            entry.insert("integrity".into(), integrity.clone());
        }
        for flag in ["dev", "optional", "devOptional", "peer"] {
            if value[flag] == true {
                entry.insert(flag.into(), json!(true));
            }
        }
        for key in ["os", "cpu", "libc"] {
            if let Some(list) = value.get(key) {
                if !list
                    .as_array()
                    .is_some_and(|list| list.iter().all(Value::is_string))
                {
                    return Err(format!("{label} has an unreadable {key} list"));
                }
                entry.insert(key.into(), list.clone());
            }
        }
        if let Some(bin) = value.get("bin") {
            if !validate_bin(bin) {
                return Err(format!(
                    "{label} has a bin link that points outside its package or has an invalid command name"
                ));
            }
            entry.insert("bin".into(), bin.clone());
        }
        if let Some(meta) = peer_meta(&value["peerDependenciesMeta"]) {
            entry.insert("peerDependenciesMeta".into(), meta);
        }
        sanitized.insert(path.clone(), Value::Object(entry));
    }
    let lock = json!({"name":"plugin-dependencies", "version":"0.0.0", "lockfileVersion":3,"requires":true,"packages":sanitized});
    Ok((package, lock))
}

fn install_dependencies(
    target: &Path,
    root: &Path,
    npm: &mut impl NpmRunner,
) -> Result<(), String> {
    if !target.join("package.json").exists() {
        return Ok(());
    }
    // bun lockfiles are not forwarded unchecked to another installer. A text
    // bun-lock reader can be added independently; npm is the supported lane.
    let Some(filename) = ["npm-shrinkwrap.json", "package-lock.json"]
        .into_iter()
        .find(|f| target.join(f).exists())
    else {
        return Ok(());
    };
    let manifest: Value =
        serde_json::from_slice(&read_regular(&target.join("package.json"), MAX_MANIFEST)?)
            .map_err(|_| "its package.json is not valid JSON")?;
    let raw_lock = read_regular(&target.join(filename), MAX_MANIFEST)?;
    let lock: Value = serde_json::from_slice(&raw_lock)
        .map_err(|_| format!("its {filename} is not valid JSON"))?;
    let stage = root.join("npm-dependencies");
    fs::create_dir(&stage).map_err(|e| e.to_string())?;
    let http_origin = if raw_lock.windows(7).any(|bytes| bytes == b"http://") {
        npm.run(
            &stage,
            &[
                "config".into(),
                "get".into(),
                "registry".into(),
                "--workspaces=false".into(),
            ],
            Duration::from_secs(30),
        )
        .ok()
        .and_then(|s| reqwest::Url::parse(s.trim()).ok())
        .filter(|u| u.scheme() == "http")
        .map(|u| u.origin().ascii_serialization())
    } else {
        None
    };
    let (package, lock) = sanitized_lock(&manifest, &lock, filename, http_origin.as_deref())?;
    if lock["packages"].as_object().is_some_and(|p| p.len() <= 1) {
        return Ok(());
    }
    fs::write(
        stage.join("package.json"),
        serde_json::to_vec(&package).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::write(
        stage.join("package-lock.json"),
        serde_json::to_vec(&lock).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    npm.run(
        &stage,
        &[
            "ci",
            "--ignore-scripts",
            "--workspaces=false",
            "--no-audit",
            "--no-fund",
        ]
        .map(str::to_string),
        Duration::from_secs(60),
    )?;
    let modules = stage.join("node_modules");
    if modules.exists() {
        let existing = target.join("node_modules");
        if existing.exists() {
            fs::remove_dir_all(&existing).map_err(|e| e.to_string())?;
        }
        fs::rename(modules, existing).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_tar(entries: &[(&str, &[u8], u8)]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, bytes, kind) in entries {
            let mut header = [0; 512];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", bytes.len()).as_bytes());
            header[148..156].fill(b' ');
            header[156] = *kind;
            header[257..263].copy_from_slice(b"ustar\0");
            let sum: usize = header.iter().map(|b| usize::from(*b)).sum();
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            tar.extend_from_slice(&header);
            tar.extend_from_slice(bytes);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.resize(tar.len() + 1024, 0);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar).unwrap();
        gz.finish().unwrap()
    }
    fn digest(bytes: &[u8]) -> String {
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
        )
    }
    fn dependency_fixture() -> (Value, Value) {
        let manifest = json!({"name":"plugin", "version":"1.0.0", "dependencies":{"dep":"^1.0.0"}, "scripts":{"install":"touch should-not-run"}});
        let lock = json!({"lockfileVersion":3, "packages":{
            "":{"dependencies":{"dep":"^1.0.0"}},
            "node_modules/dep":{"version":"1.0.0", "resolved":"https://registry.npmjs.org/dep/-/dep-1.0.0.tgz", "integrity":digest(b"dependency"), "hasInstallScript":true}
        }});
        (manifest, lock)
    }

    #[test]
    fn npm_source_rejects_git_folder_option_and_exotic_version_specs() {
        for invalid in [
            "owner/repo",
            "owner/repo/subdir",
            "github:owner/repo",
            "git+https://example.com/repo",
            "../plugin",
            "/tmp/plugin",
            "file:plugin",
            "--foreground-scripts",
            "@scope/../escape",
            "pkg@file:../escape",
            "pkg@https://example.com/a.tgz",
            "pkg@",
            "pkg@.",
            "pkg@..",
            "pkg@~",
            "pkg@~folder",
            "@scope/foo..bar",
        ] {
            assert!(
                split_package_spec(invalid, None).is_err(),
                "accepted {invalid}"
            );
        }
        assert_eq!(
            split_package_spec("@scope/plugin@1.2.3", None).unwrap(),
            ("@scope/plugin", "1.2.3")
        );
        assert_eq!(
            split_package_spec("plugin", Some(">=1.0.0 <2.0.0")).unwrap(),
            ("plugin", ">=1.0.0 <2.0.0")
        );
        assert!(split_package_spec("plugin@1", Some("2")).is_err());
    }

    #[test]
    fn npm_partial_hyphen_ranges_and_relative_bins_remain_installable() {
        for range in ["1 - 2", "1.2 - 2.3", "1.2.3 - 2.3", "1.2 - 2.3.4"] {
            assert!(split_package_spec("plugin", Some(range)).is_ok(), "{range}");
            let (mut manifest, mut lock) = dependency_fixture();
            manifest["dependencies"]["dep"] = json!(range);
            lock["packages"][""]["dependencies"]["dep"] = json!(range);
            lock["packages"]["node_modules/dep"]["bin"] = json!({"dep": "./bin/dep.js"});
            assert!(
                sanitized_lock(&manifest, &lock, "package-lock.json", None).is_ok(),
                "{range}"
            );
        }
        assert!(validate_bin(&json!("./bin/run.js")));
        for invalid in ["./../outside", "./", "././", "./C:/outside", ".//outside"] {
            assert!(!validate_bin(&json!(invalid)), "{invalid}");
        }
    }

    #[test]
    fn npm_download_refuses_git_hosts_and_registry_rewrites() {
        for bad in [
            "https://github.com/org/repo",
            "https://gist.github.com/id",
            "https://gitlab.com/org/repo",
            "https://bitbucket.org/a/b",
            "https://git.sr.ht/~u/r",
            "https://cdn.example.com/a.tgz#fragment",
            "https://cdn.example.com//a.tgz",
            "https://cdn.example.com/a\\b",
            "http://cdn.example.com/a.tgz",
        ] {
            assert!(
                validate_download(bad, None, None).is_err(),
                "accepted {bad}"
            );
        }
        assert!(validate_download(
            "https://cdn.example.com/a.tgz",
            Some("https://github.com"),
            None
        )
        .is_err());
        assert!(validate_download("https://cdn.example.com/a.tgz", None, None).is_ok());
        assert!(validate_download(
            "https://gitlab.com/api/v4/projects/1/packages/npm/@scope/pkg/-/pkg.tgz",
            None,
            None
        )
        .is_ok());
        assert!(validate_download(
            "http://registry.example.com/a.tgz",
            None,
            Some("http://registry.example.com")
        )
        .is_ok());
    }

    #[test]
    fn npm_integrity_uses_strongest_available_hash() {
        let good_sha1 = format!(
            "sha1-{}",
            base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(b"data"))
        );
        assert!(verify_integrity(b"data", &json!({"dist.integrity":digest(b"data")})).is_ok());
        let mixed = json!({"dist.integrity": format!("{good_sha1} {}", digest(b"wrong"))});
        assert!(verify_integrity(b"data", &mixed).is_err());
        let shasum = json!({"dist.shasum": format!("{:x}", sha1::Sha1::digest(b"data"))});
        assert!(verify_integrity(b"data", &shasum).is_ok());
    }

    #[test]
    fn npm_archive_rejects_traversal_links_and_duplicates_before_writing() {
        for entries in [
            vec![
                ("package/good", b"ok".as_slice(), b'0'),
                ("package/../../escape", b"bad".as_slice(), b'0'),
            ],
            vec![("package/link", b"".as_slice(), b'2')],
            vec![("package/link", b"".as_slice(), b'1')],
            vec![
                ("package/name", b"one".as_slice(), b'0'),
                ("package/name", b"two".as_slice(), b'0'),
            ],
            vec![("package/device", b"".as_slice(), b'3')],
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let target = tmp.path().join("package");
            assert!(unpack_tarball(&fixture_tar(&entries), &target).is_err());
            assert!(!target.exists(), "an invalid archive wrote files");
        }
    }

    #[test]
    fn npm_archive_handles_pax_and_rejects_pax_traversal() {
        for (name, valid) in [("package/nested/data", true), ("package/../escape", false)] {
            let payload = format!("path={name}\n");
            let mut len = payload.len() + 2;
            loop {
                let adjusted = payload.len() + len.to_string().len() + 1;
                if adjusted == len {
                    break;
                }
                len = adjusted;
            }
            let record = format!("{len} {payload}");
            let bytes = fixture_tar(&[
                ("pax", record.as_bytes(), b'x'),
                ("package/placeholder", b"data", b'0'),
            ]);
            let tmp = tempfile::tempdir().unwrap();
            let target = tmp.path().join("package");
            assert_eq!(unpack_tarball(&bytes, &target).is_ok(), valid);
            if valid {
                assert_eq!(fs::read(target.join("nested/data")).unwrap(), b"data");
            }
        }
    }

    #[test]
    fn npm_dependencies_reject_non_registry_edges_and_unsafe_lock_entries() {
        for spec in [
            "git+https://example.com/repo",
            "owner/repo",
            "file:../dependency",
            "https://cdn.example.com/a.tgz",
            "workspace:*",
        ] {
            let (mut manifest, mut lock) = dependency_fixture();
            manifest["dependencies"]["dep"] = json!(spec);
            lock["packages"][""]["dependencies"]["dep"] = json!(spec);
            assert!(
                sanitized_lock(&manifest, &lock, "package-lock.json", None).is_err(),
                "accepted {spec}"
            );
        }
        for (key, value) in [
            ("resolved", json!("https://github.com/owner/repo")),
            ("resolved", json!("file:../dep")),
            ("link", json!(false)),
            ("inBundle", json!(true)),
            ("bin", json!({"dep":"../../outside"})),
            (
                "dependencies",
                json!({"nested":"git+https://example.com/repo"}),
            ),
        ] {
            let (manifest, mut lock) = dependency_fixture();
            lock["packages"]["node_modules/dep"][key] = value;
            assert!(
                sanitized_lock(&manifest, &lock, "package-lock.json", None).is_err(),
                "accepted {key}"
            );
        }
        let (manifest, mut lock) = dependency_fixture();
        lock["packages"]["node_modules/dep/../../escape"] = json!({"version":"1.0.0"});
        assert!(sanitized_lock(&manifest, &lock, "package-lock.json", None).is_err());
    }

    #[test]
    fn npm_dependencies_keep_registry_aliases_and_strip_install_metadata() {
        let (mut manifest, mut lock) = dependency_fixture();
        manifest["dependencies"]["dep"] = json!("npm:@scope/real@^1.0.0");
        lock["packages"][""]["dependencies"]["dep"] = manifest["dependencies"]["dep"].clone();
        let (clean_manifest, clean_lock) =
            sanitized_lock(&manifest, &lock, "package-lock.json", None).unwrap();
        assert_eq!(clean_manifest["name"], "plugin-dependencies");
        assert!(clean_manifest.get("scripts").is_none());
        assert!(clean_lock["packages"]["node_modules/dep"]
            .get("hasInstallScript")
            .is_none());
        assert_eq!(
            clean_manifest["dependencies"]["dep"],
            "npm:@scope/real@^1.0.0"
        );
    }

    struct FixtureNpm {
        archive: Vec<u8>,
        calls: Vec<Vec<String>>,
        registry_tarball: String,
        wrong_integrity: bool,
    }
    impl NpmRunner for FixtureNpm {
        fn run(&mut self, cwd: &Path, args: &[String], _: Duration) -> Result<String, String> {
            self.calls.push(args.to_vec());
            match args[0].as_str() {
                "view" => Ok(json!({"name":"plugin", "version":"1.0.0", "dist.tarball":self.registry_tarball, "dist.integrity": if self.wrong_integrity { digest(b"wrong") } else { digest(&self.archive) }}).to_string()),
                "pack" => {
                    assert!(args.iter().any(|a| a == "--ignore-scripts"));
                    fs::write(cwd.join("plugin.tgz"), &self.archive).unwrap();
                    Ok(json!([{"filename":"plugin.tgz"}]).to_string())
                },
                "ci" => {
                    let manifest: Value = serde_json::from_slice(&fs::read(cwd.join("package.json")).unwrap()).unwrap();
                    assert_eq!(manifest["name"], "plugin-dependencies");
                    assert!(manifest.get("scripts").is_none());
                    assert!(!cwd.join(".npmrc").exists());
                    fs::create_dir_all(cwd.join("node_modules/dep")).unwrap();
                    fs::write(cwd.join("node_modules/dep/index.js"), "module.exports = 1;").unwrap();
                    Ok(String::new())
                },
                other => panic!("unexpected npm command {other}"),
            }
        }
    }
    fn runner(archive: Vec<u8>) -> FixtureNpm {
        FixtureNpm {
            archive,
            calls: Vec::new(),
            registry_tarball: "https://registry.npmjs.org/plugin/-/plugin-1.0.0.tgz".into(),
            wrong_integrity: false,
        }
    }

    #[test]
    fn npm_materialization_packs_and_installs_only_sanitized_locked_dependencies() {
        let (manifest, lock) = dependency_fixture();
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let lock = serde_json::to_vec(&lock).unwrap();
        let mut npm = runner(fixture_tar(&[
            ("package/package.json", &manifest, b'0'),
            ("package/package-lock.json", &lock, b'0'),
            (
                "package/.npmrc",
                b"registry=https://untrusted.invalid",
                b'0',
            ),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let target = materialize_with(
            "@scope/plugin",
            Some("1.0.0"),
            Some("https://registry.example.com"),
            tmp.path(),
            &mut npm,
        )
        .unwrap();
        assert_eq!(
            npm.calls.iter().map(|c| c[0].as_str()).collect::<Vec<_>>(),
            ["view", "pack", "ci"]
        );
        assert!(npm.calls[0]
            .iter()
            .any(|a| a == "--@scope:registry=https://registry.example.com"));
        assert!(target.join("node_modules/dep/index.js").is_file());
        assert!(!target.join("should-not-run").exists());
    }

    #[test]
    fn npm_materialization_refuses_before_fetch_and_skips_malicious_dependencies() {
        let tmp = tempfile::tempdir().unwrap();
        let mut npm = runner(Vec::new());
        assert!(materialize_with("owner/repo", None, None, tmp.path(), &mut npm).is_err());
        assert!(npm.calls.is_empty());
        npm.registry_tarball = "https://github.com/owner/repo".into();
        assert!(materialize_with("plugin", None, None, tmp.path(), &mut npm).is_err());
        assert_eq!(npm.calls.len(), 1);
        let (mut manifest, mut lock) = dependency_fixture();
        manifest["dependencies"]["dep"] = json!("github:owner/repo");
        lock["packages"][""]["dependencies"]["dep"] = json!("github:owner/repo");
        let manifest = serde_json::to_vec(&manifest).unwrap();
        let lock = serde_json::to_vec(&lock).unwrap();
        let mut npm = runner(fixture_tar(&[
            ("package/package.json", &manifest, b'0'),
            ("package/package-lock.json", &lock, b'0'),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let target = materialize_with("plugin", None, None, tmp.path(), &mut npm).unwrap();
        assert_eq!(
            npm.calls.iter().map(|c| c[0].as_str()).collect::<Vec<_>>(),
            ["view", "pack"]
        );
        assert!(!target.join("node_modules").exists());
    }

    #[test]
    fn npm_materialization_integrity_failure_never_unpacks() {
        let mut npm = runner(fixture_tar(&[("package/package.json", b"{}", b'0')]));
        npm.wrong_integrity = true;
        let tmp = tempfile::tempdir().unwrap();
        assert!(materialize_with("plugin", None, None, tmp.path(), &mut npm)
            .unwrap_err()
            .contains("integrity"));
        assert!(!tmp.path().join("package").exists());
    }
}
