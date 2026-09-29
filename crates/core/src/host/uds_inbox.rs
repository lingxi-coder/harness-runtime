//! Cross-session UDS inbox (claude-code 2.1.232 `zid` / `gum` / `B4v`).
//!
//! Wire: one JSON object per line. Optional first line `{type:"auth",token}`.
//! User payload `{msgV:1,msg_id,type:"user",message:{role:"user",content},priority,from}`.
//! Unix `requireAuth` is off (`gMo()` is Windows-only). Socket path matches
//! `$XDG_RUNTIME_DIR/cc-socks/<pid>.sock` (103-byte cap, else `/tmp/cc-socks-<uid>/`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::host::live_sessions::{
    extract_cross_session_inner, inbound_decision, parse_from_name, parse_wrap_attr,
    peer_message_reminder, process_live_dir, process_name, process_permission_class,
    process_session_id, sessions_root, wrap_cross_session_message_with_mode, InboundPolicy,
    PeerMessage,
};

const MSG_V: u32 = 1;
const MAX_LINE: usize = 1_048_576;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MACOS_LINGER: Duration = Duration::from_millis(150);
const SOCK_PATH_MAX: usize = 103;
const SOCK_DIR: &str = "cc-socks";
const SEEN_MESSAGE_LIMIT: usize = 2_048;

/// One parked inbound message (2.1.232 `Uy.inbound.held`).
#[derive(Debug, Clone)]
pub struct HeldPeer {
    /// Wire `msg_id`.
    pub id: String,
    /// Sender display name (`from-name` / live session), never a raw `uds:` path.
    pub from: String,
    /// Body preview (inner wrap text).
    pub preview: String,
    /// `g6f` hold cause.
    pub hold_cause: String,
    /// Whether the TUI has already opened a dialog for this item.
    pub announced: bool,
}

/// Process inbox: accepted queue + hold buffer.
struct InboxState {
    accepted: Mutex<VecDeque<PeerMessage>>,
    held: Mutex<Vec<HeldEntry>>,
    receipts: Mutex<VecDeque<String>>,
    seen_message_ids: Mutex<VecDeque<String>>,
    path: Mutex<Option<PathBuf>>,
    peer_token: Mutex<Option<String>>,
    child_token: Mutex<Option<String>>,
    /// Canonical destination this listener generation belongs to.
    bound_session_id: Option<String>,
    /// Set after a hot remount. Legacy unscoped UDS payloads are then refused
    /// so a sender paused after resolving A cannot enter newly mounted B.
    require_session_fence: bool,
    stop: AtomicBool,
}

impl InboxState {
    fn new(bound_session_id: Option<String>, require_session_fence: bool) -> Self {
        Self {
            accepted: Mutex::new(VecDeque::new()),
            held: Mutex::new(Vec::new()),
            receipts: Mutex::new(VecDeque::new()),
            seen_message_ids: Mutex::new(VecDeque::new()),
            path: Mutex::new(None),
            peer_token: Mutex::new(None),
            child_token: Mutex::new(None),
            bound_session_id,
            require_session_fence,
            stop: AtomicBool::new(false),
        }
    }
}

#[derive(Debug, Clone)]
struct HeldEntry {
    msg: PeerMessage,
    hold_cause: String,
    announced: bool,
    quiet_until: Option<Instant>,
}

struct InboxRuntime {
    state: Arc<InboxState>,
    join: Option<JoinHandle<()>>,
}

static RUNTIME: Mutex<Option<InboxRuntime>> = Mutex::new(None);
/// Stopped generations whose file-inbox spill failed. They are intentionally
/// kept out of `RUNTIME`, so a newly mounted session can never drain their
/// accepted/held messages as its own; a later lifecycle drain retries them.
static RETIRED_GENERATIONS: Mutex<Vec<Arc<InboxState>>> = Mutex::new(Vec::new());

fn inbox() -> Arc<InboxState> {
    let mut g = RUNTIME.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rt) = g.as_ref() {
        return rt.state.clone();
    }
    let state = Arc::new(InboxState::new(process_session_id(), false));
    *g = Some(InboxRuntime {
        state: state.clone(),
        join: None,
    });
    state
}

fn install_runtime(state: Arc<InboxState>, join: JoinHandle<()>) {
    let mut g = RUNTIME.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(InboxRuntime {
        state,
        join: Some(join),
    });
}

/// Oracle `mum()` socket path for `pid`.
#[must_use]
pub fn default_socket_path(pid: u32) -> PathBuf {
    if let Some(runtime) = safe_runtime_dir() {
        let primary = runtime.join(SOCK_DIR).join(format!("{pid}.sock"));
        if primary.as_os_str().len() <= SOCK_PATH_MAX {
            return primary;
        }
    }
    PathBuf::from("/tmp")
        .join(format!("{SOCK_DIR}-{}", uid()))
        .join(format!("{pid}.sock"))
}

fn safe_runtime_dir() -> Option<PathBuf> {
    for key in ["XDG_RUNTIME_DIR", "CLAUDE_CODE_TMPDIR", "LINGXI_TMPDIR"] {
        let Some(raw) = std::env::var_os(key) else {
            continue;
        };
        let path = PathBuf::from(raw);
        if path.as_os_str().is_empty() {
            continue;
        }
        if is_world_writable_tmp(&path) {
            continue;
        }
        if path.exists() && !dir_owned_private(&path) {
            continue;
        }
        return Some(path);
    }
    None
}

fn is_world_writable_tmp(path: &Path) -> bool {
    path == Path::new("/tmp") || path == Path::new("/var/tmp") || path == Path::new("/private/tmp")
}

