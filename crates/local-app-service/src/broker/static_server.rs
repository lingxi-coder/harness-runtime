use super::LocalAppsHostBroker;
use super::RuntimeEntry;
use super::RuntimeEntryState;
use super::RuntimeHandle;
use super::RuntimePublicationCell;
use super::LOCAL_APP_CONTENT_SECURITY_POLICY;
use super::{PortLease, PortLeases};
use local_apps::AppRuntimeState;
use local_apps::AppService;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;
use tokio::time::sleep;
use tokio::time::Duration;

pub(super) const MAX_HTTP_REQUEST_BYTES: usize = 16 * 1024;

pub(super) const MAX_STATIC_ASSET_BYTES: u64 = 32 * 1024 * 1024;

pub(super) const STATIC_REQUEST_CONCURRENCY: usize = 8;

pub(super) const STATIC_ASSET_CHUNK_BYTES: usize = 256 * 1024;

pub(super) const STATIC_ACCEPT_RETRY: Duration = Duration::from_millis(50);

/// Consecutive `accept()` failures that retire the static server.  A burst of
/// ECONNABORTED/EMFILE must not, so the cap is deliberately generous
/// (100 * 50 ms ~= 5 s of an unbroken failure); a listener whose I/O driver is
/// gone fails EVERY poll and reaches it immediately.
pub(super) const STATIC_ACCEPT_ERROR_LIMIT: u32 = 100;

/// First port of the window an app's PERMANENT loopback port is drawn from, and
/// the window's length — see `bind_stable_loopback` for why it has to sit below
/// every shipped platform's ephemeral floor.
pub(super) const APP_PORT_WINDOW_FIRST: u16 = 20_000;

pub(super) const APP_PORT_WINDOW_LEN: u16 = 12_000;

/// Offset into the derived window an app's FIRST port candidate sits at.
///
/// Extracted so the collision regression probe uses the production derivation
/// rather than a copy of it — a copy would keep asserting itself after the
/// real derivation moved.
pub(super) fn derived_window_slot(app_id: &str) -> u16 {
    let hash = app_id.bytes().fold(0u32, |hash, byte| {
        hash.wrapping_mul(16_777_619) ^ u32::from(byte)
    });
    u16::try_from(hash % u32::from(APP_PORT_WINDOW_LEN)).unwrap_or(0)
}

/// Ports every OTHER app in this profile has already pinned, each paired with
/// its owner.
///
/// Read on demand rather than kept as a registry: the records ARE the
/// registry, and a cached copy would be one more thing to invalidate on
/// create/delete. The service returns the record/runtime pair from one
/// in-memory state-lock pass.
///
/// Twice per start in the ordinary case, not once — the snapshot the caller
/// reads before `bind_stable_loopback` cannot be trusted to still be true when
/// a candidate is leased, so the leased candidate is re-checked against a fresh
/// read.  Every result is a snapshot; only one taken while the port in question
/// is leased says anything durable about it.
pub(super) async fn sibling_pinned_ports(service: &AppService, app_id: &str) -> Vec<(String, u16)> {
    service.pinned_runtime_ports_except(app_id).await
}

/// Returns `None` on the requested shutdown, and `Some(detail)` when the
/// listener has stopped being usable at all — the caller must then retire the
/// runtime entry, because a static handle has nothing else to notice it.
pub(super) async fn run_static_server(
    listener: TcpListener,
    root: PathBuf,
    mut shutdown: oneshot::Receiver<()>,
) -> Option<String> {
    let mut consecutive_errors = 0u32;
    let request_slots = Arc::new(Semaphore::new(STATIC_REQUEST_CONCURRENCY));
    loop {
        let slot = tokio::select! {
            _ = &mut shutdown => return None,
            acquired = Arc::clone(&request_slots).acquire_owned() => match acquired {
                Ok(slot) => slot,
                Err(_) => return Some("static app server request limiter closed".into()),
            },
        };
        let accepted = tokio::select! {
            _ = &mut shutdown => return None,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                consecutive_errors = 0;
                let root = root.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let _ = serve_static_request(stream, &root).await;
                });
            }
            Err(error) => {
                drop(slot);
                // ECONNABORTED / EMFILE / EINTR describe ONE would-be
                // connection, not the listener, so a single error must not
                // retire the loop.  A listener whose runtime's I/O driver
                // was dropped fails EVERY poll though, and retrying that
                // forever is a permanent busy loop behind an entry that
                // still reports `running`.
                consecutive_errors += 1;
                if consecutive_errors >= STATIC_ACCEPT_ERROR_LIMIT {
                    return Some(format!(
                        "static app server stopped accepting connections after {consecutive_errors} consecutive failures: {error}"
                    ));
                }
                sleep(STATIC_ACCEPT_RETRY).await;
            }
        }
    }
}

