//! Bounded plugin archive downloads shared by native hosts and CLI startup.
use futures::StreamExt;
use std::path::{Path, PathBuf};
const MAX_DOWNLOAD_BYTES: usize = 64 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn same_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str() == right.host_str()
        && left.port_or_known_default() == right.port_or_known_default()
}

/// Oracle `Zqt`/`PTt` (§15): *"Archive URLs must use https:// and must not
/// point at a loopback, link-local, or cloud-metadata host."* Applied to
/// every plugin-archive / `--plugin-url` download AND to every hop of a
/// redirect — an attacker-controlled HTTPS origin can otherwise 30x an
/// initially-valid URL onto `169.254.169.254` or similar, which the scheme
/// and cross-origin-redirect checks alone do not catch (this port's plugin
/// downloads run with cross-origin redirects allowed, so that check is a
/// no-op for this attack).
fn is_denied_download_host(host: &str) -> bool {
    // Oracle `PTt` opens with
    // `let t=e.toLowerCase().replace(/^\[|\]$/g,""); if(t.endsWith("."))t=t.slice(0,-1);`
    // — the trailing-dot strip is its SECOND operation, precisely so the
    // fully-qualified spellings `localhost.` / `sub.localhost.` cannot walk
    // past the denylist. (`Url::host_str` normalises `127.0.0.1.` back to
    // `127.0.0.1` via the WHATWG IPv4 parser, so only the NAME arm leaks
    // without it.) An empty host is denied too (`t===""`).
    //
    // `Url::host_str` returns an IPv6 literal WITH its brackets (`"[::1]"`);
    // strip them before handing the bare address to `IpAddr::parse`.
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let host = host.strip_suffix('.').unwrap_or(host);
    let lower = host.to_ascii_lowercase();
    if lower.is_empty() || lower == "localhost" || lower.ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .is_ok_and(is_denied_download_ip)
}

fn is_denied_download_ip(ip: std::net::IpAddr) -> bool {
    use std::net::{IpAddr, Ipv6Addr};
    match ip {
        IpAddr::V4(v4) => is_denied_download_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_denied_download_ipv4(mapped);
            }
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || v6 == "fd00:ec2::254".parse::<Ipv6Addr>().unwrap() // AWS IMDSv2 link-local alias
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10
        }
    }
}

fn is_denied_download_ipv4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 127 // 127.0.0.0/8
        || (o[0] == 169 && o[1] == 254) // 169.254.0.0/16
        || o[0] == 0 // 0.0.0.0/8
        || v4 == std::net::Ipv4Addr::new(100, 100, 100, 200) // Alibaba Cloud metadata
}

fn reject_denylisted_download_url(url: &reqwest::Url) -> Result<(), String> {
    if let Some(host) = url.host_str() {
        if is_denied_download_host(host) {
            return Err(format!(
                "refusing to download `{url}`: archive URLs must use https:// and must not point \
                 at a loopback, link-local, or cloud-metadata host (got `{host}`)"
            ));
        }
    }
    Ok(())
}

/// Same check, bypassed under `cfg(test)` — tests exercise the download path
/// against a plaintext `127.0.0.1` server (see the scheme bypass alongside
/// this one), which the loopback denylist would otherwise trip on every run.
fn reject_denylisted_download_url_unless_test(url: &reqwest::Url) -> Result<(), String> {
    if cfg!(test) {
        return Ok(());
    }
    reject_denylisted_download_url(url)
}

pub fn download_client(allow_cross_origin_redirects: bool) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() != "https" && !cfg!(test) {
                attempt.error("refusing redirect to a non-HTTPS URL")
            } else if let Err(reason) = reject_denylisted_download_url_unless_test(attempt.url()) {
                attempt.error(reason)
            } else if !allow_cross_origin_redirects
                && attempt
                    .previous()
                    .first()
                    .is_some_and(|initial| !same_origin(initial, attempt.url()))
            {
                // reqwest strips Authorization/Cookie on cross-origin redirects,
                // but `x-api-key` is an application header and would otherwise
                // be forwarded to the redirect target.
                attempt.error("credentialed download refuses a cross-origin redirect")
            } else {
                attempt.follow()
            }
        }))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|e| format!("initialise download client: {e}"))
}