fn uid() -> u32 {
    #[cfg(unix)]
    {
        rustix::process::getuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// This process's listening path, if the inbox is up.
#[must_use]
pub fn process_socket_path() -> Option<PathBuf> {
    inbox()
        .path
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// `uds:` address for this process (2.1.232 `nCr`).
#[must_use]
pub fn process_uds_address() -> Option<String> {
    process_socket_path().map(|p| uds_address(&p))
}

/// `uds:` + percent-encode of the socket path (`nCr` / `m5u`).
#[must_use]
pub fn uds_address(path: &Path) -> String {
    format!("uds:{}", encode_sock(path))
}

fn encode_sock(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::new();
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b':' | b'_' | b'/' | b'.' | b'\\' | b'-' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Inverse of [`uds_address`] (`uds:` + percent-decode).
#[must_use]
pub fn decode_uds_address(addr: &str) -> Option<PathBuf> {
    let rest = addr.strip_prefix("uds:")?;
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `*.sock` whose parent is private to this user.
#[must_use]
pub fn is_inbox_sock_path(path: &Path) -> bool {
    if path.extension().and_then(|s| s.to_str()) != Some("sock") {
        return false;
    }
    if !path.is_absolute() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    dir_owned_private(parent)
}

/// Official `cc-socks` / `cc-socks-<uid>` inbox location.
#[must_use]
pub fn is_canonical_inbox_sock(path: &Path) -> bool {
    if !is_inbox_sock_path(path) {
        return false;
    }
    let Some(parent) = path.parent().and_then(|p| p.file_name()) else {
        return false;
    };
    let name = parent.to_string_lossy();
    is_cc_socks_dir_name(&name)
}

fn is_cc_socks_dir_name(name: &str) -> bool {
    if name == SOCK_DIR {
        return true;
    }
    let Some(rest) = name.strip_prefix(&format!("{SOCK_DIR}-")) else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

fn dir_owned_private(dir: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::symlink_metadata(dir) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
        if !meta.is_dir() {
            return false;
        }
        if meta.uid() != uid() {
            return false;
        }
        meta.mode() & 0o022 == 0
    }
    #[cfg(not(unix))]
    {
        dir.is_dir()
    }
}

/// Wire user payload (2.1.232 `oTn` / `zid`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdsUserPayload {
    /// Protocol version (`msgV` = 1).
    #[serde(rename = "msgV")]
    pub msg_v: u32,
    /// UUID.
    pub msg_id: String,
    /// `"user"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Nested `{role,content}`.
    pub message: UdsUserMessage,
    /// `next` / `now` / `later`.
    pub priority: String,
    /// `uds:<socket>` of the sender.
    pub from: String,
    /// Destination session captured from the peer's socket key. Optional only
    /// for wire compatibility with pre-fence senders on an initial mount.
    #[serde(
        rename = "toSessionId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub to_session_id: Option<String>,
}

/// Nested user message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UdsUserMessage {
    /// `"user"`.
    pub role: String,
    /// Already-wrapped `<cross-session-message>` body.
    pub content: String,
}

/// Build the send payload.
#[must_use]
pub fn user_payload(
    from_name: &str,
    from_sid: &str,
    from_addr: &str,
    body: &str,
) -> UdsUserPayload {
    let content = wrap_cross_session_message_with_mode(
        from_name,
        from_sid,
        Some(from_name),
        process_permission_class().as_deref(),
        body,
    );
    UdsUserPayload {
        msg_v: MSG_V,
        msg_id: uuid::Uuid::new_v4().to_string(),
        kind: "user".into(),
        message: UdsUserMessage {
            role: "user".into(),
            content,
        },
        priority: "next".into(),
        from: from_addr.to_string(),
        to_session_id: None,
    }
}

/// Send `payload` to a peer socket. Auth is optional on Unix.
pub fn send_uds(path: &Path, payload: &UdsUserPayload) -> io::Result<()> {
    if !is_inbox_sock_path(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to send on a non-inbox socket path",
        ));
    }
    let peer = lookup_peer_key(path);
    let token = peer.as_ref().and_then(|peer| peer.peer_token.clone());
    let mut payload = payload.clone();
    if payload.to_session_id.is_none() {
        payload.to_session_id = peer.and_then(|peer| peer.session_id);
    }
    send_uds_raw(
        path,
        token.as_deref(),
        &serde_json::to_value(payload).map_err(io::Error::other)?,
    )
}

/// Low-level line send: optional `{type:auth,token}` then JSON `\n`.
pub fn send_uds_raw(path: &Path, auth_token: Option<&str>, payload: &Value) -> io::Result<()> {
    #[cfg(unix)]
    {
        send_uds_unix(path, auth_token, payload)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, auth_token, payload);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "UDS inbox is not available on this platform",
        ))
    }
}

#[cfg(unix)]
fn send_uds_unix(path: &Path, auth_token: Option<&str>, payload: &Value) -> io::Result<()> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(path)?;
    stream.set_write_timeout(Some(SEND_TIMEOUT))?;
    stream.set_read_timeout(Some(SEND_TIMEOUT))?;
    if let Some(token) = auth_token {
        let auth = json!({"type":"auth","token":token});
        writeln!(stream, "{auth}")?;
    }
    writeln!(stream, "{payload}")?;
    stream.flush()?;
    if cfg!(target_os = "macos") {
        std::thread::sleep(MACOS_LINGER);
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// Bind the process inbox and spawn the accept thread.
pub fn start_process_inbox(path: impl Into<PathBuf>) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        start_process_inbox_unix(path.into(), process_session_id(), false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "UDS inbox is not available on this platform",
        ))
    }
}

/// Bind the initial listener to an explicit session identity. The first
/// generation still accepts legacy unscoped payloads; its key advertises the
/// destination so current senders always stamp a generation fence.
pub fn start_process_inbox_for_session(
    path: impl Into<PathBuf>,
    session_id: &str,
) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        start_process_inbox_unix(path.into(), Some(session_id.to_string()), false)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, session_id);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "UDS inbox is not available on this platform",
        ))
    }
}

/// Stop the current listener, preserve its accepted work for the old session,
/// and reuse the same socket path for a new session generation. After a hot
/// switch unscoped legacy payloads are refused: only a sender that read the new
/// key can enter the new session.
pub fn retarget_process_inbox(session_id: &str) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        let path = process_socket_path().unwrap_or_else(|| default_socket_path(std::process::id()));
        start_process_inbox_unix(path, Some(session_id.to_string()), true)
    }
    #[cfg(not(unix))]
    {
        let _ = session_id;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "UDS inbox is not available on this platform",
        ))
    }
}

#[cfg(unix)]
fn start_process_inbox_unix(
    path: PathBuf,
    bound_session_id: Option<String>,
    require_session_fence: bool,
) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    stop_process_inbox_inner()?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    if let Some(parent) = path.parent() {
        if !dir_owned_private(parent) {
            let _ = std::fs::remove_file(&path);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inbox socket parent changed after bind",
            ));
        }
    }
    let state = Arc::new(InboxState::new(
        bound_session_id.clone(),
        require_session_fence,
    ));
    *state.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
    if let Some(p) = path.to_str() {
        std::env::set_var("CLAUDE_CODE_MESSAGING_SOCKET", p);
        std::env::set_var("LINGXI_MESSAGING_SOCKET", p);
    }
    if let Ok((peer, child)) =
        publish_inbox_key_for_session(&sessions_root(), &path, bound_session_id.as_deref())
    {
        *state.peer_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(peer);
        *state.child_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(child.clone());
        std::env::set_var("CLAUDE_CODE_MESSAGING_TOKEN", &child);
        std::env::set_var("LINGXI_MESSAGING_TOKEN", child);
    }
    let accept_state = state.clone();
    let accept_path = path.clone();
    let handle = std::thread::Builder::new()
        .name("lx-uds-inbox".into())
        .spawn(move || accept_loop(listener, accept_state, accept_path))
        .map_err(io::Error::other)?;
    install_runtime(state, handle);
    Ok(path)
}