/// Drop the entry this dead server owns and fail the record, so the next start
/// is legal instead of short-circuiting on a stale `Running`.
pub(super) async fn reconcile_static_runtime_exit(
    runtimes: Arc<Mutex<HashMap<String, RuntimeEntry>>>,
    service: Arc<AppService>,
    app_id: String,
    generation: u64,
    publication_cell: RuntimePublicationCell,
    detail: String,
    broker: std::sync::Weak<LocalAppsHostBroker>,
) {
    let removed = {
        let mut runtimes = runtimes.lock().await;
        let should_remove = runtimes.get(&app_id).is_some_and(|entry| {
            entry.generation == generation
                && matches!(
                    entry.state,
                    RuntimeEntryState::Running {
                        handle: RuntimeHandle::Static { .. }
                    }
                )
        });
        if should_remove {
            if let Ok(mut identity) = publication_cell.write() {
                *identity = None;
            }
            runtimes.remove(&app_id);
        }
        should_remove
    };
    if !removed {
        return;
    }
    // The listener retired on its own: reclaim what the runtime owned before
    // recording the stop, so a page that was mid-recording does not leave the
    // audio session held by nothing.
    if let Some(broker) = broker.upgrade() {
        broker
            .release_app_runtime_state(&app_id, Some(generation))
            .await;
    }
    if let Ok(record) = service.runtime_record(&app_id).await {
        let _ = service
            .update_runtime_record(
                &app_id,
                AppRuntimeState::Failed,
                record.port,
                record.pid,
                Some(detail),
            )
            .await;
    }
}

pub(super) async fn serve_static_request(
    mut stream: TcpStream,
    root: &Path,
) -> Result<(), std::io::Error> {
    let mut request = vec![0u8; MAX_HTTP_REQUEST_BYTES];
    let count = stream.read(&mut request).await?;
    request.truncate(count);
    let request_text = String::from_utf8_lossy(&request);
    let line = request_text.lines().next().unwrap_or_default().to_string();
    let if_none_match = request_text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("if-none-match"))
        })
        .map(|(_, value)| value.trim().to_string());
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let raw_path = parts.next().unwrap_or_default();
    if !matches!(method, "GET" | "HEAD") {
        return write_http(
            &mut stream,
            405,
            "text/plain",
            b"method not allowed",
            method == "HEAD",
        )
        .await;
    }
    let Some(relative) = safe_static_path(raw_path) else {
        return write_http(
            &mut stream,
            400,
            "text/plain",
            b"bad path",
            method == "HEAD",
        )
        .await;
    };
    let mut path = root.join(relative);
    if path.is_dir() {
        path.push("index.html");
    }
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            return write_http(
                &mut stream,
                404,
                "text/plain",
                b"not found",
                method == "HEAD",
            )
            .await;
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_STATIC_ASSET_BYTES => metadata,
        _ => {
            return write_http(
                &mut stream,
                404,
                "text/plain",
                b"not found",
                method == "HEAD",
            )
            .await;
        }
    };
    let served_relative = path.strip_prefix(root).unwrap_or(&path);
    let etag = static_etag(&path, &metadata);
    if if_none_match
        .as_deref()
        .is_some_and(|header| etag_matches(header, &etag))
    {
        return write_not_modified(&mut stream, &etag, static_cache_control(served_relative)).await;
    }
    write_static_file(
        &mut stream,
        file,
        metadata.len(),
        content_type(&path),
        static_cache_control(served_relative),
        &etag,
        method == "HEAD",
    )
    .await
}

pub(super) fn static_cache_control(path: &Path) -> &'static str {
    if is_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else if path.file_name().and_then(|name| name.to_str()) == Some("index.html") {
        "no-cache"
    } else {
        "no-store"
    }
}

pub(super) fn is_hashed_asset(path: &Path) -> bool {
    let mut components = path.components();
    if !matches!(
        components.next(),
        Some(std::path::Component::Normal(component)) if component == "assets"
    ) {
        return false;
    }
    let Some(file_name) = path.file_stem().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(hash) = file_name.rsplit('-').next() else {
        return false;
    };
    hash.len() >= 8 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) async fn write_static_file(
    stream: &mut TcpStream,
    mut file: tokio::fs::File,
    content_length: u64,
    content_type: &str,
    cache_control: &str,
    etag: &str,
    head: bool,
) -> Result<(), std::io::Error> {
    let header =
        static_response_header(200, content_type, content_length, cache_control, Some(etag));
    stream.write_all(header.as_bytes()).await?;
    if !head {
        let mut buffer = vec![0u8; STATIC_ASSET_CHUNK_BYTES];
        loop {
            let count = file.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            stream.write_all(&buffer[..count]).await?;
        }
    }
    stream.shutdown().await
}

