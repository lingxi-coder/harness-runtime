//! Per-installation loopback gateway. A URL contains an unguessable run epoch;
//! every accepted connection is cancelled when that epoch is stopped.
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder},
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Semaphore,
};
use tokio_util::sync::CancellationToken;

const MAX_HEAD: usize = 32 * 1024;
const MAX_AGENT_BODY: usize = 116 * 1024 * 1024;
const MAX_AGENT_CONTROL_BODY: usize = 32 * 1024;
const MAX_AGENT_JSON_DEPTH: usize = 128;
const MAX_STATIC_HTML_TRANSFORM_BYTES: u64 = 4 * 1024 * 1024;
const WEBMCP_SCRIPT_ROUTE: &str = "__host/webmcp.js";
// Serialize large full-input transfers across Apps to bound inline file memory.
static AGENT_REQUESTS: Semaphore = Semaphore::const_new(1);
// Small polling and cancellation requests stay available while a large or slow
// Agent input is being admitted. Their count and body size bound their memory.
static AGENT_CONTROL_REQUESTS: Semaphore = Semaphore::const_new(8);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PageRequest {
    method: String,
    args: Box<serde_json::value::RawValue>,
}

#[derive(Clone)]
pub(super) struct BrowserHost {
    pub owner: Weak<super::MobileLocalAppServiceHandle>,
    pub app_id: String,
    pub name: String,
    pub revision: u64,
    pub platform: String,
}

#[derive(Clone)]
pub(super) struct Route {
    pub host: Option<BrowserHost>,
    pub run_id: String,
    pub root: PathBuf,
    /// Host-owned per-App upload scratch space; never derived from page source.
    pub spool_dir: PathBuf,
    pub entry: String,
    pub entry_service: Option<String>,
    pub services: BTreeMap<String, u16>,
    pub port: u16,
    pub capabilities: Vec<String>,
    pub cancel: CancellationToken,
}

pub(super) fn spawn(
    listener: impl Into<Arc<TcpListener>>,
    route: Route,
) -> tokio::task::JoinHandle<()> {
    let listener = listener.into();
    tokio::spawn(async move {
        let route = Arc::new(route);
        let permits = Arc::new(Semaphore::new(32));
        loop {
            let socket = tokio::select! {
                _ = route.cancel.cancelled() => break,
                accepted = listener.accept() => match accepted { Ok((socket, peer)) if peer.ip().is_loopback() => socket, Ok(_) => continue, Err(_) => break },
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            let route = route.clone();
            tokio::spawn(async move {
                let _permit = permit;
                tokio::select! {
                    _ = route.cancel.cancelled() => {},
                    _ = serve(socket, &route) => {},
                }
            });
        }
    })
}

async fn error(socket: &mut TcpStream, code: u16, message: &str) -> std::io::Result<()> {
    let body = message.as_bytes();
    socket.write_all(format!("HTTP/1.1 {code} Error\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n", body.len()).as_bytes()).await?;
    socket.write_all(body).await
}

async fn spool_fixed_body<R>(
    reader: &mut R,
    prefix: &[u8],
    length: usize,
    directory: &Path,
) -> std::io::Result<std::fs::File>
where
    R: tokio::io::AsyncRead + Unpin,
{
    if prefix.len() > length {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "body exceeds content length",
        ));
    }
    validate_private_spool_dir(directory)?;
    // The host chooses a per-App directory inside its private library. This
    // avoids Android's `/data/local/tmp` default and never accepts a page path.
    // tempfile_in creates an owner-private anonymous file (0600 on Unix); the
    // entry is unlinked on close, including timeout/cancellation/error paths.
    let file = tempfile::tempfile_in(directory)?;
    let mut writer = tokio::fs::File::from_std(file);
    writer.write_all(prefix).await?;
    let mut written = prefix.len();
    while written < length {
        let mut chunk = [0_u8; 8192];
        let remaining = (length - written).min(chunk.len());
        let count = reader.read(&mut chunk[..remaining]).await?;
        if count == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        writer.write_all(&chunk[..count]).await?;
        written += count;
    }
    writer.flush().await?;
    Ok(writer.into_std().await)
}

pub(super) fn prepare_agent_spool_dir(
    library_root: &Path,
    app_id: &str,
) -> std::io::Result<PathBuf> {
    if app_id.is_empty()
        || !app_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid app ID for upload spool",
        ));
    }
    let root_metadata = fs::symlink_metadata(library_root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "App library root is not a real directory",
        ));
    }
    let canonical_root = library_root.canonicalize()?;
    if canonical_root.as_path() != library_root {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "App library root is not canonical",
        ));
    }
    let library_root = canonical_root;
    let apps = library_root.join("apps");
    validate_existing_directory(&apps)?;
    let app_root = apps.join(app_id);
    validate_existing_directory(&app_root)?;

    let agent = app_root.join("agent");
    ensure_host_directory(&agent, false)?;
    let assets = agent.join("assets");
    ensure_host_directory(&assets, false)?;
    let spool_dir = assets.join("upload-spool");
    ensure_host_directory(&spool_dir, true)?;
    validate_private_spool_dir(&spool_dir)?;
    Ok(spool_dir)
}

fn validate_existing_directory(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "upload spool path contains a symlink or non-directory",
        ));
    }
    Ok(())
}

fn ensure_host_directory(path: &Path, private: bool) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "upload spool path contains a symlink or non-directory",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            #[cfg(unix)]
            if private {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
        }
        Err(error) => return Err(error),
    }
    if private {
        validate_private_spool_dir(path)?;
    }
    Ok(())
}

fn validate_private_spool_dir(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "upload spool directory is not a real directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "upload spool directory permissions must be owner-only",
            ));
        }
    }
    if path.canonicalize()?.as_path() != path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "upload spool directory resolves outside its host-owned path",
        ));
    }
    Ok(())
}

fn parse_page_request<R: Read + Seek>(mut reader: R) -> Result<PageRequest, &'static str> {
    use serde::Deserialize as _;

    reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| "could not seek Agent request body")?;
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let request =
        PageRequest::deserialize(&mut deserializer).map_err(|_| "invalid Agent request JSON")?;
    deserializer
        .end()
        .map_err(|_| "invalid Agent request JSON")?;

    // RawValue keeps large inline DataSource payloads in one allocation. Its
    // deserializer can capture deeply nested values without serde_json's usual
    // recursion limit, so enforce the same bound with a no-allocation scan.
    validate_agent_json_depth(request.args.get())?;
    Ok(request)
}

fn validate_agent_json_depth(value: &str) -> Result<(), &'static str> {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in value.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_AGENT_JSON_DEPTH {
                    return Err("Agent request JSON is too deeply nested");
                }
            }
            b'}' | b']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or("invalid Agent request JSON structure")?;
            }
            _ => {}
        }
    }
    if in_string || escaped || depth != 0 {
        return Err("invalid Agent request JSON structure");
    }
    Ok(())
}

fn is_page_method_allowed(method: &str) -> bool {
    matches!(
        method,
        "agent.tasks.start"
            | "agent.tasks.get"
            | "agent.tasks.list"
            | "agent.tasks.cancel"
            | "agent.tasks.events"
            | "agent.chats.create"
            | "agent.chats.get"
            | "agent.chats.list"
            | "agent.chats.archive"
            | "agent.chats.delete"
            | "agent.chats.history"
            | "agent.chats.send"
            | "agent.chats.cancel"
    )
}

fn is_small_control_method(method: &str) -> bool {
    matches!(
        method,
        "agent.tasks.get"
            | "agent.tasks.list"
            | "agent.tasks.cancel"
            | "agent.tasks.events"
            | "agent.chats.get"
            | "agent.chats.list"
            | "agent.chats.cancel"
    )
}

fn is_agent_json_content_type(value: &[u8]) -> bool {
    value.eq_ignore_ascii_case(b"application/json")
}

fn browser_sdk_script(configuration: &str) -> String {
    let browser_sdk =
        include_str!("browser_sdk.js").replace("__LINGXI_APP_CONFIGURATION__", configuration);
    let agui_bundle = include_str!("../../../../../packages/local-app-agui/dist/browser.global.js");
    let mut script = String::with_capacity(agui_bundle.len() + browser_sdk.len() + 96);
    script.push_str(agui_bundle);
    // browser.global.js declares its namespace with top-level var, which
    // becomes module-scoped when this route is loaded through import().
    // Publish that exact official bundle export on the browser global before
    // the host adapter runs. In a classic script this is an idempotent write.
    script.push_str("\n;\nglobalThis.LingxiLocalAppAGUI = LingxiLocalAppAGUI;\n;\n");
    script.push_str(&browser_sdk);
    script
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn find_tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (offset, byte) in html.as_bytes().iter().copied().enumerate().skip(start) {
        match (quote, byte) {
            (Some(active), value) if active == value => quote = None,
            (Some(_), _) => {}
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(offset + 1),
            (None, _) => {}
        }
    }
    None
}

struct HtmlTag {
    name_start: usize,
    name_end: usize,
    end: usize,
    closing: bool,
    declaration: bool,
}

fn is_html_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')
}

fn is_html_whitespace_char(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\u{000c}' | '\r')
}

fn trim_html_whitespace(value: &str) -> &str {
    value.trim_matches(is_html_whitespace_char)
}

fn next_html_tag(html: &str, lower: &str, mut cursor: usize) -> Option<HtmlTag> {
    let bytes = lower.as_bytes();
    loop {
        let offset = lower[cursor..].find('<')?;
        let start = cursor + offset;
        if lower[start..].starts_with("<!--") {
            let close = lower[start + 4..].find("-->")?;
            cursor = start + 4 + close + 3;
            continue;
        }
        let mut name_start = start + 1;
        let declaration = bytes
            .get(name_start)
            .is_some_and(|byte| matches!(*byte, b'!' | b'?'));
        if declaration {
            name_start += 1;
        }
        let closing = !declaration && bytes.get(name_start) == Some(&b'/');
        if closing {
            name_start += 1;
        }
        if !declaration && !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            cursor = start + 1;
            continue;
        }
        let mut name_end = name_start;
        while bytes
            .get(name_end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b':'))
        {
            name_end += 1;
        }
        let boundary = bytes.get(name_end).copied();
        if !declaration
            && !boundary.is_some_and(|byte| is_html_whitespace(byte) || matches!(byte, b'/' | b'>'))
        {
            // Skip the malformed/start-like token as one attribute-bearing tag,
            // so a '<head>' substring inside it cannot become an insertion point.
            cursor = find_tag_end(html, start).unwrap_or(start + 1);
            continue;
        }
        let end = find_tag_end(html, start)?;
        return Some(HtmlTag {
            name_start,
            name_end,
            end,
            closing,
            declaration,
        });
    }
}