#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    if std::fs::symlink_metadata(dir)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "inbox socket directory {} must not be a symlink",
                dir.display()
            ),
        ));
    }
    if !dir_owned_private(dir) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "inbox socket directory {} is not a 0700 directory owned by this user",
                dir.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn accept_loop(listener: std::os::unix::net::UnixListener, state: Arc<InboxState>, path: PathBuf) {
    let _ = listener.set_nonblocking(true);
    let mut clients = Vec::new();
    while !state.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                if state.stop.load(Ordering::Relaxed) {
                    break;
                }
                let st = state.clone();
                clients.push(std::thread::spawn(move || handle_client(stream, &st)));
                let mut index = 0;
                while index < clients.len() {
                    if clients[index].is_finished() {
                        let handle = clients.swap_remove(index);
                        let _ = handle.join();
                    } else {
                        index += 1;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                if state.stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    // A socket accepted under generation A continues to write only into A's
    // state. Join every such client before A is spilled and generation B is
    // published, closing the accepted-before-switch race.
    for client in clients {
        let _ = client.join();
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn handle_client(mut stream: std::os::unix::net::UnixStream, state: &InboxState) {
    // `accept_loop` puts the LISTENER into non-blocking mode, and on macOS/BSD an
    // accepted socket INHERITS O_NONBLOCK (Linux does not — POSIX leaves it
    // unspecified and the two disagree). A non-blocking socket ignores
    // `set_read_timeout`: `read` returns `WouldBlock` the instant no bytes are
    // buffered, and the loop below treats that as end-of-message and BREAKS.
    //
    // So on macOS every message whose bytes had not already landed when the
    // server got here was silently dropped — which is most of them, since the
    // sender connects and writes in that order. Verified against a real
    // `UnixListener`: accept, `set_read_timeout(2s)`, read before the peer
    // writes ⇒ `Os { code: 35 }`.
    //
    // `bg_attach.rs` already does this on its accepted stream for the same
    // reason. Restore blocking mode so `SEND_TIMEOUT` is what bounds the read.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(SEND_TIMEOUT));
    let Some((peer_pid, peer_uid)) = peer_identity(&stream) else {
        return;
    };
    if peer_uid != self::uid() {
        return;
    }
    let mut buf = Vec::new();
    let mut tmp = [0_u8; 4096];
    let mut role: Option<&'static str> = None;
    let mut first = true;
    let required = require_auth();
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_LINE {
                    break;
                }
                while let Some(idx) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=idx).collect();
                    let text = String::from_utf8_lossy(&line);
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let Ok(v) = serde_json::from_str::<Value>(text) else {
                        continue;
                    };
                    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "auth" {
                        if first {
                            let tok = v.get("token").and_then(Value::as_str).unwrap_or("");
                            role = token_role(state, tok);
                            if required && role.is_none() {
                                return;
                            }
                        }
                        first = false;
                        continue;
                    }
                    first = false;
                    if required && role.is_none() {
                        return;
                    }
                    dispatch_line(v, state, Some(peer_pid));
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(_) => break,
        }
    }
}

/// `(pid, uid)` of the connected peer when the OS exposes it.
#[cfg(unix)]
fn peer_identity(stream: &std::os::unix::net::UnixStream) -> Option<(u32, u32)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let cred = rustix::net::sockopt::get_socket_peercred(stream).ok()?;
        let pid = cred.pid.as_raw_nonzero().get() as u32;
        let uid = cred.uid.as_raw();
        Some((pid, uid))
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        peer_identity_apple(stream)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    {
        let _ = stream;
        None
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[allow(unsafe_code)]
fn peer_identity_apple(stream: &std::os::unix::net::UnixStream) -> Option<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SOL_LOCAL = 0, LOCAL_PEERPID = 0x002
    let rc = unsafe {
        libc::getsockopt(
            fd,
            0,
            0x002,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    if rc != 0 || pid <= 0 {
        return None;
    }
    let mut euid: libc::uid_t = 0;
    let mut egid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(fd, &mut euid, &mut egid) };
    if rc != 0 {
        return None;
    }
    Some((pid as u32, euid))
}

fn dispatch_line(v: Value, state: &InboxState, peer_pid: Option<u32>) {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "auth" => {}
        "user" => {
            let Some(peer_pid) = peer_pid else {
                return;
            };
            let destination = v.get("toSessionId").and_then(Value::as_str);
            if destination.is_some_and(|value| Some(value) != state.bound_session_id.as_deref())
                || (state.require_session_fence && destination.is_none())
            {
                return;
            }
            let content = v
                .pointer("/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if content.is_empty() {
                return;
            }
            let from_addr = v.get("from").and_then(Value::as_str).map(str::to_string);
            let msg_id = v.get("msg_id").and_then(Value::as_str).map(str::to_string);
            let Some(msg) = attribute_user_message(content, from_addr, msg_id, peer_pid) else {
                return;
            };
            apply_inbound(msg, state);
        }
        "control" => {
            let Some(peer_pid) = peer_pid else {
                return;
            };
            if peer_pid != std::process::id() && process_live_dir().find_by_pid(peer_pid).is_none()
            {
                return;
            }
            if v.get("action").and_then(Value::as_str) == Some("peer_message_status") {
                let status = v.get("status").and_then(Value::as_str).unwrap_or("");
                let notice = receipt_notice(status);
                state
                    .receipts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(notice);
            }
        }
        _ => {}
    }
}

/// Bind identity to the connecting live session. Unregistered pids are dropped.
fn attribute_user_message(
    content: String,
    from_addr: Option<String>,
    msg_id: Option<String>,
    peer_pid: u32,
) -> Option<PeerMessage> {
    let wrap_from = parse_wrap_attr(&content, "from");
    let wrap_name = parse_from_name(&content);
    let wrap_sid = parse_wrap_attr(&content, "from-session");
    let inner = extract_cross_session_inner(&content).to_string();

    let (from, from_session, from_mode, receipt_addr) = if peer_pid == std::process::id() {
        let from = process_name()
            .or(wrap_name)
            .or(wrap_from)
            .filter(|s| !s.starts_with("uds:"))
            .unwrap_or_else(|| "unknown".into());
        let sid = process_session_id().or(wrap_sid).unwrap_or_default();
        let mode = process_permission_class();
        let addr = process_socket_path()
            .map(|p| crate::host::uds_inbox::uds_address(&p))
            .or(from_addr);
        (from, sid, mode, addr)
    } else if let Some(rec) = process_live_dir().find_by_pid(peer_pid) {
        let addr = rec
            .messaging_socket_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|p| crate::host::uds_inbox::uds_address(std::path::Path::new(p)));
        let _ = from_addr;
        (
            rec.display_name().to_string(),
            rec.sid().to_string(),
            rec.permission_class
                .filter(|m| m == "bypass" || m == "prompting"),
            addr,
        )
    } else {
        return None;
    };

    let content = wrap_cross_session_message_with_mode(
        &from,
        &from_session,
        Some(&from),
        from_mode.as_deref(),
        &inner,
    );
    Some(PeerMessage {
        from,
        from_session_id: from_session,
        content,
        summary: None,
        msg_id,
        from_addr: receipt_addr,
        from_mode,
    })
}

fn apply_inbound(msg: PeerMessage, state: &InboxState) {
    if let Some(message_id) = msg.msg_id.as_deref() {
        let mut seen = state
            .seen_message_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if seen.iter().any(|known| known == message_id) {
            return;
        }
        if seen.len() >= SEEN_MESSAGE_LIMIT {
            seen.pop_front();
        }
        seen.push_back(message_id.to_string());
    }
    let (policy, cause) = inbound_decision(msg.from_mode.as_deref());
    match policy {
        InboundPolicy::Refuse | InboundPolicy::Default => {
            if policy == InboundPolicy::Refuse {
                send_receipt(&msg, "denied");
            }
        }
        InboundPolicy::Hold => {
            send_receipt(&msg, "held");
            let mut h = state.held.lock().unwrap_or_else(|e| e.into_inner());
            if h.len() >= 32 {
                if let Some(old) = h.first() {
                    send_receipt(&old.msg, "expired");
                }
                h.remove(0);
            }
            h.push(HeldEntry {
                msg,
                hold_cause: cause.to_string(),
                announced: false,
                quiet_until: None,
            });
        }
        InboundPolicy::Accept => {
            send_receipt(&msg, "delivered");
            state
                .accepted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(msg);
        }
    }
}