pub(super) fn etag_matches(header: &str, current: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == current
    })
}

pub(super) async fn write_not_modified(
    stream: &mut TcpStream,
    etag: &str,
    cache_control: &str,
) -> Result<(), std::io::Error> {
    let header = static_response_header(304, "text/plain", 0, cache_control, Some(etag));
    stream.write_all(header.as_bytes()).await?;
    stream.shutdown().await
}

pub(super) fn safe_static_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.split(['?', '#']).next()?;
    if !raw.starts_with('/') || raw.contains('%') || raw.contains('\\') {
        return None;
    }
    let relative = raw.trim_start_matches('/');
    let relative = if relative.is_empty() {
        "index.html"
    } else {
        relative
    };
    let path = Path::new(relative);
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

pub(super) async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    head: bool,
) -> Result<(), std::io::Error> {
    let header = static_response_header(status, content_type, body.len() as u64, "no-store", None);
    stream.write_all(header.as_bytes()).await?;
    if !head {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

pub(super) fn static_response_header(
    status: u16,
    content_type: &str,
    content_length: u64,
    cache_control: &str,
    etag: Option<&str>,
) -> String {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let etag_header = etag
        .map(|value| format!("ETag: {value}\r\n"))
        .unwrap_or_default();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {content_length}\r\nContent-Type: {content_type}\r\nContent-Security-Policy: {LOCAL_APP_CONTENT_SECURITY_POLICY}\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nCache-Control: {cache_control}\r\n{etag_header}Connection: close\r\n\r\n"
    )
}

pub(super) fn static_etag(path: &Path, metadata: &std::fs::Metadata) -> String {
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let path_digest = Sha256::digest(path.to_string_lossy().as_bytes());
    format!("\"{}-{}-{:x}\"", modified_ns, metadata.len(), path_digest)
}

pub(super) fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("woff2") => "font/woff2",
        // `instantiateStreaming` REQUIRES this exact type and this server sends
        // `X-Content-Type-Options: nosniff`, so serving a .wasm as
        // application/octet-stream fails the streaming path outright — with a
        // MIME complaint that reads nothing like the CSP refusal it is not.
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Bind the app's loopback listener: the port it was already given if it has
/// one, otherwise a fresh port derived from its id.
///
/// WHY there is no fallback when `assigned` is taken.  An app's port is
/// PERMANENT by design one layer down: `AppState::set_runtime`
/// (`local-apps/src/state.rs`, "a port is NEVER reassigned — `IndexedDB` origin
/// stability") rejects any request that carries a different port than the one
/// already recorded, because the WebView loads the app from
/// `http://127.0.0.1:<port>` and every store the app owns client-side
/// (`IndexedDB`, `localStorage`, cookies, service-worker registration) is keyed
/// by that ORIGIN — a port that moved between starts would silently orphan the
/// app's own data.  Handing back a different port here would therefore not
/// rescue the start: `update_runtime_record` two calls later refuses the
/// reassignment and the start fails anyway, one message further along, with a
/// listener bound to a port nothing will ever use.
/// `the_pinned_app_port_can_never_be_reassigned` pins that invariant so this
/// reasoning goes red rather than stale if the pin is ever lifted.
///
/// WHY the window is 20000..32000.  A port that can never be reassigned must be
/// drawn from a range nobody else is ever GIVEN, so the window sits below every
/// shipped platform's ephemeral floor — Linux/Android
/// `net.ipv4.ip_local_port_range` starts at 32768, iOS/macOS
/// `net.inet.ip.portrange.first` at 49152.  Below those floors the kernel can
/// never hand an app's permanent port to some other process's socket while the
/// app is stopped.  The previous window (30000..50000) sat inside Android's
/// ephemeral range across 86% of its span and inside iOS's across its top 848
/// ports, which made the OS itself the likeliest squatter of a port that, by
/// design, the app can never give up.
///
/// WHY a sibling app's pin also excludes a candidate.  The window slot is a
/// hash of the app id, so two ids in ONE profile can derive the same slot —
/// `6b4cb242` and `c3baea9e` both derive 30809, and the birthday rate over
/// 12000 slots is ~1.3% at 20 apps.  A bind probe cannot see that collision:
/// a pin outlives the runtime that made it (`stop_runtime` re-passes
/// `runtime.port`), so a STOPPED sibling's permanent port probes free and
/// would be pinned a second time.  After that neither app can start while the
/// other runs — `set_runtime` refuses to move either — and on Android the two
/// share one `http://127.0.0.1:<port>` origin, hence one `localStorage` /
/// `IndexedDB` store, because the Android WebView is built on the default
/// profile with no per-app data store (iOS partitions by app id and is not
/// exposed to that half).  `sibling_pinned_ports` is therefore consulted
/// alongside the probe.  It PREVENTS new collisions only: a pair that already
/// collided is permanent on both sides, so the `assigned` branch can do
/// nothing but name the sibling instead of reporting a bare bind failure.
///
/// WHY a lease is taken as well.  A pin only reaches the records after the
/// choice — see [`PortLeases`] — so `sibling_pins` cannot describe a sibling
/// that is choosing right now.  Choice and reservation are therefore ONE step
/// here: the lease is taken before the probe, which is what makes a candidate
/// visible to every concurrent allocator from the instant it is picked.  The
/// returned guard belongs to the CALLER, which must hold it until the port is
/// persisted and then `commit` it.
///
/// WHY the pins are then read a SECOND time, from `service`, for the candidate
/// the lease was just taken on.  `sibling_pins` is a SNAPSHOT the caller read
/// before this call; a sibling can persist its pin and drop its lease in the
/// interval between that read and the take, and a candidate caught mid-hand-off
/// like that appears in neither half of "pins UNION leases" (see
/// [`PortLeases`]).  Re-reading after the take is what removes the interval:
/// `PortLease::commit` runs only after the persist, so once we hold the lease
/// on a port, a sibling that could have owned it either still holds its own
/// lease — and then our take returned `None` and we never got here — or has
/// already made its pin readable.  A sibling cannot newly take the port either,
/// because we hold it.  The re-read costs one `AppService` state lock per
/// candidate actually leased, which is one per start in the ordinary case.
///
/// The `assigned` branch takes no lease and needs no re-read: an assigned port
/// is by definition already persisted, so every sibling's pin snapshot has it.
pub(super) async fn bind_stable_loopback(
    app_id: &str,
    assigned: Option<u16>,
    sibling_pins: &[(String, u16)],
    leases: &PortLeases,
    service: &AppService,
) -> Result<(TcpListener, u16, Option<PortLease>), String> {
    if let Some(port) = assigned {
        return match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => Ok((listener, port, None)),
            Err(error) => Err(
                match sibling_pins.iter().find(|(_, pinned)| *pinned == port) {
                    Some((sibling, _)) => format!(
                        "stable app port {port} is unavailable: app {sibling} is pinned to the same port and is holding it. \
                         A pinned port can never be reassigned, so only one of the two apps can run; \
                         recreate one of them to mint a fresh port ({error})"
                    ),
                    None => format!("stable app port {port} is unavailable: {error}"),
                },
            ),
        };
    }
    let first = derived_window_slot(app_id);
    for offset in 0..256u16 {
        let port = APP_PORT_WINDOW_FIRST + (first + offset) % APP_PORT_WINDOW_LEN;
        // A sibling's pin outlives its runtime, so a candidate that binds
        // cleanly can still be owned forever by an app that is merely stopped.
        if sibling_pins.iter().any(|(_, pinned)| *pinned == port) {
            continue;
        }
        // Taken BEFORE the probe, so a sibling that reaches this candidate
        // finds it occupied even though nothing is persisted and — on the full
        // runtime, once the probe below is released — nothing is bound either.
        // The lock is dropped by `take` itself and never spans the await.
        let Some(lease) = PortLease::take(leases, app_id, port) else {
            continue;
        };
        // Now that the candidate cannot move again, ask the records once more.
        // `sibling_pins` was read before this call and a sibling's pin may have
        // landed since; because a lease outlives its own persist, holding this
        // one makes the answer stable rather than merely fresher.  Order is the
        // point — a re-read BEFORE the take would reproduce the same interval
        // it is here to remove.
        if sibling_pinned_ports(service, app_id)
            .await
            .iter()
            .any(|(_, pinned)| *pinned == port)
        {
            drop(lease);
            continue;
        }
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, port, Some(lease)));
        }
        // Refused by the kernel: hand the candidate straight back rather than
        // holding it for the rest of this scan.
        drop(lease);
    }
    Err("no stable loopback port is available for the app".into())
}