fn skip_raw_text_element(html: &str, lower: &str, after_opening_tag: usize, name: &str) -> usize {
    if name == "plaintext" {
        return html.len();
    }
    if name == "script" {
        return skip_script_element(html, lower, after_opening_tag);
    }
    let closing = format!("</{name}");
    let mut cursor = after_opening_tag;
    while let Some(offset) = lower[cursor..].find(&closing) {
        let start = cursor + offset;
        let boundary = lower.as_bytes().get(start + closing.len()).copied();
        if boundary.is_some_and(|byte| is_html_whitespace(byte) || matches!(byte, b'/' | b'>')) {
            return find_tag_end(html, start).unwrap_or(html.len());
        }
        cursor = start + closing.len();
    }
    html.len()
}

fn is_html_tag_boundary(byte: u8) -> bool {
    is_html_whitespace(byte) || matches!(byte, b'/' | b'>')
}

fn skip_script_element(html: &str, lower: &str, after_opening_tag: usize) -> usize {
    let bytes = lower.as_bytes();
    let mut state = 0_u8; // script data, escaped, or double escaped
    let mut cursor = after_opening_tag;
    while cursor < bytes.len() {
        let rest = &bytes[cursor..];
        if state != 0 && rest.starts_with(b"-->") {
            state = 0;
            cursor += 3;
            continue;
        }
        if state == 0 && rest.starts_with(b"<!--") {
            state = 1;
            cursor += 4;
            continue;
        }
        if state == 2
            && rest.starts_with(b"</script")
            && bytes
                .get(cursor + 8)
                .copied()
                .is_some_and(is_html_tag_boundary)
        {
            state = 1;
            cursor += 8;
            continue;
        }
        if state != 2
            && rest.starts_with(b"</script")
            && bytes
                .get(cursor + 8)
                .copied()
                .is_some_and(is_html_tag_boundary)
        {
            return find_tag_end(html, cursor).unwrap_or(html.len());
        }
        if state == 1
            && rest.starts_with(b"<script")
            && bytes
                .get(cursor + 7)
                .copied()
                .is_some_and(is_html_tag_boundary)
        {
            state = 2;
            cursor += 7;
            continue;
        }
        cursor += 1;
    }
    html.len()
}

fn next_raw_text_cursor(html: &str, lower: &str, tag: &HtmlTag) -> usize {
    if !tag.closing && !tag.declaration {
        let name = &lower[tag.name_start..tag.name_end];
        if matches!(
            name,
            "script"
                | "style"
                | "textarea"
                | "title"
                | "noscript"
                | "iframe"
                | "xmp"
                | "noembed"
                | "noframes"
                | "plaintext"
        ) {
            return skip_raw_text_element(html, lower, tag.end, name);
        }
    }
    tag.end
}

fn skip_template_element(html: &str, lower: &str, after_opening_tag: usize) -> usize {
    let mut depth = 1_usize;
    let mut cursor = after_opening_tag;
    while let Some(tag) = next_html_tag(html, lower, cursor) {
        if !tag.declaration && lower[tag.name_start..tag.name_end].eq("template") {
            if tag.closing {
                depth -= 1;
                if depth == 0 {
                    return tag.end;
                }
            } else {
                depth += 1;
            }
        }
        // Unlike the outer scanner, this cursor must see nested template tags
        // so it can match their closing tags. It still skips comments and raw
        // text, where a template-looking substring is not a tag token.
        cursor = next_raw_text_cursor(html, lower, &tag);
    }
    html.len()
}

fn next_tag_cursor(html: &str, lower: &str, tag: &HtmlTag) -> usize {
    if !tag.closing && !tag.declaration && lower[tag.name_start..tag.name_end].eq("template") {
        return skip_template_element(html, lower, tag.end);
    }
    next_raw_text_cursor(html, lower, tag)
}

fn is_webmcp_script_source(value: &str, expected: &str) -> bool {
    let path = trim_html_whitespace(value)
        .split(|character| matches!(character, '?' | '#'))
        .next()
        .unwrap_or("");
    path == expected || path == "/__host/webmcp.js"
}

fn base_href_may_override_origin(href: &str) -> bool {
    // Mobile pages use an HTTP loopback origin. A custom-scheme sentinel has
    // a fresh opaque Origin on each comparison, even for a relative URL.
    let document_base = url::Url::parse("http://local-app.invalid/index.html")
        .expect("static Local App URL is valid");
    let Ok(candidate) = document_base.join(trim_html_whitespace(href)) else {
        return false;
    };
    if matches!(candidate.scheme(), "data" | "javascript") {
        return false;
    }
    candidate.origin() != document_base.origin()
}

#[derive(Default)]
struct ScriptAttributes<'a> {
    source: Option<&'a str>,
    script_type: Option<&'a str>,
    href: Option<&'a str>,
    asynchronous: bool,
    defer: bool,
    no_module: bool,
    integrity: bool,
}

fn script_attributes(attributes: &str) -> ScriptAttributes<'_> {
    let mut result = ScriptAttributes::default();
    let mut seen_source = false;
    let mut seen_type = false;
    let mut seen_href = false;
    let mut seen_async = false;
    let mut seen_defer = false;
    let mut seen_no_module = false;
    let mut seen_integrity = false;
    let mut cursor = 0;
    while cursor < attributes.len() {
        while let Some(character) = attributes[cursor..].chars().next() {
            if !is_html_whitespace_char(character) && character != '/' {
                break;
            }
            cursor += character.len_utf8();
        }
        let name_start = cursor;
        while let Some(character) = attributes[cursor..].chars().next() {
            if is_html_whitespace_char(character) || matches!(character, '=' | '/' | '>') {
                break;
            }
            cursor += character.len_utf8();
        }
        if cursor == name_start {
            if let Some(character) = attributes[cursor..].chars().next() {
                cursor += character.len_utf8();
            }
            continue;
        }
        let name = &attributes[name_start..cursor];
        while let Some(character) = attributes[cursor..].chars().next() {
            if !is_html_whitespace_char(character) {
                break;
            }
            cursor += character.len_utf8();
        }
        let value = if attributes.as_bytes().get(cursor) == Some(&b'=') {
            cursor += 1;
            while let Some(character) = attributes[cursor..].chars().next() {
                if !is_html_whitespace_char(character) {
                    break;
                }
                cursor += character.len_utf8();
            }
            match attributes[cursor..].chars().next() {
                Some(quote @ ('\'' | '"')) => {
                    cursor += 1;
                    let value_start = cursor;
                    while let Some(character) = attributes[cursor..].chars().next() {
                        if character == quote {
                            break;
                        }
                        cursor += character.len_utf8();
                    }
                    let value = &attributes[value_start..cursor];
                    if cursor < attributes.len() {
                        cursor += quote.len_utf8();
                    }
                    value
                }
                Some(_) => {
                    let value_start = cursor;
                    while let Some(character) = attributes[cursor..].chars().next() {
                        if is_html_whitespace_char(character) || character == '>' {
                            break;
                        }
                        cursor += character.len_utf8();
                    }
                    &attributes[value_start..cursor]
                }
                None => "",
            }
        } else {
            ""
        };
        if name.eq_ignore_ascii_case("src") && !seen_source {
            result.source = Some(value);
            seen_source = true;
        } else if name.eq_ignore_ascii_case("type") && !seen_type {
            result.script_type = Some(value);
            seen_type = true;
        } else if name.eq_ignore_ascii_case("href") && !seen_href {
            result.href = Some(value);
            seen_href = true;
        } else if name.eq_ignore_ascii_case("async") && !seen_async {
            result.asynchronous = true;
            seen_async = true;
        } else if name.eq_ignore_ascii_case("defer") && !seen_defer {
            result.defer = true;
            seen_defer = true;
        } else if name.eq_ignore_ascii_case("nomodule") && !seen_no_module {
            result.no_module = true;
            seen_no_module = true;
        } else if name.eq_ignore_ascii_case("integrity") && !seen_integrity {
            result.integrity = true;
            seen_integrity = true;
        }
    }
    result
}

fn is_classic_script(attributes: &ScriptAttributes<'_>) -> bool {
    let Some(script_type) = attributes.script_type else {
        return true;
    };
    let mime = trim_html_whitespace(
        trim_html_whitespace(script_type)
            .split(';')
            .next()
            .unwrap_or(""),
    );
    mime.is_empty()
        || [
            "application/ecmascript",
            "application/javascript",
            "application/x-ecmascript",
            "application/x-javascript",
            "text/ecmascript",
            "text/javascript",
            "text/javascript1.0",
            "text/javascript1.1",
            "text/javascript1.2",
            "text/javascript1.3",
            "text/javascript1.4",
            "text/javascript1.5",
            "text/jscript",
            "text/livescript",
            "text/x-ecmascript",
            "text/x-javascript",
        ]
        .iter()
        .any(|candidate| mime.eq_ignore_ascii_case(candidate))
}

fn may_run_before_later_host_script(attributes: &ScriptAttributes<'_>) -> bool {
    if attributes
        .script_type
        .is_some_and(|value| trim_html_whitespace(value).eq_ignore_ascii_case("module"))
    {
        return attributes.asynchronous;
    }
    if !is_classic_script(attributes) {
        return false;
    }
    if attributes.source.is_none() {
        return true;
    }
    attributes.asynchronous || attributes.no_module || !attributes.defer
}

fn reliable_host_script(attributes: &ScriptAttributes<'_>) -> bool {
    attributes.source.is_some()
        && is_classic_script(attributes)
        && !attributes.asynchronous
        && !attributes.defer
        && !attributes.no_module
        && !attributes.integrity
}