fn receipt_notice(status: &str) -> String {
    match status {
        "held" => "[Cross-session delivery notice] held for the recipient user's approval. Not delivered to that session's Claude yet; its user must approve first. Do not wait for a reply; continue, or choose another approach.".into(),
        "denied" => "[Cross-session delivery notice] denied by the recipient user. Not delivered to that session's Claude. Do not wait for a reply; continue, or choose another approach.".into(),
        "expired" => "[Cross-session delivery notice] not approved before expiry. Not delivered to that session's Claude. Do not wait for a reply; continue, or choose another approach.".into(),
        "delivered" => "[Cross-session delivery notice] It was approved and released to that session (final delivery is up to their queue).".into(),
        _ => format!("[Cross-session delivery notice] {status}"),
    }
}

fn send_receipt(msg: &PeerMessage, status: &str) {
    let Some(addr) = msg.from_addr.as_deref() else {
        return;
    };
    let Some(path) = decode_uds_address(addr) else {
        return;
    };
    if !is_canonical_inbox_sock(&path) || !path.exists() {
        return;
    }
    let payload = json!({
        "type": "control",
        "action": "peer_message_status",
        "status": status,
        "orig_msg_id": msg.msg_id,
        "from": process_uds_address(),
    });
    let _ = send_uds_raw(&path, lookup_peer_token(&path).as_deref(), &payload);
}

/// `{pid}.{sha256(canonical)}.key` (2.1.232 `wS_` / `K5u`).
#[must_use]
pub fn inbox_key_path(sessions_dir: &Path, sock: &Path) -> PathBuf {
    let hash = socket_key_hash(sock);
    sessions_dir.join(format!("{}.{hash}.key", std::process::id()))
}