pub async fn bounded_get(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
) -> Result<Vec<u8>, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL `{url}`: {e}"))?;
    if parsed.scheme() != "https" && !cfg!(test) {
        return Err(format!("refusing non-HTTPS URL `{url}`"));
    }
    reject_denylisted_download_url_unless_test(&parsed)?;
    let mut request = client.get(parsed);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("download `{url}`: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "download `{url}` returned HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_DOWNLOAD_BYTES as u64)
    {
        return Err(format!("download `{url}` exceeds 64 MiB"));
    }
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("download `{url}`: {e}"))?;
        if out.len().saturating_add(chunk.len()) > MAX_DOWNLOAD_BYTES {
            return Err(format!("download `{url}` exceeds 64 MiB"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

pub async fn download_plugin_urls(urls: &[String], root: &Path) -> Result<Vec<PathBuf>, String> {
    let client = download_client(true)?;
    let mut paths = Vec::with_capacity(urls.len());
    for (index, url) in urls.iter().enumerate() {
        let bytes = bounded_get(&client, url, &[]).await?;
        let extracted = root.join(format!("plugin-{index}"));
        // 0700 + clear-before-unpack (2.1.269): another local user must not be
        // able to read a plugin extracted for this session, and no file from a
        // previous extraction may survive into the new one.
        plugin::prepare_extract_dir(&extracted)
            .map_err(|e| format!("create plugin extraction directory: {e}"))?;
        plugin::unpack_plugin_archive(&bytes, &extracted)
            .map_err(|e| format!("invalid plugin archive `{url}`: {e}"))?;
        let plugin_root = unwrap_plugin_root(&extracted)?;
        plugin::ensure_plugin_manifest(&plugin_root)
            .map_err(|e| format!("invalid plugin manifest `{url}`: {e}"))?;
        paths.push(plugin_root);
    }
    Ok(paths)
}

/// Download and unpack the oracle `archive` plugin-entry source: an HTTPS
/// zip, optionally pinned by `sha256` — *"verified against every download and
/// the install is refused on mismatch"*.
///
/// `dest` is CLEARED before unpacking, so an existing directory is replaced
/// rather than merged into (2.1.269). Callers stage into a fresh path anyway;
/// the clear is what stops a file from a previous extraction outliving the
/// archive that no longer ships it.
pub async fn download_plugin_archive(
    url: &str,
    sha256: Option<&str>,
    dest: &Path,
) -> Result<PathBuf, String> {
    let client = download_client(true)?;
    let bytes = bounded_get(&client, url, &[]).await?;
    if let Some(expected) = sha256 {
        let actual = plugin::plugin_source_sha256(&bytes);
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(format!(
                "invalid plugin archive `{url}`: sha256 verification failed (expected {expected}, got {actual})"
            ));
        }
    }
    // Same 2.1.269 hardening as `download_plugin_urls`. `create_dir_all` alone
    // left stale files from a prior extraction in place — an archive that no
    // longer ships a file could not remove the old copy, so a downgrade kept
    // executing the newer version's code.
    plugin::prepare_extract_dir(dest)
        .map_err(|e| format!("create plugin extraction directory: {e}"))?;
    plugin::unpack_plugin_archive(&bytes, dest)
        .map_err(|e| format!("invalid plugin archive `{url}`: {e}"))?;
    // §22: an archive that unpacked cleanly but held nothing but macOS zip
    // cruft (a bare `__MACOSX/` sibling, or an archive with zero entries) is
    // its own distinct failure — the oracle's exact copy names the URL and
    // tells the author what to fix, rather than falling through to
    // `ensure_plugin_manifest`'s generic "manifest invalid" error below.
    if !archive_has_plugin_files(dest) {
        return Err(format!(
            "Plugin archive from {url} contained no plugin files. The archive was not \
             installed. Verify the URL serves a zip of the plugin contents."
        ));
    }
    let plugin_root = unwrap_plugin_root(dest)?;
    plugin::ensure_plugin_manifest(&plugin_root)
        .map_err(|e| format!("invalid plugin manifest `{url}`: {e}"))?;
    Ok(plugin_root)
}

/// The oracle's own filter for "did this archive extract anything besides
/// macOS zip cruft" (`!B.startsWith("__MACOSX/") && bc(B)!==".DS_Store"`,
/// applied to every extracted entry). Recurses through the whole tree — a
/// legitimate `__MACOSX/` sibling can appear at any depth, not just the
/// archive root.
fn archive_has_plugin_files(dir: &Path) -> bool {
    fn walk(dir: &Path, at_root: bool, found: &mut bool) {
        if *found {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if *found {
                return;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if at_root && name == "__MACOSX" {
                continue;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if is_dir {
                walk(&entry.path(), false, found);
            } else if name != ".DS_Store" {
                *found = true;
            }
        }
    }
    let mut found = false;
    walk(dir, true, &mut found);
    found
}

fn has_plugin_manifest(root: &Path) -> bool {
    root.join(branding::PLUGIN_MANIFEST_DIR).is_dir() || root.join("manifest.json").is_file()
}

fn unwrap_plugin_root(root: &Path) -> Result<PathBuf, String> {
    if has_plugin_manifest(root) {
        return Ok(root.to_path_buf());
    }
    let mut entries = std::fs::read_dir(root)
        .map_err(|e| format!("inspect extracted plugin `{}`: {e}", root.display()))?;
    let Some(entry) = entries
        .next()
        .transpose()
        .map_err(|e| format!("inspect extracted plugin `{}`: {e}", root.display()))?
    else {
        return Ok(root.to_path_buf());
    };
    if entries.next().is_none()
        && entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
        && has_plugin_manifest(&entry.path())
    {
        Ok(entry.path())
    } else {
        Ok(root.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §15 — oracle `Zqt`/`PTt`: an https archive URL naming a loopback,
    /// link-local, or cloud-metadata host must be refused before the download
    /// is even attempted (`bounded_get` is the shared path for both
    /// `--plugin-url` and the marketplace `archive` source's fetch).
    #[test]
    fn denylisted_hosts_are_refused() {
        for denied in [
            "https://127.0.0.1/x",
            "https://127.255.255.255/x",
            "https://169.254.169.254/latest/meta-data/",
            "https://0.0.0.0/x",
            "https://100.100.100.200/x", // Alibaba Cloud metadata
            "https://localhost/x",
            "https://sub.localhost/x",
            // Oracle `PTt` strips a trailing dot before the name arms — the
            // fully-qualified spelling resolves to 127.0.0.1 just the same.
            "https://localhost./x",
            "https://localhost.:8080/evil.zip",
            "https://sub.localhost./x",
            "https://[::1]/x",
            "https://[::]/x",
            "https://[fd00:ec2::254]/x", // AWS IMDSv2 IPv6 alias
            "https://[fe80::1]/x",
            "https://[::ffff:127.0.0.1]/x",   // IPv4-mapped loopback
            "https://[::ffff:169.254.1.1]/x", // IPv4-mapped link-local
        ] {
            let url = reqwest::Url::parse(denied).unwrap();
            assert!(
                reject_denylisted_download_url(&url).is_err(),
                "expected `{denied}` to be denylisted"
            );
        }
    }

    /// An ordinary public host must NOT be denylisted.
    #[test]
    fn ordinary_hosts_are_not_denylisted() {
        for allowed in [
            "https://github.com/x",
            "https://objects.githubusercontent.com/x",
            "https://8.8.8.8/x",
            "https://[2001:4860:4860::8888]/x",
        ] {
            let url = reqwest::Url::parse(allowed).unwrap();
            assert!(
                reject_denylisted_download_url(&url).is_ok(),
                "expected `{allowed}` to be allowed"
            );
        }
    }

    #[tokio::test]
    async fn credentialed_download_rejects_cross_origin_redirect_without_leaking_key() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let redirect_target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = redirect_target.local_addr().unwrap();
        let redirect_source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_addr = redirect_source.local_addr().unwrap();
        let source_task = tokio::spawn(async move {
            let (mut stream, _) = redirect_source.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });

        let client = download_client(false).unwrap();
        let result = bounded_get(
            &client,
            &format!("http://{source_addr}/file"),
            &[("x-api-key".to_string(), "must-not-leak".to_string())],
        )
        .await;
        source_task.await.unwrap();
        let error = result.expect_err("cross-origin redirect must fail");
        assert!(
            error.contains("redirect"),
            "expected redirect failure, got: {error}"
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                redirect_target.accept()
            )
            .await
            .is_err(),
            "redirect target must not receive the credentialed request"
        );
    }

    #[test]
    fn session_plugin_normalizes_root_mcpb_manifest() {
        let extracted = tempfile::tempdir().unwrap();
        let bundle = extracted.path().join("bundle");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::write(
            bundle.join("manifest.json"),
            r#"{"name":"session-plugin","version":"1.2.3"}"#,
        )
        .unwrap();

        let root = unwrap_plugin_root(extracted.path()).unwrap();
        assert_eq!(root, bundle);
        plugin::ensure_plugin_manifest(&root).unwrap();
        let normalized =
            std::fs::read_to_string(root.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"))
                .unwrap();
        assert!(normalized.contains("session-plugin"));
    }

    fn build_plugin_zip() -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            writer
                .start_file(
                    format!("{}/plugin.json", branding::PLUGIN_MANIFEST_DIR),
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer
                .write_all(br#"{"name":"archive-demo","version":"1.0.0"}"#)
                .unwrap();
            writer.finish().unwrap();
        }
        buf
    }

    async fn respond_once(listener: tokio::net::TcpListener, body: Vec<u8>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
    }

    /// The `archive` plugin-entry source (oracle: *"verified against every
    /// download and the install is refused on mismatch"*) — a wrong `sha256`
    /// must refuse the install before anything is unpacked.
    #[tokio::test]
    async fn archive_source_rejects_sha256_mismatch() {
        let body = b"not a zip; the sha check must fire before unpacking".to_vec();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(respond_once(listener, body));
        let dest = tempfile::tempdir().unwrap();
        let target = dest.path().join("out");

        let wrong_sha256 = "0".repeat(64);
        let result = download_plugin_archive(
            &format!("http://{addr}/plugin.zip"),
            Some(&wrong_sha256),
            &target,
        )
        .await;
        server.await.unwrap();

        let error = result.expect_err("a sha256 mismatch must be refused");
        assert!(
            error.contains("sha256 verification failed"),
            "expected a sha256 verification failure, got: {error}"
        );
        assert!(!target.exists(), "must not unpack before verification");
    }

    /// A matching `sha256` unpacks the archive and resolves the plugin root.
    #[tokio::test]
    async fn archive_source_unpacks_on_sha256_match() {
        let zip_bytes = build_plugin_zip();
        let expected_sha256 = plugin::plugin_source_sha256(&zip_bytes);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(respond_once(listener, zip_bytes));
        let dest = tempfile::tempdir().unwrap();
        let target = dest.path().join("out");

        let plugin_root = download_plugin_archive(
            &format!("http://{addr}/plugin.zip"),
            Some(&expected_sha256),
            &target,
        )
        .await
        .unwrap();
        server.await.unwrap();

        let manifest = std::fs::read_to_string(
            plugin_root
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
        )
        .unwrap();
        assert!(manifest.contains("archive-demo"));
    }

    // --- §22: "contained no plugin files" ------------------------------

    /// A zip with zero entries at all.
    fn build_empty_zip() -> Vec<u8> {
        let mut buf = Vec::new();
        zip::ZipWriter::new(std::io::Cursor::new(&mut buf))
            .finish()
            .unwrap();
        buf
    }

    /// A zip holding only the macOS Archive Utility's cruft — a `__MACOSX/`
    /// resource-fork sibling and a stray `.DS_Store` — nothing an
    /// `ensure_plugin_manifest` scan should ever treat as real content.
    fn build_macosx_only_zip() -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            writer
                .start_file(
                    "__MACOSX/._plugin.json",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.write_all(b"resource fork junk").unwrap();
            writer
                .start_file(".DS_Store", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"finder junk").unwrap();
            writer.finish().unwrap();
        }
        buf
    }

    #[test]
    fn archive_has_plugin_files_is_true_for_a_real_plugin() {
        let dest = tempfile::tempdir().unwrap();
        plugin::unpack_plugin_archive(&build_plugin_zip(), dest.path()).unwrap();
        assert!(archive_has_plugin_files(dest.path()));
    }

    #[test]
    fn archive_has_plugin_files_is_false_for_an_empty_archive() {
        let dest = tempfile::tempdir().unwrap();
        plugin::unpack_plugin_archive(&build_empty_zip(), dest.path()).unwrap();
        assert!(!archive_has_plugin_files(dest.path()));
    }

    #[test]
    fn archive_has_plugin_files_is_false_for_macosx_cruft_only() {
        let dest = tempfile::tempdir().unwrap();
        plugin::unpack_plugin_archive(&build_macosx_only_zip(), dest.path()).unwrap();
        assert!(!archive_has_plugin_files(dest.path()));
    }

    /// End-to-end: downloading an archive with no real content is refused
    /// with the oracle's exact copy, naming the URL, before
    /// `ensure_plugin_manifest`'s generic "invalid plugin manifest" error
    /// ever gets a chance to fire.
    #[tokio::test]
    async fn download_plugin_archive_rejects_a_content_free_zip_with_the_oracle_copy() {
        let zip_bytes = build_macosx_only_zip();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(respond_once(listener, zip_bytes));
        let dest = tempfile::tempdir().unwrap();
        let target = dest.path().join("out");
        let url = format!("http://{addr}/plugin.zip");

        let error = download_plugin_archive(&url, None, &target)
            .await
            .expect_err("a content-free archive must be refused");
        server.await.unwrap();

        assert_eq!(
            error,
            format!(
                "Plugin archive from {url} contained no plugin files. The archive was not \
                 installed. Verify the URL serves a zip of the plugin contents."
            )
        );
    }
}