fn has_existing_webmcp_script(html: &str, expected: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    let mut earlier_blocking_application_script = false;
    let mut base_href_seen = false;
    let mut base_overrides_origin = false;
    while let Some(tag) = next_html_tag(html, &lower, cursor) {
        let name = &lower[tag.name_start..tag.name_end];
        if !tag.closing && !tag.declaration && name == "base" {
            let href = script_attributes(&html[tag.name_end..tag.end.saturating_sub(1)]).href;
            if !base_href_seen {
                if let Some(href) = href {
                    base_href_seen = true;
                    base_overrides_origin = base_href_may_override_origin(href);
                }
            }
        } else if !tag.closing && !tag.declaration && name == "script" {
            let attributes = script_attributes(&html[tag.name_end..tag.end.saturating_sub(1)]);
            let host_script = attributes.source.is_some_and(|value| {
                !base_overrides_origin && is_webmcp_script_source(value, expected)
            });
            if host_script
                && reliable_host_script(&attributes)
                && !earlier_blocking_application_script
            {
                return true;
            }
            if !host_script && may_run_before_later_host_script(&attributes) {
                earlier_blocking_application_script = true;
            }
        }
        cursor = next_tag_cursor(html, &lower, &tag);
    }
    false
}

fn find_leading_doctype_end(html: &str) -> Option<usize> {
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(tag) = next_html_tag(html, &lower, cursor) {
        if tag.declaration && lower[tag.name_start..tag.name_end].eq("doctype") {
            let tag_start = tag.name_start.saturating_sub(2);
            return is_leading_html_preamble(&html[..tag_start]).then_some(tag.end);
        }
        if !tag.declaration {
            return None;
        }
        cursor = next_tag_cursor(html, &lower, &tag);
    }
    None
}

fn is_leading_html_preamble(value: &str) -> bool {
    let mut cursor = if value.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let bytes = value.as_bytes();
    while cursor < bytes.len() {
        if is_html_whitespace(bytes[cursor]) {
            cursor += 1;
            continue;
        }
        if bytes[cursor..].starts_with(b"<!--") {
            let Some(end) = value[cursor + 4..].find("-->") else {
                return false;
            };
            cursor += 4 + end + 3;
            continue;
        }
        if bytes[cursor..].starts_with(b"<?") || bytes[cursor..].starts_with(b"<!") {
            let Some(end) = bytes[cursor + 2..].iter().position(|byte| *byte == b'>') else {
                return false;
            };
            cursor += 2 + end + 1;
            continue;
        }
        return false;
    }
    true
}

fn inject_webmcp_script(html: &str, run_id: &str) -> Option<String> {
    let run_id = encode_path_segment(run_id);
    let source = format!("/__run/{run_id}/{WEBMCP_SCRIPT_ROUTE}");
    if has_existing_webmcp_script(html, &source) {
        return None;
    }
    let script = format!("<script src=\"{source}\" data-lingxi-webmcp></script>");
    let doctype_end = find_leading_doctype_end(html);
    let insert_at = doctype_end.unwrap_or_else(|| {
        if html.starts_with('\u{feff}') {
            '\u{feff}'.len_utf8()
        } else {
            0
        }
    });
    let mut injected = String::with_capacity(html.len() + script.len());
    injected.push_str(&html[..insert_at]);
    injected.push_str(&script);
    injected.push_str(&html[insert_at..]);
    Some(injected)
}

async fn serve(socket: TcpStream, route: &Route) -> std::io::Result<()> {
    serve_with_agent_dispatch(
        socket,
        route,
        &AGENT_REQUESTS,
        &AGENT_CONTROL_REQUESTS,
        |host, run_id, method, arguments| async move {
            let owner = host.owner.upgrade()?;
            Some(
                owner
                    .agent_page_json(host.app_id, run_id, method, arguments)
                    .await,
            )
        },
    )
    .await
}