fn socket_key_hash(sock: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canon = sock
        .canonicalize()
        .unwrap_or_else(|_| sock.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let digest = Sha256::digest(canon.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_peer_token() -> String {
    uuid::Uuid::new_v4()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Publish the inbox auth key next to `sessions/<pid>.json`.
/// Returns `(peerToken, childToken)`.
pub fn publish_inbox_key(sessions_dir: &Path, sock: &Path) -> io::Result<(String, String)> {
    publish_inbox_key_for_session(sessions_dir, sock, None)
}

fn publish_inbox_key_for_session(
    sessions_dir: &Path,
    sock: &Path,
    session_id: Option<&str>,
) -> io::Result<(String, String)> {
    std::fs::create_dir_all(sessions_dir)?;
    let peer = random_peer_token();
    let child = random_peer_token();
    let path = inbox_key_path(sessions_dir, sock);
    let mut body = json!({
        "peerToken": peer,
        "childToken": child,
        "procStart": now_ms_string(),
    });
    if let (Some(session_id), Value::Object(object)) = (session_id, &mut body) {
        object.insert("sessionId".into(), Value::String(session_id.to_string()));
    }
    let body = serde_json::to_vec(&body).map_err(io::Error::other)?;
    std::fs::write(&path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((peer, child))
}

fn now_ms_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".into())
}

/// `gMo()` — Windows requires an auth frame; Unix accepts unauthenticated.
#[must_use]
pub fn require_auth() -> bool {
    cfg!(windows)
        || std::env::var("LINGXI_UDS_REQUIRE_AUTH")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
}

fn token_role(state: &InboxState, token: &str) -> Option<&'static str> {
    let peer = state.peer_token.lock().unwrap_or_else(|e| e.into_inner());
    if peer.as_deref() == Some(token) {
        return Some("peer");
    }
    drop(peer);
    let child = state.child_token.lock().unwrap_or_else(|e| e.into_inner());
    if child.as_deref() == Some(token) {
        Some("child")
    } else {
        None
    }
}

/// Look up a peer's `peerToken` for `sock` (any `{pid}.{hash}.key`).
#[must_use]
pub fn lookup_peer_token(sock: &Path) -> Option<String> {
    lookup_peer_key(sock).and_then(|key| key.peer_token)
}

#[derive(Debug)]
struct InboxPeerKey {
    peer_token: Option<String>,
    session_id: Option<String>,
}

fn lookup_peer_key(sock: &Path) -> Option<InboxPeerKey> {
    let hash = socket_key_hash(sock);
    let suffix = format!(".{hash}.key");
    let dir = sessions_root();
    let rd = std::fs::read_dir(dir).ok()?;
    for ent in rd.flatten() {
        let name = ent.file_name();
        let name = name.to_string_lossy();
        if !name.ends_with(&suffix) {
            continue;
        }
        let body = std::fs::read_to_string(ent.path()).ok()?;
        let v: Value = serde_json::from_str(&body).ok()?;
        return Some(InboxPeerKey {
            peer_token: v
                .get("peerToken")
                .and_then(Value::as_str)
                .map(str::to_string),
            session_id: v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    None
}

/// Next unannounced held message for the TUI dialog.
#[must_use]
pub fn next_unannounced_held() -> Option<HeldPeer> {
    let state = inbox();
    let mut held = state.held.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let entry = held
        .iter_mut()
        .find(|e| !e.announced && e.quiet_until.is_none_or(|until| now >= until))?;
    entry.announced = true;
    entry.quiet_until = None;
    let preview_src = extract_cross_session_inner(&entry.msg.content);
    let preview: String = preview_src.chars().take(240).collect();
    let from = if entry.msg.from.is_empty() || entry.msg.from.starts_with("uds:") {
        parse_from_name(&entry.msg.content)
            .or_else(|| parse_wrap_attr(&entry.msg.content, "from"))
            .filter(|s| !s.starts_with("uds:"))
            .unwrap_or_else(|| "an unidentified session".into())
    } else {
        entry.msg.from.clone()
    };
    Some(HeldPeer {
        id: entry
            .msg
            .msg_id
            .clone()
            .unwrap_or_else(|| entry.msg.from.clone()),
        from,
        preview,
        hold_cause: entry.hold_cause.clone(),
        announced: true,
    })
}

/// Esc dismissed the dialog: keep the message held and allow it to reappear.
pub fn unannounce_held(id: &str) {
    let state = inbox();
    let mut held = state.held.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = held
        .iter_mut()
        .find(|e| e.msg.msg_id.as_deref() == Some(id) || e.msg.from == id)
    {
        entry.announced = false;
        entry.quiet_until = Some(Instant::now() + Duration::from_secs(2));
    }
}

/// Approve or deny a held message. Approve moves it to the accepted queue.
pub fn resolve_held(id: &str, approve: bool) -> bool {
    let state = inbox();
    let mut held = state.held.lock().unwrap_or_else(|e| e.into_inner());
    let idx = held
        .iter()
        .position(|e| e.msg.msg_id.as_deref() == Some(id) || e.msg.from == id);
    let Some(idx) = idx else {
        return false;
    };
    let entry = held.remove(idx);
    drop(held);
    if approve {
        send_receipt(&entry.msg, "delivered");
        state
            .accepted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(entry.msg);
    } else {
        send_receipt(&entry.msg, "denied");
    }
    true
}

/// Flush the hold buffer into the accepted queue (`policy-accepts`).
pub fn release_all_held() -> usize {
    let state = inbox();
    let mut held = state.held.lock().unwrap_or_else(|e| e.into_inner());
    let n = held.len();
    let drained: Vec<_> = held.drain(..).collect();
    drop(held);
    let mut acc = state.accepted.lock().unwrap_or_else(|e| e.into_inner());
    for entry in drained {
        send_receipt(&entry.msg, "delivered");
        acc.push_back(entry.msg);
    }
    n
}

/// Drain sender-side delivery notices.
#[must_use]
pub fn take_delivery_notices() -> Vec<String> {
    let state = inbox();
    let mut q = state.receipts.lock().unwrap_or_else(|e| e.into_inner());
    q.drain(..).collect()
}

/// Test/helper: enqueue as if a peer sent an accepted message.
pub fn enqueue_accepted(msg: PeerMessage) {
    inbox()
        .accepted
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(msg);
}

/// Enqueue a message from the JSONL fallback through the same inbound policy
/// and message-id de-duplication used by the UDS receive loop.
pub fn enqueue_inbound(msg: PeerMessage) {
    apply_inbound(msg, &inbox());
}

/// Drain accepted UDS messages into reminder strings.
#[must_use]
pub fn take_accepted_peer_reminders(mid_turn: bool) -> Vec<String> {
    let state = inbox();
    let mut q = state.accepted.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    while let Some(msg) = q.pop_front() {
        out.push(peer_message_reminder(&msg, mid_turn));
    }
    out
}

/// Stop the accept loop, join it, and unlink the socket + key.
pub fn stop_process_inbox() {
    let _ = stop_process_inbox_inner();
}

/// Stop the inbox and report whether all accepted work was preserved to the
/// bound session's file inbox. Lifecycle owners use this checked form so a
/// shutdown persistence failure is visible rather than mistaken for a clean
/// drain.
pub fn stop_process_inbox_checked() -> io::Result<()> {
    stop_process_inbox_inner()
}

fn stop_process_inbox_inner() -> io::Result<()> {
    let (state, join, path) = {
        let mut g = RUNTIME.lock().unwrap_or_else(|e| e.into_inner());
        match g.take() {
            Some(rt) => {
                let join = rt.join;
                let path = rt
                    .state
                    .path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                (Some(rt.state), join, path)
            }
            None => (None, None, None),
        }
    };
    let mut preservation_error = retry_retired_generations().err();
    if let Some(state) = state {
        state.stop.store(true, Ordering::SeqCst);
        if let Some(path) = path.as_ref() {
            #[cfg(unix)]
            {
                let _ = std::os::unix::net::UnixStream::connect(path);
            }
        }
        if let Some(handle) = join {
            let _ = handle.join();
        }
        if let Some(path) = path {
            let key = inbox_key_path(&sessions_root(), &path);
            let _ = std::fs::remove_file(key);
            let _ = std::fs::remove_file(&path);
        }
        if let Err(error) = spill_generation(&state) {
            RETIRED_GENERATIONS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(state.clone());
            if preservation_error.is_none() {
                preservation_error = Some(error);
            }
        }
        state
            .receipts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        state
            .seen_message_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *state.path.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *state.peer_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *state.child_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");
    std::env::remove_var("LINGXI_MESSAGING_SOCKET");
    std::env::remove_var("CLAUDE_CODE_MESSAGING_TOKEN");
    std::env::remove_var("LINGXI_MESSAGING_TOKEN");
    preservation_error.map_or(Ok(()), Err)
}

fn spill_generation(state: &InboxState) -> io::Result<()> {
    let Some(session_id) = state.bound_session_id.as_deref() else {
        state
            .accepted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        state.held.lock().unwrap_or_else(|e| e.into_inner()).clear();
        return Ok(());
    };
    let live_dir = process_live_dir();
    let mut accepted = state.accepted.lock().unwrap_or_else(|e| e.into_inner());
    while let Some(message) = accepted.pop_front() {
        if let Err(error) = live_dir.send_inbox(session_id, &message) {
            accepted.push_front(message);
            return Err(error);
        }
    }
    drop(accepted);
    let mut held = state.held.lock().unwrap_or_else(|e| e.into_inner());
    while !held.is_empty() {
        let entry = held.remove(0);
        if let Err(error) = live_dir.send_inbox(session_id, &entry.msg) {
            held.insert(0, entry);
            return Err(error);
        }
    }
    Ok(())
}

fn retry_retired_generations() -> io::Result<()> {
    let retired = std::mem::take(
        &mut *RETIRED_GENERATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    let mut failed = Vec::new();
    let mut first_error = None;
    for state in retired {
        if let Err(error) = spill_generation(&state) {
            if first_error.is_none() {
                first_error = Some(error);
            }
            failed.push(state);
        }
    }
    RETIRED_GENERATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(failed);
    first_error.map_or(Ok(()), Err)
}

/// Deliver to a live peer: UDS when `messagingSocketPath` is set.
pub fn send_to_live_peer(
    sock: &Path,
    from_name: &str,
    from_sid: &str,
    body: &str,
) -> io::Result<()> {
    if !is_inbox_sock_path(sock) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to send on a non-inbox socket path",
        ));
    }
    let from_addr = process_uds_address().unwrap_or_else(|| uds_address(Path::new("/")));
    let payload = user_payload(from_name, from_sid, &from_addr, body);
    send_uds(sock, &payload)
}

/// Deliver a pre-built message over UDS without changing its stable id.
///
/// `discovered_session_id` is the session the CALLER resolved, which is not
/// necessarily whoever owns `sock` by the time this runs: the inbox key is
/// rewritten with a new `sessionId` on every remount. Stamping the caller's
/// destination is what lets the receiver's fence refuse a payload aimed at a
/// session that has since been replaced, instead of handing it to the new
/// tenant. A generation that advertises no session binding predates the fence
/// and refuses anything stamped, so leave the stamp off for one.
///
/// Delivery is not implied by `Ok`: the wire is one-way, so a fenced-out
/// payload is dropped by the receiver with nothing reported back. Callers
/// treat this as a wake-up on top of a durable transport, never as the
/// delivery itself.
pub fn send_peer_message(
    sock: &Path,
    message: &PeerMessage,
    discovered_session_id: &str,
) -> io::Result<()> {
    if !is_inbox_sock_path(sock) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to send on a non-inbox socket path",
        ));
    }
    let peer_is_fenced = lookup_peer_key(sock)
        .and_then(|key| key.session_id)
        .is_some();
    let payload = UdsUserPayload {
        msg_v: MSG_V,
        msg_id: message
            .msg_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        kind: "user".into(),
        message: UdsUserMessage {
            role: "user".into(),
            content: message.content.clone(),
        },
        priority: "next".into(),
        from: message
            .from_addr
            .clone()
            .or_else(process_uds_address)
            .unwrap_or_else(|| uds_address(Path::new("/"))),
        to_session_id: peer_is_fenced.then(|| discovered_session_id.to_string()),
    };
    send_uds(sock, &payload)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn clean_env() {
        std::env::remove_var("LINGXI_UDS_REQUIRE_AUTH");
        std::env::remove_var("LINGXI_CROSS_SESSION_INBOUND");
        crate::host::live_sessions::set_process_permission_class(None);
    }

    #[test]
    fn uds_address_encodes_spaces() {
        let _g = test_guard();
        assert_eq!(
            uds_address(Path::new("/tmp/cc socks/1.sock")),
            "uds:/tmp/cc%20socks/1.sock"
        );
        assert_eq!(
            decode_uds_address("uds:/tmp/cc%20socks/1.sock").as_deref(),
            Some(Path::new("/tmp/cc socks/1.sock"))
        );
        assert_eq!(
            decode_uds_address("uds:/tmp/cc%20socks/1.sock").as_deref(),
            Some(Path::new("/tmp/cc socks/1.sock"))
        );
    }

    #[test]
    fn default_socket_avoids_world_writable_tmp() {
        let _g = test_guard();
        let prev_xdg = std::env::var_os("XDG_RUNTIME_DIR");
        let prev_tmp = std::env::var_os("CLAUDE_CODE_TMPDIR");
        let prev_lx = std::env::var_os("LINGXI_TMPDIR");
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        std::env::remove_var("CLAUDE_CODE_TMPDIR");
        std::env::remove_var("LINGXI_TMPDIR");
        let path = default_socket_path(42);
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        match prev_tmp {
            Some(v) => std::env::set_var("CLAUDE_CODE_TMPDIR", v),
            None => std::env::remove_var("CLAUDE_CODE_TMPDIR"),
        }
        match prev_lx {
            Some(v) => std::env::set_var("LINGXI_TMPDIR", v),
            None => std::env::remove_var("LINGXI_TMPDIR"),
        }
        let parent = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(
            parent.starts_with("cc-socks-"),
            "expected /tmp/cc-socks-<uid>, got {}",
            path.display()
        );
        assert_eq!(path.file_name().unwrap(), "42.sock");
    }

    #[test]
    fn loopback_user_message() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        crate::host::live_sessions::set_process_name("alpha");
        crate::host::live_sessions::set_process_session_id("s1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        start_process_inbox(&path).unwrap();
        send_to_live_peer(&path, "alpha", "s1", "hello").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            got = take_accepted_peer_reminders(false);
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop_process_inbox();
        clean_env();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("<cross-session-message"), "{got:?}");
        assert!(got[0].contains("hello"), "{got:?}");
        assert!(got[0].contains("from=\"alpha\""), "{got:?}");
    }

    /// A sender that connects and then pauses before writing must still be
    /// heard.
    ///
    /// `loopback_user_message` above cannot catch this: `send_uds` connects and
    /// writes back-to-back, so the bytes are usually already buffered by the
    /// time `handle_client` reads and the bug hides. Here the server is made to
    /// read FIRST.
    ///
    /// The bug: `accept_loop` sets the LISTENER non-blocking, and on macOS/BSD
    /// the accepted socket inherits O_NONBLOCK. `set_read_timeout` does nothing
    /// on a non-blocking socket, so the first `read` returned `WouldBlock`, which
    /// the read loop treats as end-of-message — dropping the message.
    #[test]
    fn a_sender_that_pauses_before_writing_is_still_heard() {
        use std::io::Write;
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        crate::host::live_sessions::set_process_name("alpha");
        crate::host::live_sessions::set_process_session_id("s1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        start_process_inbox(&path).unwrap();

        let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
        // Let the server accept and reach its first `read` with nothing buffered.
        std::thread::sleep(Duration::from_millis(250));
        // Built from the real payload type so the test cannot drift from the wire
        // shape the reader expects.
        let payload = UdsUserPayload {
            msg_v: 1,
            msg_id: "11111111-1111-4111-8111-111111111111".into(),
            kind: "user".into(),
            message: UdsUserMessage {
                role: "user".into(),
                content: "delayed hello".into(),
            },
            priority: "next".into(),
            from: uds_address(Path::new("/beta")),
            to_session_id: Some("s1".into()),
        };
        let line = serde_json::to_string(&payload).unwrap();
        stream.write_all(line.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            got = take_accepted_peer_reminders(false);
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop_process_inbox();
        clean_env();
        assert_eq!(
            got.len(),
            1,
            "a paused sender must still be delivered: {got:?}"
        );
        assert!(got[0].contains("delayed hello"), "{got:?}");
    }

    #[test]
    fn file_fallback_uses_receive_policy_and_delivers_each_message_id_once() {
        let _guard = test_guard();
        stop_process_inbox();
        clean_env();
        std::env::set_var("LINGXI_CROSS_SESSION_INBOUND", "accept");
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        let session_id = "11111111-2222-4333-8444-555555555555";
        crate::host::live_sessions::set_process_dir(dir.clone());
        crate::host::live_sessions::set_process_session_id(session_id);
        crate::host::live_sessions::set_process_name("receiver");
        let message = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "22222222-3333-4444-8555-666666666666",
            "fallback hello",
            Some("fallback"),
        );
        dir.send_inbox(session_id, &message).unwrap();
        let first = crate::host::live_sessions::take_accepted_peer_reminders(false);
        assert_eq!(first.len(), 1);
        assert!(first[0].contains("fallback hello"));
        assert!(crate::host::live_sessions::take_accepted_peer_reminders(false).is_empty());

        // A retry over the alternate transport with the same stable id must not
        // surface twice.
        dir.send_inbox(session_id, &message).unwrap();
        assert!(crate::host::live_sessions::take_accepted_peer_reminders(false).is_empty());
        std::env::remove_var("LINGXI_CROSS_SESSION_INBOUND");
        stop_process_inbox();
    }

    #[test]
    fn restart_does_not_leak_previous_accept_loop() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        crate::host::live_sessions::set_process_name("alpha");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("restart.sock");
        start_process_inbox(&path).unwrap();
        send_to_live_peer(&path, "alpha", "s1", "first").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !take_accepted_peer_reminders(false).is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        start_process_inbox(&path).unwrap();
        assert!(take_accepted_peer_reminders(false).is_empty());
        send_to_live_peer(&path, "alpha", "s1", "second").unwrap();
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            got = take_accepted_peer_reminders(false);
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop_process_inbox();
        clean_env();
        assert!(got.iter().any(|s| s.contains("second")), "{got:?}");
        assert!(got.iter().all(|s| !s.contains("first")), "{got:?}");
    }

    #[test]
    fn require_auth_drops_unauthenticated() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        std::env::set_var("LINGXI_UDS_REQUIRE_AUTH", "1");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.sock");
        start_process_inbox(&path).unwrap();
        let payload = serde_json::json!({"type":"user","msg_id":"x","message":{"role":"user","content":"nope"},"from":"uds:/tmp/x"});
        let _ = send_uds_raw(&path, None, &payload);
        std::thread::sleep(Duration::from_millis(80));
        let got = take_accepted_peer_reminders(false);
        stop_process_inbox();
        clean_env();
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn hold_then_approve() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        crate::host::live_sessions::set_process_name("alpha");
        std::env::set_var("LINGXI_CROSS_SESSION_INBOUND", "hold");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hold.sock");
        start_process_inbox(&path).unwrap();
        send_to_live_peer(&path, "alpha", "s1", "park-me").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut held = None;
        while Instant::now() < deadline {
            held = next_unannounced_held();
            if held.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let held = held.expect("held");
        assert!(held.hold_cause.contains("explicit") || held.hold_cause == "explicit-setting");
        assert!(!held.from.starts_with("uds:"), "{}", held.from);
        assert!(held.preview.contains("park-me"), "{}", held.preview);
        assert!(take_accepted_peer_reminders(false).is_empty());
        assert!(resolve_held(&held.id, true));
        let got = take_accepted_peer_reminders(false);
        stop_process_inbox();
        clean_env();
        assert!(got.iter().any(|s| s.contains("park-me")), "{got:?}");
    }

    #[test]
    fn mode_mismatch_holds_when_default() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        crate::host::live_sessions::set_process_permission_class(Some("bypass"));
        let (policy, cause) = inbound_decision(Some("prompting"));
        assert_eq!(policy, InboundPolicy::Hold);
        assert_eq!(cause, "mode-mismatch");
        let (policy, cause) = inbound_decision(None);
        assert_eq!(policy, InboundPolicy::Hold);
        assert_eq!(cause, "no-mode-asserted");
        crate::host::live_sessions::set_process_permission_class(Some("prompting"));
        let (policy, _) = inbound_decision(None);
        assert_eq!(policy, InboundPolicy::Accept);
        crate::host::live_sessions::set_process_permission_class(None);
        let (policy, cause) = inbound_decision(Some("bypass"));
        assert_eq!(policy, InboundPolicy::Hold);
        assert_eq!(cause, "mode-mismatch");
    }

    #[test]
    fn key_round_trip() {
        let _g = test_guard();
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("x.sock");
        std::fs::write(&sock, b"").unwrap();
        let (peer, child) = publish_inbox_key(dir.path(), &sock).unwrap();
        assert_eq!(peer.len(), 32);
        assert_eq!(child.len(), 32);
        assert_ne!(peer, child);
        let body = std::fs::read_to_string(inbox_key_path(dir.path(), &sock)).unwrap();
        assert!(body.contains("peerToken"));
        assert!(body.contains("childToken"));
        assert!(body.contains("procStart"));
    }

    #[test]
    fn payload_shape() {
        let p = user_payload("alpha", "sid", "uds:/tmp/x.sock", "hi");
        assert_eq!(p.msg_v, 1);
        assert_eq!(p.kind, "user");
        assert_eq!(p.message.role, "user");
        assert_eq!(p.priority, "next");
        assert!(p.message.content.contains("from=\"alpha\""));
    }

    #[test]
    fn hot_generation_refuses_missing_or_stale_destination() {
        let _g = test_guard();
        clean_env();
        std::env::set_var("LINGXI_CROSS_SESSION_INBOUND", "accept");
        crate::host::live_sessions::set_process_name("receiver");
        crate::host::live_sessions::set_process_session_id("session-b");
        let state = InboxState::new(Some("session-b".into()), true);
        let payload = |destination: Option<&str>, id: &str| {
            let mut value = serde_json::json!({
                "type": "user",
                "msg_id": id,
                "message": {"role": "user", "content": "hello"},
                "from": "uds:/tmp/source.sock"
            });
            if let (Some(destination), Value::Object(object)) = (destination, &mut value) {
                object.insert("toSessionId".into(), Value::String(destination.into()));
            }
            value
        };

        dispatch_line(payload(None, "missing"), &state, Some(std::process::id()));
        dispatch_line(
            payload(Some("session-a"), "stale"),
            &state,
            Some(std::process::id()),
        );
        dispatch_line(
            payload(Some("session-b"), "current"),
            &state,
            Some(std::process::id()),
        );

        let accepted = state.accepted.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(accepted.len(), 1);
        assert_eq!(
            accepted
                .front()
                .and_then(|message| message.msg_id.as_deref()),
            Some("current")
        );
        std::env::remove_var("LINGXI_CROSS_SESSION_INBOUND");
    }

    /// Shutdown must preserve BOTH queues, not just the accepted one.
    ///
    /// `spill_generation` walks `accepted` and then `held`, and the held half
    /// had no coverage here — it was only asserted in `bridge-server`'s
    /// `live_registration_allows_resume_of_existing_transcript_and_cleans_up`,
    /// where the fixture races `take_accepted_peer_reminders` (every turn calls
    /// it, from another crate, holding no test lock). This crate owns the
    /// global and serialises its own tests, so the property belongs here.
    #[test]
    fn shutdown_spills_both_accepted_and_held_work_to_the_bound_session() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        crate::host::live_sessions::set_process_dir(dir.clone());
        crate::host::live_sessions::set_process_session_id("session-a");
        let socket = temp.path().join("inbox.sock");
        start_process_inbox_for_session(&socket, "session-a").unwrap();

        let mut accepted = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source-session",
            "survive shutdown",
            None,
        );
        accepted.msg_id = Some("shutdown-accepted".into());
        enqueue_accepted(accepted);

        std::env::set_var("LINGXI_CROSS_SESSION_INBOUND", "hold");
        let mut held = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source-session",
            "held across shutdown",
            None,
        );
        held.msg_id = Some("shutdown-held".into());
        enqueue_inbound(held);
        std::env::remove_var("LINGXI_CROSS_SESSION_INBOUND");

        stop_process_inbox_checked().expect("shutdown preserves both queues");

        let mut spilled = dir
            .drain_inbox("session-a")
            .expect("bound session inbox")
            .iter()
            .filter_map(|message| message.msg_id.clone())
            .collect::<Vec<_>>();
        spilled.sort();
        assert_eq!(spilled, vec!["shutdown-accepted", "shutdown-held"]);
        clean_env();
    }

    #[test]
    fn retarget_spills_old_generation_work_to_old_session() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        crate::host::live_sessions::set_process_dir(dir.clone());
        crate::host::live_sessions::set_process_session_id("session-a");
        let socket = temp.path().join("inbox.sock");
        start_process_inbox_for_session(&socket, "session-a").unwrap();
        let mut message = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source-session",
            "owned by A",
            None,
        );
        message.msg_id = Some("generation-a".into());
        enqueue_accepted(message);

        retarget_process_inbox("session-b").expect("retarget");

        let spilled = dir.drain_inbox("session-a").expect("old inbox");
        assert_eq!(spilled.len(), 1);
        assert_eq!(spilled[0].msg_id.as_deref(), Some("generation-a"));
        assert!(dir.drain_inbox("session-b").unwrap().is_empty());
        stop_process_inbox();
    }

    #[test]
    fn paused_peer_lookup_keeps_delivery_in_original_session_after_remount() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        crate::host::live_sessions::set_process_dir(dir.clone());
        crate::host::live_sessions::set_process_session_id("session-a");
        let socket = temp.path().join("inbox.sock");
        start_process_inbox_for_session(&socket, "session-a").unwrap();
        dir.upsert_identity(
            std::process::id(),
            "session-a",
            Some("receiver"),
            None,
            Some(&socket),
            None,
        )
        .unwrap();
        let peer = dir.find_by_pid(std::process::id()).unwrap();
        let message =
            crate::host::live_sessions::outbound_peer_message("sender", "source", "for A", None);
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let sender_dir = dir.clone();
        let sender = std::thread::spawn(move || {
            // The real discovery result is retained across this scheduling gap.
            resume_rx.recv().unwrap();
            // Both production senders write this transport unconditionally and
            // key it by their discovery result, which is what keeps a remount
            // from re-routing the message. Their socket leg is a separate
            // best-effort wake-up, covered by
            // `one_message_on_both_transports_is_delivered_once`.
            sender_dir.send_inbox(peer.sid(), &message).unwrap();
        });
        retarget_process_inbox("session-b").unwrap();
        crate::host::live_sessions::set_process_session_id("session-b");
        dir.upsert_identity(
            std::process::id(),
            "session-b",
            Some("receiver"),
            None,
            Some(&socket),
            None,
        )
        .unwrap();
        resume_tx.send(()).unwrap();
        sender.join().unwrap();
        assert!(take_accepted_peer_reminders(false).is_empty());
        assert!(dir.drain_inbox("session-b").unwrap().is_empty());
        let original = dir.drain_inbox("session-a").unwrap();
        assert_eq!(original.len(), 1);
        assert!(original[0].content.contains("for A"));
        stop_process_inbox();
    }

    /// The two production senders write both transports for one message, so
    /// the peer must still see it once. Receive-time de-duplication keys on
    /// `msg_id`, which the socket payload and the inbox row carry unchanged.
    #[test]
    fn one_message_on_both_transports_is_delivered_once() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        crate::host::live_sessions::set_process_dir(dir.clone());
        crate::host::live_sessions::set_process_session_id("session-a");
        let socket = temp.path().join("inbox.sock");
        start_process_inbox_for_session(&socket, "session-a").unwrap();

        let mut message = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source",
            "exactly once",
            None,
        );
        message.msg_id = Some("both-transports".into());

        // The order the senders use: the durable copy first, then the wake-up.
        dir.send_inbox("session-a", &message).unwrap();
        send_peer_message(&socket, &message, "session-a").unwrap();

        // This entry drains the file inbox into the same receive-time path the
        // socket feeds, so a second copy would surface right here.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            got.extend(crate::host::live_sessions::take_accepted_peer_reminders(
                false,
            ));
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Whichever leg landed first, give the other one room to follow.
        std::thread::sleep(Duration::from_millis(250));
        got.extend(crate::host::live_sessions::take_accepted_peer_reminders(
            false,
        ));
        let leftover = dir.drain_inbox("session-a").unwrap();
        stop_process_inbox();
        clean_env();
        assert_eq!(got.len(), 1, "one message must arrive once: {got:?}");
        assert!(got[0].contains("exactly once"), "{got:?}");
        assert!(leftover.is_empty(), "the drain must consume the inbox row");
    }

    /// A payload stamped for the session the SENDER discovered is refused by a
    /// generation that has since remounted, rather than handed to the new
    /// tenant. The stamp has to come from the caller's discovery result: the
    /// inbox key on disk is rewritten with the new `sessionId` at remount, so
    /// reading the destination back off the key would address whoever owns the
    /// socket now — exactly the mis-delivery this fence exists to stop.
    #[test]
    fn a_remounted_peer_refuses_a_payload_stamped_for_the_replaced_session() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        let temp = tempfile::TempDir::new().unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(temp.path().join("sessions"));
        crate::host::live_sessions::set_process_dir(dir.clone());
        crate::host::live_sessions::set_process_session_id("session-a");
        let socket = temp.path().join("inbox.sock");
        start_process_inbox_for_session(&socket, "session-a").unwrap();

        // The peer remounts; its key on disk now advertises the new session.
        retarget_process_inbox("session-b").unwrap();
        crate::host::live_sessions::set_process_session_id("session-b");

        let mut stale = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source",
            "meant for A",
            None,
        );
        stale.msg_id = Some("stamped-for-a".into());
        send_peer_message(&socket, &stale, "session-a").unwrap();

        // `Ok` above only says the bytes were written. Hold the assertion open
        // well past the point an accepted payload would have surfaced.
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            let leaked = take_accepted_peer_reminders(false);
            assert!(
                leaked.is_empty(),
                "a payload stamped for the replaced session reached the new one: {leaked:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Prove that emptiness is the fence and not a socket nobody is reading.
        let mut fresh = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source",
            "meant for B",
            None,
        );
        fresh.msg_id = Some("stamped-for-b".into());
        send_peer_message(&socket, &fresh, "session-b").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        while Instant::now() < deadline {
            got = take_accepted_peer_reminders(false);
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop_process_inbox();
        clean_env();
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("meant for B"), "{got:?}");
    }

    #[test]
    fn failed_old_generation_spill_is_retained_but_never_exposed_to_new_session() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        let temp = tempfile::TempDir::new().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::write(&sessions, "temporarily unavailable").unwrap();
        let dir = crate::host::live_sessions::LiveSessionDir::at(sessions.clone());
        crate::host::live_sessions::set_process_dir(dir.clone());
        let old = Arc::new(InboxState::new(Some("session-a".into()), true));
        let mut message = crate::host::live_sessions::outbound_peer_message(
            "sender",
            "source-session",
            "retained A",
            None,
        );
        message.msg_id = Some("retained-a".into());
        old.accepted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(message);
        *RUNTIME.lock().unwrap_or_else(|e| e.into_inner()) = Some(InboxRuntime {
            state: old,
            join: None,
        });

        assert!(stop_process_inbox_checked().is_err());
        crate::host::live_sessions::set_process_session_id("session-b");
        assert!(
            take_accepted_peer_reminders(false).is_empty(),
            "retired A work must not be readable through B's active inbox state"
        );

        std::fs::remove_file(&sessions).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        stop_process_inbox_checked().expect("retry retained generation");
        let spilled = dir.drain_inbox("session-a").unwrap();
        assert_eq!(spilled.len(), 1);
        assert_eq!(spilled[0].msg_id.as_deref(), Some("retained-a"));
        assert!(dir.drain_inbox("session-b").unwrap().is_empty());
    }

    #[test]
    fn sock_path_gate_rejects_world_tmp() {
        assert!(!is_inbox_sock_path(Path::new("/tmp/evil.sock")));
        assert!(!is_canonical_inbox_sock(Path::new("/tmp/evil.sock")));
        assert!(!is_inbox_sock_path(Path::new("relative.sock")));
        assert!(!is_cc_socks_dir_name("cc-socks-owned"));
        assert!(is_cc_socks_dir_name("cc-socks"));
        assert!(is_cc_socks_dir_name("cc-socks-501"));
    }

    #[test]
    fn control_from_unknown_pid_is_dropped() {
        let _g = test_guard();
        stop_process_inbox();
        clean_env();
        dispatch_line(
            serde_json::json!({
                "type": "control",
                "action": "peer_message_status",
                "status": "delivered"
            }),
            &inbox(),
            Some(4_294_967_294),
        );
        assert!(take_delivery_notices().is_empty());
    }

    #[test]
    fn attribute_refuses_unknown_pid() {
        let _g = test_guard();
        clean_env();
        assert!(attribute_user_message(
            "<cross-session-message from-mode=\"bypass\">hi</cross-session-message>".into(),
            Some("uds:/tmp/x.sock".into()),
            Some("m".into()),
            4_294_967_294,
        )
        .is_none());
    }
}