async fn serve_with_agent_dispatch<F, Fut>(
    mut socket: TcpStream,
    route: &Route,
    agent_requests: &Semaphore,
    agent_control_requests: &Semaphore,
    dispatch: F,
) -> std::io::Result<()>
where
    F: FnOnce(BrowserHost, String, String, String) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let mut bytes = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() >= MAX_HEAD {
            return error(&mut socket, 431, "request headers too large").await;
        }
        let mut buf = [0_u8; 4096];
        let count = match tokio::time::timeout(Duration::from_secs(10), socket.read(&mut buf)).await
        {
            Ok(result) => result?,
            Err(_) => return error(&mut socket, 408, "request timeout").await,
        };
        if count == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&buf[..count]);
    };
    if head_end > MAX_HEAD {
        return error(&mut socket, 431, "request headers too large").await;
    }
    let mut headers = [httparse::EMPTY_HEADER; 96];
    let mut request = httparse::Request::new(&mut headers);
    if !matches!(
        request.parse(&bytes[..head_end]),
        Ok(httparse::Status::Complete(_))
    ) {
        return error(&mut socket, 400, "invalid HTTP request").await;
    }
    let method = request.method.unwrap_or("");
    let target = request.path.unwrap_or("");
    let expected_host = format!("127.0.0.1:{}", route.port);
    let expected_origin = format!("http://{expected_host}");
    let hosts: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("host"))
        .collect();
    if hosts.len() != 1 || hosts[0].value != expected_host.as_bytes() {
        return error(&mut socket, 403, "invalid app host").await;
    }
    if request
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("origin") && h.value != expected_origin.as_bytes())
    {
        return error(
            &mut socket,
            403,
            "cross-origin app requests are not allowed",
        )
        .await;
    }
    let prefix = format!("/__run/{}/", route.run_id);
    let Some(relative) = target.strip_prefix(&prefix) else {
        // Ordinary frameworks use root-absolute URLs. Redirect those into the
        // exact referring run, rather than granting the current run by origin.
        // Same-origin Referer cannot be forged by page JavaScript. A stale
        // document consequently cannot enter a replacement run.
        let epoch_referrer = format!("{expected_origin}{prefix}");
        let from_current = request.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("referer")
                && std::str::from_utf8(h.value)
                    .is_ok_and(|value| value.starts_with(&epoch_referrer))
        });
        if from_current
            && target.starts_with('/')
            && !target.starts_with("//")
            && !target.starts_with("/__run/")
        {
            let static_base = if route.entry_service.is_none() {
                route
                    .entry
                    .rsplit_once('/')
                    .map(|(parent, _)| format!("{parent}/"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let relative = target.trim_start_matches('/');
            let path = relative.split('?').next().unwrap_or(relative);
            let target = if matches!(
                path,
                "__local_app_sdk.js" | "__host/sdk.js" | "__host/recovery-sw.js" | WEBMCP_SCRIPT_ROUTE
            ) {
                relative.to_owned()
            } else if relative.is_empty() || relative.starts_with('?') {
                format!("{}{relative}", route.entry)
            } else {
                format!("{static_base}{relative}")
            };
            socket.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {prefix}{target}\r\nContent-Length: 0\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: same-origin\r\n\r\n").as_bytes()).await?;
            return Ok(());
        }
        return error(
            &mut socket,
            410,
            "this application run is no longer available",
        )
        .await;
    };
    let relative_path = relative.split('?').next().unwrap_or(relative);
    // Reject request-smuggling ambiguity before opening any upstream socket.
    let lengths: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("content-length"))
        .collect();
    let transfers: Vec<_> = request
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
        .collect();
    if lengths.len() > 1 || transfers.len() > 1 || (!lengths.is_empty() && !transfers.is_empty()) {
        return error(&mut socket, 400, "ambiguous request body framing").await;
    }
    if lengths.iter().any(|h| {
        std::str::from_utf8(h.value)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .is_none()
    }) {
        return error(&mut socket, 400, "invalid content length").await;
    }
    if transfers
        .iter()
        .any(|h| !h.value.eq_ignore_ascii_case(b"chunked"))
    {
        return error(&mut socket, 400, "unsupported transfer encoding").await;
    }
    // Reserved routes are handled before any user application service. The
    // caller cannot select an app, run, model, permission mode or host operation.
    if relative_path == "__local_app_sdk.js" || relative_path == "__host/sdk.js" {
        if method != "GET" && method != "HEAD" {
            return error(&mut socket, 405, "SDK accepts GET and HEAD").await;
        }
        let Some(host) = &route.host else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        let config = serde_json::json!({"appId":host.app_id,"name":host.name,"revision":host.revision,"hostPlatform":host.platform,"runId":route.run_id,"capabilities":route.capabilities,"services":route.services.keys().collect::<Vec<_>>()});
        let script = browser_sdk_script(&config.to_string());
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{}\r\n", script.len(), security_headers(route)).as_bytes()).await?;
        if method == "GET" {
            socket.write_all(script.as_bytes()).await?;
        }
        return Ok(());
    }
    if relative_path == WEBMCP_SCRIPT_ROUTE {
        if method != "GET" && method != "HEAD" {
            return error(&mut socket, 405, "WebMCP polyfill accepts GET and HEAD").await;
        }
        let Some(_host) = &route.host else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        let script = include_str!("../../../../../packages/local-app-webmcp/dist/webmcp-polyfill.iife.js");
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{}\r\n", script.len(), security_headers(route)).as_bytes()).await?;
        if method == "GET" {
            socket.write_all(script.as_bytes()).await?;
        }
        return Ok(());
    }
    if relative_path == "__host/recovery-sw.js" {
        if method != "GET" && method != "HEAD" {
            return error(&mut socket, 405, "recovery worker accepts GET and HEAD").await;
        }
        let Some(host) = &route.host else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        let config = serde_json::json!({
            "appId": host.app_id,
            "revision": host.revision,
            "hostPlatform": host.platform
        });
        let script = include_str!("recovery_sw.js")
            .replace("__LINGXI_RECOVERY_CONFIGURATION__", &config.to_string());
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nService-Worker-Allowed: /\r\nX-Content-Type-Options: nosniff\r\n\r\n", script.len()).as_bytes()).await?;
        if method == "GET" {
            socket.write_all(script.as_bytes()).await?;
        }
        return Ok(());
    }
    if relative.starts_with("__host/") {
        if relative != "__host/agent" {
            return error(&mut socket, 404, "unknown host operation").await;
        }
        if method != "POST" {
            return error(&mut socket, 405, "Agent accepts POST").await;
        }
        // A run URL alone is not enough for cross-site writes. Require all
        // browser-controlled origin/referrer metadata and a non-simple header.
        let exact_header = |name: &str, value: &[u8]| {
            let values: Vec<_> = request
                .headers
                .iter()
                .filter(|h| h.name.eq_ignore_ascii_case(name))
                .collect();
            values.len() == 1 && values[0].value == value
        };
        let referrer = format!("{expected_origin}{prefix}");
        let referrers: Vec<_> = request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("referer"))
            .collect();
        let content_types: Vec<_> = request
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("content-type"))
            .collect();
        if !exact_header("origin", expected_origin.as_bytes())
            || !exact_header("x-lingxi-app-request", b"1")
            || content_types.len() != 1
            || referrers.len() != 1
            || !std::str::from_utf8(referrers[0].value).is_ok_and(|v| v.starts_with(&referrer))
        {
            return error(&mut socket, 403, "invalid application caller").await;
        }
        if !is_agent_json_content_type(content_types[0].value) {
            return error(&mut socket, 400, "Agent requires application/json").await;
        }
        if !transfers.is_empty() || lengths.len() != 1 {
            return error(&mut socket, 400, "Agent requires a fixed content length").await;
        }
        let length: usize = match std::str::from_utf8(lengths[0].value)
            .ok()
            .and_then(|v| v.parse().ok())
        {
            Some(length) if length <= MAX_AGENT_BODY => length,
            _ => return error(&mut socket, 413, "Agent request too large").await,
        };
        if length == 0 {
            return error(&mut socket, 400, "invalid request body").await;
        }
        let small_body = length <= MAX_AGENT_CONTROL_BODY;
        let mut control_permit = if small_body {
            match agent_control_requests.try_acquire() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    return error(&mut socket, 429, "application host is busy; retry later").await;
                }
            }
        } else {
            None
        };
        let mut heavy_permit = if small_body {
            None
        } else {
            match agent_requests.try_acquire() {
                Ok(permit) => Some(permit),
                Err(_) => {
                    return error(&mut socket, 429, "application host is busy; retry later").await;
                }
            }
        };
        let prefix = &bytes[head_end..];
        let spool = match tokio::time::timeout(
            Duration::from_secs(30),
            spool_fixed_body(&mut socket, prefix, length, &route.spool_dir),
        )
        .await
        {
            Ok(Ok(spool)) => spool,
            Ok(Err(io_error)) if io_error.kind() == std::io::ErrorKind::InvalidData => {
                return error(&mut socket, 400, "invalid request body").await;
            }
            Ok(Err(_)) => return error(&mut socket, 408, "incomplete Agent request").await,
            Err(_) => return error(&mut socket, 408, "Agent request timed out").await,
        };
        let input = {
            let mut spool = spool;
            match parse_page_request(&mut spool) {
                Ok(input) => input,
                Err(_) => return error(&mut socket, 400, "invalid Agent request").await,
            }
        };
        // The raw wire body stays on the anonymous tempfile until the official
        // args value has been parsed into its single bounded RawValue allocation.
        // Timeout, parse rejection, cancellation, and normal return close the
        // spool through RAII.
        if !is_page_method_allowed(&input.method) {
            return error(
                &mut socket,
                403,
                "host operation is not available to applications",
            )
            .await;
        }
        let control_method = is_small_control_method(&input.method);
        if control_method && !small_body {
            return error(&mut socket, 413, "Agent control request too large").await;
        }
        let _heavy_permit = if control_method {
            None
        } else {
            let permit = match heavy_permit.take() {
                Some(permit) => permit,
                None => match agent_requests.try_acquire() {
                    Ok(permit) => permit,
                    Err(_) => {
                        return error(&mut socket, 429, "application host is busy; retry later")
                            .await;
                    }
                },
            };
            // The body was bounded and parsed while holding the control lane;
            // after taking the heavy lane, only its one request can retain an
            // arbitrary-size input across the awaited native handoff.
            drop(control_permit.take());
            Some(permit)
        };
        let Some(host) = route.host.as_ref().cloned() else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        let PageRequest { method, args } = input;
        let arguments = String::from(Box::<str>::from(args));
        let Some(result) = dispatch(host, route.run_id.clone(), method, arguments).await else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n", result.len()).as_bytes()).await?;
        return socket.write_all(result.as_bytes()).await;
    }
    let named_service = relative.starts_with("__service/");
    let (service, upstream_path) = if let Some(service_route) = relative.strip_prefix("__service/")
    {
        let Some((name, path)) = service_route.split_once('/') else {
            return error(&mut socket, 404, "unknown service").await;
        };
        (Some(name), format!("/{path}"))
    } else {
        (route.entry_service.as_deref(), format!("/{relative}"))
    };
    if let Some(name) = service {
        let Some(port) = route.services.get(name) else {
            return error(&mut socket, 404, "unknown service").await;
        };
        let mut upstream = match tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, *port)),
        )
        .await
        {
            Ok(Ok(upstream)) => upstream,
            _ => return error(&mut socket, 502, "application service is unavailable").await,
        };
        let upgrade = request.headers.iter().any(|h| {
            h.name.eq_ignore_ascii_case("upgrade") && h.value.eq_ignore_ascii_case(b"websocket")
        });
        let mut head = format!("{method} {upstream_path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Forwarded-Prefix: {prefix}\r\n").into_bytes();
        for header in request.headers.iter() {
            if [
                "host",
                "connection",
                "proxy-connection",
                "proxy-authorization",
                "x-forwarded-prefix",
            ]
            .iter()
            .any(|name| header.name.eq_ignore_ascii_case(name))
            {
                continue;
            }
            head.extend_from_slice(header.name.as_bytes());
            head.extend_from_slice(b": ");
            head.extend_from_slice(header.value);
            head.extend_from_slice(b"\r\n");
        }
        head.extend_from_slice(if upgrade {
            b"Connection: Upgrade\r\n\r\n"
        } else {
            b"Connection: close\r\n\r\n"
        });
        upstream.write_all(&head).await?;
        upstream.write_all(&bytes[head_end..]).await?;
        let response_prefix = if named_service {
            format!("{prefix}__service/{name}/")
        } else {
            prefix.clone()
        };
        let (mut client_read, mut client_write) = socket.split();
        let (mut upstream_read, mut upstream_write) = upstream.split();
        // Only bounded HTTP headers are inspected. Bodies and upgraded binary
        // streams remain opaque, concurrently flowing in both directions.
        let security = security_headers(route);
        tokio::try_join!(
            async {
                tokio::io::copy(&mut client_read, &mut upstream_write).await?;
                upstream_write.shutdown().await
            },
            forward_response(
                &mut upstream_read,
                &mut client_write,
                &response_prefix,
                *port,
                &security,
            ),
        )?;
        return Ok(());
    }
    if method != "GET" && method != "HEAD" {
        return error(&mut socket, 405, "static applications accept GET and HEAD").await;
    }
    let path = relative.split('?').next().unwrap_or("");
    let path = if path.is_empty() {
        route.entry.as_str()
    } else {
        path
    };
    let Some(file) = confined_file(&route.root, path) else {
        return error(&mut socket, 404, "asset not found").await;
    };
    let mut file_handle = match tokio::fs::File::open(&file).await {
        Ok(file) => file,
        Err(_) => return error(&mut socket, 404, "asset not found").await,
    };
    let metadata = file_handle.metadata().await?;
    if !metadata.is_file() {
        return error(&mut socket, 404, "asset not found").await;
    }
    let extension = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    let is_html = extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm");
    let content_type = if is_html {
        "text/html; charset=utf-8"
    } else {
        match extension {
            "js" | "mjs" => "text/javascript; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "json" => "application/json",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "wasm" => "application/wasm",
            "ico" => "image/x-icon",
            "woff2" => "font/woff2",
            _ => "application/octet-stream",
        }
    };
    if is_html && metadata.len() <= MAX_STATIC_HTML_TRANSFORM_BYTES {
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file_handle.read_to_end(&mut bytes).await?;
        let body = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|html| inject_webmcp_script(html, &route.run_id))
            .map_or(bytes, |html| html.into_bytes());
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{}\r\n", body.len(), security_headers(route)).as_bytes()).await?;
        if method == "GET" {
            socket.write_all(&body).await?;
        }
        return Ok(());
    }
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{}\r\n", metadata.len(), security_headers(route)).as_bytes()).await?;
    if method == "GET" {
        tokio::io::copy(&mut file_handle, &mut socket).await?;
    }
    Ok(())
}

fn security_headers(route: &Route) -> String {
    let network = route
        .capabilities
        .iter()
        .any(|capability| capability == "network");
    let remote_sources = if network { " https: wss:" } else { "" };
    let remote_scripts = if network { " https:" } else { "" };
    let camera = if route.capabilities.iter().any(|c| c == "camera") {
        "(self)"
    } else {
        "()"
    };
    let microphone = if route.capabilities.iter().any(|c| c == "microphone") {
        "(self)"
    } else {
        "()"
    };
    format!(
        "Content-Security-Policy: sandbox allow-scripts allow-forms allow-modals allow-downloads allow-same-origin allow-top-navigation-to-custom-protocols; default-src 'self' data: blob:{remote_scripts}; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:{remote_scripts}; style-src 'self' 'unsafe-inline'{remote_scripts}; connect-src 'self' ws://127.0.0.1:{}{remote_sources}; worker-src 'self'; object-src 'none'; base-uri 'none'; frame-src 'none'; frame-ancestors 'none'\r\nPermissions-Policy: camera={camera}, microphone={microphone}, geolocation=()\r\nReferrer-Policy: same-origin\r\nX-Frame-Options: DENY\r\n",
        route.port
    )
}

async fn forward_response<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    upstream: &mut R,
    client: &mut W,
    prefix: &str,
    port: u16,
    security: &str,
) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    loop {
        let end = loop {
            if let Some(index) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
                break index + 4;
            }
            if bytes.len() > MAX_HEAD {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "upstream headers too large",
                ));
            }
            let mut chunk = [0_u8; 4096];
            let count = tokio::time::timeout(Duration::from_secs(30), upstream.read(&mut chunk))
                .await
                .map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "upstream response timeout")
                })??;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "upstream closed before headers",
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        };
        if end > MAX_HEAD {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "upstream headers too large",
            ));
        }
        let mut headers = [httparse::EMPTY_HEADER; 96];
        let mut response = httparse::Response::new(&mut headers);
        if !matches!(
            response.parse(&bytes[..end]),
            Ok(httparse::Status::Complete(_))
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid upstream HTTP response",
            ));
        }
        let code = response.code.unwrap_or(502);
        let mut head =
            format!("HTTP/1.1 {code} {}\r\n", response.reason.unwrap_or("")).into_bytes();
        for header in response.headers.iter() {
            head.extend_from_slice(header.name.as_bytes());
            head.extend_from_slice(b": ");
            if header.name.eq_ignore_ascii_case("location") {
                if let Ok(location) = std::str::from_utf8(header.value) {
                    head.extend_from_slice(rewrite_location(location, prefix, port).as_bytes());
                } else {
                    head.extend_from_slice(header.value);
                }
            } else {
                head.extend_from_slice(header.value);
            }
            head.extend_from_slice(b"\r\n");
        }
        if code >= 200 {
            head.extend_from_slice(security.as_bytes());
        }
        head.extend_from_slice(b"\r\n");
        client.write_all(&head).await?;
        bytes.drain(..end);
        if (100..200).contains(&code) && code != 101 {
            continue;
        }
        client.write_all(&bytes).await?;
        tokio::io::copy(upstream, client).await?;
        return client.shutdown().await;
    }
}
fn rewrite_location(location: &str, prefix: &str, port: u16) -> String {
    let local = location
        .strip_prefix(&format!("http://127.0.0.1:{port}/"))
        .or_else(|| location.strip_prefix(&format!("http://localhost:{port}/")));
    if let Some(path) = local {
        return format!("{prefix}{path}");
    }
    if location.starts_with('/') && !location.starts_with("//") {
        return format!("{prefix}{}", location.trim_start_matches('/'));
    }
    location.to_owned()
}

fn confined_file(root: &Path, raw: &str) -> Option<PathBuf> {
    let mut decoded = Vec::new();
    let bytes = raw.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%' {
            let hex = std::str::from_utf8(bytes.get(cursor + 1..cursor + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            cursor += 3;
        } else {
            decoded.push(bytes[cursor]);
            cursor += 1;
        }
    }
    let value = String::from_utf8(decoded).ok()?;
    if value.contains(['\\', '\0']) {
        return None;
    }
    let path = Path::new(&value);
    if path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let candidate = root.join(path).canonicalize().ok()?;
    candidate.starts_with(&root).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_base_preserves_the_mobile_http_origin() {
        for href in ["/assets/", "./assets/", "../assets/", "", "#fragment"] {
            assert!(!base_href_may_override_origin(href), "{href}");
        }
        for href in [
            "https://other.example/",
            "lingxi-app://current/custom/",
            "mailto:x",
        ] {
            assert!(base_href_may_override_origin(href), "{href}");
        }
    }

    pub(super) async fn exchange(port: u16, bytes: Vec<u8>) -> Vec<u8> {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        socket.write_all(&bytes).await.unwrap();
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut output))
            .await
            .unwrap()
            .unwrap();
        output
    }
    #[tokio::test]
    async fn static_epoch_host_and_path_are_checked() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("index.html"), "fixture").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "current".into(),
                root: temp.path().into(),
                spool_dir: temp.path().into(),
                entry: "index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        for (path, host, expected) in [
            ("/__run/current/", format!("127.0.0.1:{port}"), "200"),
            ("/__run/old/", format!("127.0.0.1:{port}"), "410"),
            (
                "/__run/current/%2e%2e/secret",
                format!("127.0.0.1:{port}"),
                "404",
            ),
            ("/__run/current/", "attacker.test".into(), "403"),
        ] {
            let response = exchange(
                port,
                format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
            )
            .await;
            assert!(String::from_utf8_lossy(&response).starts_with(&format!("HTTP/1.1 {expected}")));
        }
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn static_html_receives_run_scoped_webmcp_and_non_html_stays_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("index.html"),
            "<!doctype html><html><head data-note=\"left>right\"><script id=\"app-start\"></script></head><body></body></html>",
        )
        .unwrap();
        std::fs::write(
            temp.path().join("UPPER.HTML"),
            "<!doctype html><script id=\"app-start\"></script>",
        )
        .unwrap();
        std::fs::write(temp.path().join("asset.js"), "window.asset = true;").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "current-run".into(),
                root: temp.path().into(),
                spool_dir: temp.path().into(),
                entry: "index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );

        let response = String::from_utf8(
            exchange(
                port,
                format!(
                    "GET /__run/current-run/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
                )
                .into_bytes(),
            )
            .await,
        )
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Content-Security-Policy:"));
        assert!(response.contains("script-src 'self'"));
        let body = response.split_once("\r\n\r\n").unwrap().1;
        let script = "<script src=\"/__run/current-run/__host/webmcp.js\" data-lingxi-webmcp></script>";
        assert_eq!(body.matches("data-lingxi-webmcp").count(), 1);
        assert!(body.find(script).unwrap() < body.find("id=\"app-start\"").unwrap());
        assert!(response.contains(&format!("Content-Length: {}\r\n", body.len())));

        let response = String::from_utf8(
            exchange(
                port,
                format!(
                    "GET /__run/current-run/UPPER.HTML HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
                )
                .into_bytes(),
            )
            .await,
        )
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Content-Type: text/html; charset=utf-8"));
        let body = response.split_once("\r\n\r\n").unwrap().1;
        assert!(body.find("webmcp.js").unwrap() < body.find("id=\"app-start\"").unwrap());

        let response = String::from_utf8(
            exchange(
                port,
                format!(
                    "GET /__run/current-run/asset.js HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
                )
                .into_bytes(),
            )
            .await,
        )
        .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("window.asset = true;"));
        assert!(!response.contains("data-lingxi-webmcp"));

        let stale = exchange(
            port,
            format!("GET /__run/old-run/__host/webmcp.js HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                .into_bytes(),
        )
        .await;
        assert!(String::from_utf8_lossy(&stale).starts_with("HTTP/1.1 410"));
        cancel.cancel();
        task.await.unwrap();
    }
    #[tokio::test]
    async fn post_binary_bytes_and_sse_are_forwarded_without_buffering_response() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut input = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                input.extend_from_slice(&buf[..n]);
                if input.ends_with(&[0, 255, 1, 2]) {
                    break;
                }
            }
            assert!(input.starts_with(b"POST /upload HTTP/1.1"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: ready\n\n").await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "epoch".into(),
                root: PathBuf::new(),
                spool_dir: PathBuf::new(),
                entry: String::new(),
                entry_service: Some("api".into()),
                services: BTreeMap::from([("api".into(), upstream_port)]),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        let mut request=format!("POST /__run/epoch/upload HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 4\r\n\r\n").into_bytes();
        request.extend_from_slice(&[0, 255, 1, 2]);
        let response = exchange(port, request).await;
        assert!(response.ends_with(b"data: ready\n\n"));
        server.await.unwrap();
        cancel.cancel();
        task.await.unwrap();
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    async fn header(socket: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let byte = socket.read_u8().await.unwrap();
            head.push(byte);
            assert!(head.len() < MAX_HEAD);
        }
        head
    }
    #[tokio::test]
    async fn websocket_binary_frames_flow_both_ways_and_stop_closes_stream() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let head = header(&mut socket).await;
            assert!(head.starts_with(b"GET /socket HTTP/1.1"));
            socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: test\r\n\r\n").await.unwrap();
            let mut masked = [0_u8; 8];
            socket.read_exact(&mut masked).await.unwrap();
            assert_eq!(masked, [0x82, 0x82, 1, 2, 3, 4, 1, 0xfd]);
            socket.write_all(&[0x82, 2, 0, 0xff]).await.unwrap();
            let _ = reached_tx.send(());
            let mut byte = [0; 1];
            let eof = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(eof, 0);
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "ws".into(),
                root: PathBuf::new(),
                spool_dir: PathBuf::new(),
                entry: String::new(),
                entry_service: None,
                services: BTreeMap::from([("socket".into(), upstream_port)]),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        client.write_all(format!("GET /__run/ws/__service/socket/socket HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").as_bytes()).await.unwrap();
        assert!(header(&mut client).await.starts_with(b"HTTP/1.1 101"));
        client
            .write_all(&[0x82, 0x82, 1, 2, 3, 4, 1, 0xfd])
            .await
            .unwrap();
        let mut frame = [0; 4];
        client.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame, [0x82, 2, 0, 0xff]);
        reached_rx.await.unwrap();
        cancel.cancel();
        task.await.unwrap();
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        server.await.unwrap();
    }
    #[tokio::test]
    async fn sse_chunk_arrives_before_upstream_finishes_and_redirect_stays_in_epoch() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            header(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: first\n\n").await.unwrap();
            finish_rx.await.unwrap();
            socket.write_all(b"data: last\n\n").await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "sse".into(),
                root: PathBuf::new(),
                spool_dir: PathBuf::new(),
                entry: String::new(),
                entry_service: Some("api".into()),
                services: BTreeMap::from([("api".into(), upstream_port)]),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        let mut client = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        client
            .write_all(
                format!("GET /__run/sse/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        header(&mut client).await;
        let mut first = [0; 13];
        tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut first))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&first, b"data: first\n\n");
        finish_tx.send(()).unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        assert_eq!(rest, b"data: last\n\n");
        server.await.unwrap();
        cancel.cancel();
        task.await.unwrap();
        assert_eq!(
            rewrite_location("/login?next=home", "/__run/x/__service/api/", 32100),
            "/__run/x/__service/api/login?next=home"
        );
        assert_eq!(
            rewrite_location("http://127.0.0.1:32100/login", "/__run/x/", 32100),
            "/__run/x/login"
        );
        assert_eq!(
            rewrite_location("https://external.example/login", "/__run/x/", 32100),
            "https://external.example/login"
        );
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[tokio::test]
    async fn rejects_cross_origin_and_ambiguous_body_and_serves_host_csp() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.html"), "safe").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "scope".into(),
                root: root.path().into(),
                spool_dir: root.path().into(),
                entry: "index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        for (headers, code) in [
            ("Origin: https://evil.example\r\n", 403),
            ("Content-Length: 2\r\nTransfer-Encoding: chunked\r\n", 400),
            ("Content-Length: 1\r\nContent-Length: 1\r\n", 400),
            ("Transfer-Encoding: gzip\r\n", 400),
            ("", 200),
        ] {
            let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            socket
                .write_all(
                    format!(
                        "GET /__run/scope/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {code}")));
            if code == 200 {
                assert!(response.contains("Content-Security-Policy:"));
                assert!(response.contains("worker-src 'self'"));
                assert!(response.contains("frame-ancestors 'none'"));
                assert!(response.contains("Permissions-Policy: camera=(), microphone=()"));
                assert!(!response.contains(" https:"));
            }
        }
        cancel.cancel();
        task.await.unwrap();
    }
}

#[cfg(test)]
mod absolute_path_tests {
    use super::*;
    #[tokio::test]
    async fn root_absolute_urls_redirect_only_from_current_epoch_and_keep_post_semantics() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("dist")).unwrap();
        std::fs::write(root.path().join("dist/index.html"), "fixture").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let task = spawn(
            listener,
            Route {
                host: None,
                run_id: "current".into(),
                root: root.path().into(),
                spool_dir: root.path().into(),
                entry: "dist/index.html".into(),
                entry_service: None,
                services: BTreeMap::new(),
                port,
                capabilities: vec![],
                cancel: cancel.clone(),
            },
        );
        for (referrer, code) in [
            (
                format!("http://127.0.0.1:{port}/__run/current/dist/index.html"),
                307,
            ),
            (
                format!("http://127.0.0.1:{port}/__run/old/dist/index.html"),
                410,
            ),
            (String::new(), 410),
        ] {
            let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            socket.write_all(format!("POST /assets/app.js?q=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nReferer: {referrer}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
            let mut response = Vec::new();
            socket.read_to_end(&mut response).await.unwrap();
            let response = String::from_utf8(response).unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {code}")));
            if code == 307 {
                assert!(response.contains("Location: /__run/current/dist/assets/app.js?q=1"));
            }
        }
        let response = super::tests::exchange(
            port,
            format!(
                "GET /__host/webmcp.js?cache=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nReferer: http://127.0.0.1:{port}/__run/current/dist/index.html\r\n\r\n"
            )
            .into_bytes(),
        )
        .await;
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 307"));
        assert!(response.contains("Location: /__run/current/__host/webmcp.js?cache=1"));
        cancel.cancel();
        task.await.unwrap();
    }
}

#[cfg(test)]
mod webmcp_injection_tests {
    use super::*;

    #[test]
    fn script_injection_handles_head_without_head_and_duplicate_source() {
        let with_head = "<!doctype html><html><head data-note=\"left>right\"><script id=\"app-start\"></script></head><body></body></html>";
        let injected = inject_webmcp_script(with_head, "run-1").unwrap();
        let script = "<script src=\"/__run/run-1/__host/webmcp.js\" data-lingxi-webmcp></script>";
        assert_eq!(injected.matches("data-lingxi-webmcp").count(), 1);
        assert!(injected.find(script).unwrap() < injected.find("id=\"app-start\"").unwrap());

        let no_head = "<!doctype html><script id=\"app-start\"></script>";
        let injected = inject_webmcp_script(no_head, "run-2").unwrap();
        assert!(injected.starts_with("<!doctype html><script src=\"/__run/run-2/__host/webmcp.js\""));
        assert!(injected.find("webmcp.js").unwrap() < injected.find("id=\"app-start\"").unwrap());

        let no_document_tags = "<script id=\"app-start\"></script>";
        let injected = inject_webmcp_script(no_document_tags, "run-3").unwrap();
        assert!(injected.starts_with("<script src=\"/__run/run-3/__host/webmcp.js\""));

        let already_loaded = "<html><head><script src=\"/__host/webmcp.js?cache=1\"></script></head></html>";
        assert!(inject_webmcp_script(already_loaded, "run-4").is_none());

        let current_run_loaded = "<script src=\"/__run/run-5/__host/webmcp.js\"></script>";
        assert!(inject_webmcp_script(current_run_loaded, "run-5").is_none());

        let late_head = "<!doctype html><script id=\"app-start\"></script><head></head>";
        let late_head_injected = inject_webmcp_script(late_head, "run-11").unwrap();
        let late_head_script =
            "<script src=\"/__run/run-11/__host/webmcp.js\" data-lingxi-webmcp></script>";
        let doctype_end = "<!doctype html>".len();
        assert!(late_head_injected.starts_with(&format!(
            "{}{}",
            &late_head[..doctype_end],
            late_head_script
        )));
        assert!(
            late_head_injected.find(late_head_script).unwrap()
                < late_head_injected.find("id=\"app-start\"").unwrap()
        );

        let vt_prefix = "\u{000b}<!doctype html><script id=\"app-start\"></script>";
        let vt_injected = inject_webmcp_script(vt_prefix, "run-16").unwrap();
        assert!(vt_injected.starts_with("<script src=\"/__run/run-16/__host/webmcp.js\""));
    }

    #[test]
    fn comments_and_script_or_style_strings_do_not_suppress_injection() {
        let html = "<!doctype html><!-- data-lingxi-webmcp <script src=\"/__host/webmcp.js\"></script> --><script>const fake='<script src=\"/__run/run-6/__host/webmcp.js\">'; const marker='data-lingxi-webmcp';</script><style>body::after{content:'__host/webmcp.js'}</style><p>app</p>";
        let injected = inject_webmcp_script(html, "run-6").unwrap();
        let script = "<script src=\"/__run/run-6/__host/webmcp.js\" data-lingxi-webmcp></script>";
        assert_eq!(injected.matches(script).count(), 1);
        assert!(injected.find(script).unwrap() < injected.find("<script>const fake").unwrap());
    }

    #[test]
    fn pseudo_head_in_attributes_and_raw_text_are_not_insertion_points_or_duplicates() {
        let html = "<!doctype html><!-- <head><script src=\"/__host/webmcp.js\"></script></head> --><div title=\"<head>\" data-route=\"/__run/run-7/__host/webmcp.js\"></div><script>const fakeHead='<head>'; const fakeSource='/__run/run-7/__host/webmcp.js';</script><style>.fake::after{content:'<head>'}</style><p>app</p>";
        let injected = inject_webmcp_script(html, "run-7").unwrap();
        let script = "<script src=\"/__run/run-7/__host/webmcp.js\" data-lingxi-webmcp></script>";
        let doctype_end = "<!doctype html>".len();
        assert!(injected.starts_with(&format!("{}{}", &html[..doctype_end], script)));
        assert_eq!(injected.matches(script).count(), 1);

        // A script-looking URL in an unrelated actual script path is not the
        // host resource and must not suppress run-scoped injection.
        let unrelated = "<script src=\"/other/__run/run-7/__host/webmcp.js\"></script>";
        assert!(inject_webmcp_script(unrelated, "run-7").is_some());
    }

    #[test]
    fn template_content_and_inert_raw_text_do_not_suppress_or_capture_injection() {
        let html = "<!doctype html><template><template><!-- </template><head> --><script>const fake='</template><head><script src=\"/__host/webmcp.js\">';</script><head><script src=\"/__host/webmcp.js\"></script></head></template></template><noscript><head><script src=\"/__host/webmcp.js\"></script></head></noscript><script id=\"app-start\"></script>";
        let injected = inject_webmcp_script(html, "run-8").unwrap();
        let script = "<script src=\"/__run/run-8/__host/webmcp.js\" data-lingxi-webmcp></script>";
        let doctype_end = "<!doctype html>".len();
        assert!(injected.starts_with(&format!("{}{}", &html[..doctype_end], script)));
        assert_eq!(injected.matches("data-lingxi-webmcp").count(), 1);
        assert!(injected.find(script).unwrap() < injected.find("id=\"app-start\"").unwrap());

        let minimal = "<!doctype html><template><head></head><p>x</p></template><script id=\"app-start\"></script>";
        let minimal_injected = inject_webmcp_script(minimal, "run-10").unwrap();
        let minimal_script =
            "<script src=\"/__run/run-10/__host/webmcp.js\" data-lingxi-webmcp></script>";
        assert!(minimal_injected.starts_with(&format!(
            "{}{}",
            &minimal[..doctype_end],
            minimal_script
        )));
        assert!(
            minimal_injected.find(minimal_script).unwrap()
                < minimal_injected.find("id=\"app-start\"").unwrap()
        );

        let template_only_script = "<!doctype html><template><script src=\"/__host/webmcp.js\"></script></template><script id=\"app-start\"></script>";
        assert!(inject_webmcp_script(template_only_script, "run-9").is_some());

        let double_escaped_script = "<!doctype html><script><!--<script></script><head><script src=\"/__host/webmcp.js\"></script></script><script id=\"app-start\"></script>";
        let double_escaped_injected =
            inject_webmcp_script(double_escaped_script, "run-12").unwrap();
        let double_escaped_host =
            "<script src=\"/__run/run-12/__host/webmcp.js\" data-lingxi-webmcp></script>";
        assert!(double_escaped_injected.starts_with(&format!(
            "{}{}",
            &double_escaped_script[..doctype_end],
            double_escaped_host
        )));
        assert!(
            double_escaped_injected.find(double_escaped_host).unwrap()
                < double_escaped_injected.find("id=\"app-start\"").unwrap()
        );

        let invalid_whitespace = "<!doctype html><script\u{00a0}src=\"/__host/webmcp.js\"></script><script\u{000b}src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>";
        assert!(inject_webmcp_script(invalid_whitespace, "run-15").is_some());
    }

    #[test]
    fn only_an_early_blocking_classic_first_src_counts_as_preloaded_webmcp() {
        let root = "/__host/webmcp.js";
        for markup in [
            "<script type=\"application/json\" src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script async src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script defer src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script type=\"module\" src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script type=\"text/javascript\u{00a0}\" src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script type=\"text/javascript\u{000b}\" src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<base href=\"https://other.example/\"><script src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script src=\"/other.js\" src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script id=\"app-start\"></script><script src=\"/__host/webmcp.js\"></script>",
            "<script async src=\"/app.js\"></script><script src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
            "<script type=\"module\" async src=\"/app.js\"></script><script src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>",
        ] {
            let html = format!("<!doctype html>{markup}");
            assert!(inject_webmcp_script(&html, "run-13").is_some(), "{markup}");
        }
        let already_blocking = format!(
            "<!doctype html><script src=\"{root}\"></script><script id=\"app-start\"></script>"
        );
        assert!(inject_webmcp_script(&already_blocking, "run-14").is_none());

        for ignored_base in [
            "<base href=\"data:text/html,\"><script src=\"/__host/webmcp.js\"></script>",
            "<base href=\"javascript:alert(1)\"><script src=\"/__host/webmcp.js\"></script>",
            "<base href=\"http[\"><script src=\"/__host/webmcp.js\"></script>",
        ] {
            let html = format!("<!doctype html>{ignored_base}<script id=\"app-start\"></script>");
            assert!(inject_webmcp_script(&html, "run-14").is_none(), "{ignored_base}");
        }

        for first_base in [
            "data:text/html,",
            "javascript:alert(1)",
            "http://[",
        ] {
            let html = format!(
                "<!doctype html><base href=\"{first_base}\"><base href=\"https://example.invalid/assets/\"><script src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>"
            );
            assert!(inject_webmcp_script(&html, "run-15").is_none(), "{first_base}");
        }

        for valid_override in [
            "mailto:x",
            "about:blank",
            "lingxi-app://app/custom/",
            "foo:opaque",
        ] {
            let html = format!(
                "<!doctype html><base href=\"{valid_override}\"><script src=\"/__host/webmcp.js\"></script><script id=\"app-start\"></script>"
            );
            assert!(inject_webmcp_script(&html, "run-16").is_some(), "{valid_override}");
        }
    }
}

#[cfg(test)]
mod agent_json_tests {
    use super::*;
    use base64::Engine as _;

    #[tokio::test]
    async fn request_body_spools_in_chunks_and_drops_tempfiles_on_timeout_or_eof() {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_path = scratch.path().canonicalize().unwrap();
        let spool_path = scratch_path.join("host-owned-agent-spool");
        ensure_host_directory(&spool_path, true).unwrap();
        assert_ne!(spool_path, std::env::temp_dir());
        let spool_dir = spool_path.as_path();
        let (mut reader, mut writer) = tokio::io::duplex(3);
        let streamed = b"streamed payload";
        let writer_task = tokio::spawn(async move {
            writer.write_all(streamed).await.unwrap();
        });
        let mut file = spool_fixed_body(&mut reader, b"prefix:", 7 + streamed.len(), spool_dir)
            .await
            .unwrap();
        writer_task.await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
        let mut spooled_bytes = Vec::new();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_to_end(&mut spooled_bytes).unwrap();
        assert_eq!(&spooled_bytes, b"prefix:streamed payload");
        drop(spooled_bytes);
        drop(file);
        assert_eq!(std::fs::read_dir(spool_dir).unwrap().count(), 0);

        let (mut hanging, _sink) = tokio::io::duplex(1);
        assert!(tokio::time::timeout(
            Duration::from_millis(5),
            spool_fixed_body(&mut hanging, b"prefix:", 1024, spool_dir)
        )
        .await
        .is_err());
        assert_eq!(std::fs::read_dir(spool_dir).unwrap().count(), 0);

        let (mut truncated, mut sink) = tokio::io::duplex(1);
        sink.write_all(b"x").await.unwrap();
        drop(sink);
        assert_eq!(
            spool_fixed_body(&mut truncated, b"", 2, spool_dir)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        assert_eq!(std::fs::read_dir(spool_dir).unwrap().count(), 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let public_dir = scratch_path.join("not-private");
            std::fs::create_dir(&public_dir).unwrap();
            std::fs::set_permissions(&public_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let (mut empty, _sink) = tokio::io::duplex(1);
            assert_eq!(
                spool_fixed_body(&mut empty, b"", 1, &public_dir)
                    .await
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }

    #[test]
    fn app_spool_directory_is_private_host_storage_and_rejects_symlinks_or_weak_modes() {
        let root = tempfile::tempdir().unwrap();
        let library_root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(library_root.join("apps/app-1")).unwrap();
        let spool = prepare_agent_spool_dir(&library_root, "app-1").unwrap();
        assert!(spool.ends_with("apps/app-1/agent/assets/upload-spool"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&spool).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        std::fs::create_dir_all(library_root.join("apps/app-2/agent/assets/upload-spool")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                library_root.join("apps/app-2/agent/assets/upload-spool"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        assert!(prepare_agent_spool_dir(&library_root, "app-2").is_err());
        assert!(prepare_agent_spool_dir(&library_root, "../outside").is_err());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let external = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(library_root.join("apps/app-3")).unwrap();
            symlink(external.path(), library_root.join("apps/app-3/agent")).unwrap();
            assert!(prepare_agent_spool_dir(&library_root, "app-3").is_err());
        }
    }

    #[test]
    fn full_official_input_and_large_inline_document_are_preserved_as_json() {
        let bytes = vec![0x5a; 1024 * 1024 + 17];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let expected_input = serde_json::json!({
            "threadId": "thread-1",
            "runId": "run-1",
            "protocolVersion": "1.0",
            "messages": [{
                "id": "message-1",
                "role": "user",
                "content": [
                    {"type": "text", "text": "Inspect this file"},
                    {
                        "type": "document",
                        "id": "part-1",
                        "source": {
                            "type": "data",
                            "value": encoded,
                            "mimeType": "application/octet-stream"
                        },
                        "metadata": {
                            "com.lingxi.localApp": {
                                "version": 1,
                                "filename": "sample.bin"
                            }
                        }
                    }
                ]
            }],
            "forwardedProps": {
                "com.lingxi.localApp": {"version": 1}
            }
        });
        let wire = serde_json::json!({
            "method": "agent.tasks.start",
            "args": {"input": expected_input}
        });
        let body = serde_json::to_vec(&wire).unwrap();
        assert!(body.len() > 1024 * 1024);
        assert!(body.len() < MAX_AGENT_BODY);

        let request = parse_page_request(std::io::Cursor::new(body)).unwrap();
        assert_eq!(request.method, "agent.tasks.start");
        let args = serde_json::from_str::<serde_json::Value>(request.args.get()).unwrap();
        assert_eq!(args["input"], expected_input);
        assert_eq!(
            args["input"]["messages"][0]["content"][1]["metadata"]["com.lingxi.localApp"]
                ["filename"],
            "sample.bin"
        );
    }

    #[test]
    fn request_parser_rejects_unknown_fields_trailing_data_and_excessive_nesting() {
        let unknown = br#"{"method":"agent.tasks.start","args":{},"unexpected":true}"#;
        assert!(parse_page_request(std::io::Cursor::new(unknown)).is_err());

        let trailing = br#"{"method":"agent.tasks.events","args":{}} false"#;
        assert!(parse_page_request(std::io::Cursor::new(trailing)).is_err());

        let nested = format!(
            "{{\"method\":\"agent.tasks.events\",\"args\":{}}}",
            format!("{}null{}", "[".repeat(160), "]".repeat(160))
        );
        assert!(parse_page_request(std::io::Cursor::new(nested)).is_err());
    }

    #[test]
    fn json_depth_limit_ignores_brackets_inside_strings_and_caps_containers() {
        assert!(validate_agent_json_depth(r#"{"text":"[[[[{{{{}}}}]]]]"}"#).is_ok());
        let at_limit = format!(
            "{}0{}",
            "[".repeat(MAX_AGENT_JSON_DEPTH),
            "]".repeat(MAX_AGENT_JSON_DEPTH)
        );
        let over_limit = format!(
            "{}0{}",
            "[".repeat(MAX_AGENT_JSON_DEPTH + 1),
            "]".repeat(MAX_AGENT_JSON_DEPTH + 1)
        );
        assert!(validate_agent_json_depth(&at_limit).is_ok());
        assert!(validate_agent_json_depth(&over_limit).is_err());
    }

    #[test]
    fn page_allowlist_has_standard_event_reads_but_no_native_event_operation() {
        assert!(is_page_method_allowed("agent.tasks.events"));
        assert!(is_page_method_allowed("agent.tasks.start"));
        assert!(is_page_method_allowed("agent.chats.send"));
        assert!(!is_page_method_allowed("eventsNative"));
        assert!(!is_page_method_allowed("agent.eventsNative"));
        assert!(!is_page_method_allowed("agent.run"));
    }

    #[test]
    fn gateway_accepts_only_the_json_agent_envelope_content_type() {
        assert!(is_agent_json_content_type(b"application/json"));
        assert!(is_agent_json_content_type(b"APPLICATION/JSON"));
        assert!(!is_agent_json_content_type(
            b"application/json; charset=utf-8"
        ));
        assert!(!is_agent_json_content_type(
            b"multipart/form-data; boundary=test"
        ));
        assert_eq!(MAX_AGENT_BODY, 116 * 1024 * 1024);
        assert_eq!(MAX_AGENT_JSON_DEPTH, 128);
    }

    #[test]
    fn sdk_bootstrap_loads_the_pinned_agui_global_before_the_host_adapter() {
        let bundle = include_str!("../../../../../packages/local-app-agui/dist/browser.global.js");
        let script = browser_sdk_script(r#"{"appId":"fixture"}"#);
        assert!(script.starts_with(bundle));
        let global_export = script
            .find("globalThis.LingxiLocalAppAGUI = LingxiLocalAppAGUI;")
            .unwrap();
        let host_adapter = script.find("const config =").unwrap();
        assert!(global_export >= bundle.len());
        assert!(global_export < host_adapter);
        assert!(host_adapter >= bundle.len());
        assert!(script[bundle.len()..].contains("installBrowserAgent"));
        assert!(!script.contains("__LINGXI_APP_CONFIGURATION__"));
    }
}

#[cfg(test)]
mod agent_control_lane_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::task::JoinSet;

    struct TestAdmission {
        heavy: Semaphore,
        control: Semaphore,
    }

    struct DelayedAgent {
        chat_started: tokio::sync::Notify,
        release_chat: Semaphore,
        delay_events: AtomicBool,
        events_started: AtomicUsize,
        eight_events_started: tokio::sync::Notify,
        release_events: Semaphore,
        cancelled: AtomicBool,
        dispatch_count: AtomicUsize,
    }

    impl DelayedAgent {
        fn new() -> Self {
            Self {
                chat_started: tokio::sync::Notify::new(),
                release_chat: Semaphore::new(0),
                delay_events: AtomicBool::new(false),
                events_started: AtomicUsize::new(0),
                eight_events_started: tokio::sync::Notify::new(),
                release_events: Semaphore::new(0),
                cancelled: AtomicBool::new(false),
                dispatch_count: AtomicUsize::new(0),
            }
        }

        async fn dispatch(
            &self,
            host: BrowserHost,
            run_id: String,
            method: String,
            arguments: String,
        ) -> Option<String> {
            if host.app_id != "authorized-app" || run_id != "epoch" {
                return None;
            }
            self.dispatch_count.fetch_add(1, Ordering::SeqCst);
            let arguments: serde_json::Value = serde_json::from_str(&arguments).ok()?;
            let value = match method.as_str() {
                "agent.chats.send" => {
                    if arguments["chatId"] != "chat-1" || arguments["input"]["runId"] != "run-1" {
                        return None;
                    }
                    self.chat_started.notify_one();
                    self.release_chat.acquire().await.ok()?.forget();
                    serde_json::json!({"runId":"run-1"})
                }
                "agent.tasks.get" => {
                    if arguments["runId"] != "run-1" {
                        return None;
                    }
                    serde_json::json!({
                        "runId":"run-1",
                        "state":if self.cancelled.load(Ordering::SeqCst) {"cancelled"} else {"running"}
                    })
                }
                "agent.tasks.events" => {
                    if arguments["runId"] != "run-1" {
                        return None;
                    }
                    if self.delay_events.load(Ordering::SeqCst) {
                        let started = self.events_started.fetch_add(1, Ordering::SeqCst) + 1;
                        if started == 8 {
                            self.eight_events_started.notify_one();
                        }
                        self.release_events.acquire().await.ok()?.forget();
                    }
                    serde_json::json!({"events":[],"sequence":1})
                }
                "agent.tasks.list" => serde_json::json!({"tasks":[]}),
                "agent.tasks.cancel" => {
                    if arguments["runId"] != "run-1" {
                        return None;
                    }
                    self.cancelled.store(true, Ordering::SeqCst);
                    serde_json::Value::Null
                }
                _ => return None,
            };
            Some(serde_json::json!({"ok":true,"value":value}).to_string())
        }
    }

    fn spawn_test_gateway(
        listener: TcpListener,
        route: Route,
        admission: Arc<TestAdmission>,
        agent: Arc<DelayedAgent>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let listener = Arc::new(listener);
            let route = Arc::new(route);
            let permits = Arc::new(Semaphore::new(32));
            loop {
                let socket = tokio::select! {
                    _ = route.cancel.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((socket, peer)) if peer.ip().is_loopback() => socket,
                        Ok(_) => continue,
                        Err(_) => break,
                    },
                };
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    continue;
                };
                let route = route.clone();
                let admission = admission.clone();
                let agent = agent.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    tokio::select! {
                        _ = route.cancel.cancelled() => {},
                        _ = serve_with_agent_dispatch(
                            socket,
                            &route,
                            &admission.heavy,
                            &admission.control,
                            move |host, run_id, method, arguments| {
                                let agent = agent.clone();
                                async move { agent.dispatch(host, run_id, method, arguments).await }
                            },
                        ) => {},
                    }
                });
            }
        })
    }

    async fn post_body(port: u16, origin: &str, body: &str) -> String {
        let mut socket = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        socket
            .write_all(
                format!(
                    "POST /__run/epoch/__host/agent HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {origin}\r\nReferer: http://127.0.0.1:{port}/__run/epoch/index.html\r\nContent-Type: application/json\r\nX-LingXi-App-Request: 1\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut output = Vec::new();
        let read_result =
            tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut output))
                .await
                .unwrap();
        if let Err(error) = read_result {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            assert!(
                !output.is_empty(),
                "connection reset before the gateway response arrived"
            );
        }
        String::from_utf8(output).unwrap()
    }

    async fn post_json(port: u16, method: &str, args: serde_json::Value) -> String {
        post_body(
            port,
            &format!("http://127.0.0.1:{port}"),
            &serde_json::json!({"method":method,"args":args}).to_string(),
        )
        .await
    }

    fn status(response: &str) -> u16 {
        response
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }

    fn json_body(response: &str) -> serde_json::Value {
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[tokio::test]
    async fn slow_chat_send_keeps_bounded_control_requests_live_and_rejects_oversized_controls() {
        let temp = tempfile::tempdir().unwrap();
        let temp_root = temp.path().canonicalize().unwrap();
        let spool_path = temp_root.join("host-owned-agent-spool");
        ensure_host_directory(&spool_path, true).unwrap();
        let spool_dir = spool_path.canonicalize().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancel = CancellationToken::new();
        let route = Route {
            host: Some(BrowserHost {
                owner: Weak::new(),
                app_id: "authorized-app".into(),
                name: "Test App".into(),
                revision: 1,
                platform: "android".into(),
            }),
            run_id: "epoch".into(),
            root: temp.path().into(),
            spool_dir: spool_dir.clone(),
            entry: "index.html".into(),
            entry_service: None,
            services: BTreeMap::new(),
            port,
            capabilities: Vec::new(),
            cancel: cancel.clone(),
        };
        let admission = Arc::new(TestAdmission {
            heavy: Semaphore::new(1),
            control: Semaphore::new(8),
        });
        let agent = Arc::new(DelayedAgent::new());
        let server = spawn_test_gateway(listener, route, admission, agent.clone());

        let chat_send = tokio::spawn(post_json(
            port,
            "agent.chats.send",
            serde_json::json!({
                "chatId":"chat-1",
                "input":{"runId":"run-1"}
            }),
        ));
        if tokio::time::timeout(Duration::from_secs(2), agent.chat_started.notified())
            .await
            .is_err()
        {
            if chat_send.is_finished() {
                panic!(
                    "chat.send did not reach dispatch: {}",
                    chat_send.await.unwrap()
                );
            }
            panic!("chat.send remained pending without entering dispatch");
        }
        assert!(!chat_send.is_finished());

        let get = post_json(
            port,
            "agent.tasks.get",
            serde_json::json!({"runId":"run-1"}),
        )
        .await;
        assert_eq!(status(&get), 200);
        assert_eq!(json_body(&get)["value"]["state"], "running");
        let events = post_json(
            port,
            "agent.tasks.events",
            serde_json::json!({"runId":"run-1","afterSequence":0}),
        )
        .await;
        assert_eq!(status(&events), 200);
        assert_eq!(json_body(&events)["value"]["sequence"], 1);
        let list = post_json(port, "agent.tasks.list", serde_json::json!({"limit":10})).await;
        assert_eq!(status(&list), 200);
        assert_eq!(json_body(&list)["value"]["tasks"], serde_json::json!([]));
        let cancel_response = post_json(
            port,
            "agent.tasks.cancel",
            serde_json::json!({"runId":"run-1"}),
        )
        .await;
        assert_eq!(status(&cancel_response), 200);
        assert_eq!(
            json_body(&cancel_response)["value"],
            serde_json::Value::Null
        );
        let cancelled = post_json(
            port,
            "agent.tasks.get",
            serde_json::json!({"runId":"run-1"}),
        )
        .await;
        assert_eq!(status(&cancelled), 200);
        assert_eq!(json_body(&cancelled)["value"]["state"], "cancelled");

        let competing_heavy = serde_json::json!({
            "method":"agent.tasks.start",
            "args":{"input":{"padding":"x".repeat(MAX_AGENT_CONTROL_BODY)}}
        })
        .to_string();
        assert!(competing_heavy.len() > MAX_AGENT_CONTROL_BODY);
        let competing_response =
            post_body(port, &format!("http://127.0.0.1:{port}"), &competing_heavy).await;
        assert_eq!(status(&competing_response), 429);
        assert_eq!(std::fs::read_dir(&spool_dir).unwrap().count(), 0);

        let call_count = agent.dispatch_count.load(Ordering::SeqCst);
        let unauthorized_body = serde_json::json!({
            "method":"agent.tasks.events",
            "args":{"runId":"run-1","padding":"x".repeat(MAX_AGENT_CONTROL_BODY)}
        })
        .to_string();
        assert!(unauthorized_body.len() > MAX_AGENT_CONTROL_BODY);
        let unauthorized = post_body(port, "https://untrusted.example", &unauthorized_body).await;
        assert_eq!(status(&unauthorized), 403);
        assert_eq!(agent.dispatch_count.load(Ordering::SeqCst), call_count);
        assert_eq!(std::fs::read_dir(&spool_dir).unwrap().count(), 0);

        agent.delay_events.store(true, Ordering::SeqCst);
        let mut event_requests = JoinSet::new();
        for _ in 0..9 {
            event_requests.spawn(post_json(
                port,
                "agent.tasks.events",
                serde_json::json!({"runId":"run-1","afterSequence":0}),
            ));
        }
        tokio::time::timeout(
            Duration::from_secs(2),
            agent.eight_events_started.notified(),
        )
        .await
        .unwrap();
        let saturated = tokio::time::timeout(Duration::from_secs(2), event_requests.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(status(&saturated), 429);
        assert_eq!(agent.events_started.load(Ordering::SeqCst), 8);
        agent.release_events.add_permits(8);
        let mut released_control_responses = 0;
        while let Some(result) = event_requests.join_next().await {
            assert_eq!(status(&result.unwrap()), 200);
            released_control_responses += 1;
        }
        assert_eq!(released_control_responses, 8);

        agent.release_chat.add_permits(1);
        let chat_response = chat_send.await.unwrap();
        assert_eq!(status(&chat_response), 200);
        assert_eq!(json_body(&chat_response)["value"]["runId"], "run-1");

        let dispatch_count = agent.dispatch_count.load(Ordering::SeqCst);
        let oversized_control = serde_json::json!({
            "method":"agent.tasks.events",
            "args":{"runId":"run-1","padding":"x".repeat(MAX_AGENT_CONTROL_BODY)}
        })
        .to_string();
        assert!(oversized_control.len() > MAX_AGENT_CONTROL_BODY);
        let oversized = post_body(
            port,
            &format!("http://127.0.0.1:{port}"),
            &oversized_control,
        )
        .await;
        assert_eq!(status(&oversized), 413);
        assert_eq!(agent.dispatch_count.load(Ordering::SeqCst), dispatch_count);
        assert_eq!(std::fs::read_dir(&spool_dir).unwrap().count(), 0);

        cancel.cancel();
        server.await.unwrap();
    }
}
