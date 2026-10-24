//! Per-installation loopback gateway. A URL contains an unguessable run epoch;
//! every accepted connection is cancelled when that epoch is stopped.
use base64::Engine as _;
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder},
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
const MAX_AGENT_FILE_COUNT: usize = 8;
const MAX_AGENT_FILE_SIZE: usize = 25 * 1024 * 1024;
const MAX_AGENT_FILES_TOTAL: usize = 50 * 1024 * 1024;
const MAX_AGENT_METADATA: usize = 1024 * 1024;
const MAX_AGENT_BODY: usize = MAX_AGENT_FILES_TOTAL + 2 * 1024 * 1024;
const MAX_MULTIPART_PART_HEAD: usize = 16 * 1024;
// A single upload is decoded at a time across all Apps to bound base64 DTO memory.
static AGENT_REQUESTS: Semaphore = Semaphore::const_new(1);

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PageRequest {
    method: String,
    args: serde_json::Value,
}

struct MultipartPart<'a> {
    name: String,
    filename: Option<String>,
    media_type: Option<String>,
    body: &'a [u8],
}

struct SpooledPart {
    name: String,
    filename: Option<String>,
    media_type: Option<String>,
    body_start: u64,
    body_end: u64,
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

fn split_header_parameters(
    value: &str,
) -> Result<(String, BTreeMap<String, String>), &'static str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == ';' && !quoted {
            pieces.push(value[start..index].trim());
            start = index + ch.len_utf8();
        }
    }
    if quoted || escaped {
        return Err("invalid multipart header parameter");
    }
    pieces.push(value[start..].trim());
    let Some((kind, rest)) = pieces.split_first() else {
        return Err("invalid multipart header");
    };
    let mut parameters = BTreeMap::new();
    for piece in rest {
        let Some((name, raw)) = piece.split_once('=') else {
            return Err("invalid multipart header parameter");
        };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() || parameters.contains_key(&name) {
            return Err("duplicate multipart header parameter");
        }
        let raw = raw.trim();
        let parameter = if raw.starts_with('"') {
            if !raw.ends_with('"') || raw.len() < 2 {
                return Err("invalid quoted multipart parameter");
            }
            let mut output = String::with_capacity(raw.len() - 2);
            let mut chars = raw[1..raw.len() - 1].chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    let Some(escaped) = chars.next() else {
                        return Err("invalid quoted multipart parameter");
                    };
                    if escaped != '\\' && escaped != '"' {
                        return Err("invalid quoted multipart escape");
                    }
                    output.push(escaped);
                } else {
                    output.push(ch);
                }
            }
            output
        } else {
            if raw.is_empty()
                || raw
                    .bytes()
                    .any(|byte| byte.is_ascii_whitespace() || byte == b'"')
            {
                return Err("invalid multipart parameter");
            }
            raw.to_owned()
        };
        parameters.insert(name, parameter);
    }
    Ok((kind.trim().to_ascii_lowercase(), parameters))
}

fn multipart_boundary(content_type: &[u8]) -> Result<Vec<u8>, &'static str> {
    let content_type = std::str::from_utf8(content_type).map_err(|_| "invalid content type")?;
    let (kind, parameters) = split_header_parameters(content_type)?;
    if kind != "multipart/form-data" {
        return Err("Agent requires multipart/form-data");
    }
    let boundary = parameters
        .get("boundary")
        .ok_or("multipart boundary is missing")?;
    let bytes = boundary.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 70
        || bytes
            .iter()
            .any(|byte| !byte.is_ascii_graphic() || *byte == b'"')
    {
        return Err("invalid multipart boundary");
    }
    Ok(bytes.to_vec())
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() || needle.len() > haystack.len() - from {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

fn parse_disposition(value: &[u8]) -> Result<(String, Option<String>), &'static str> {
    let value = std::str::from_utf8(value).map_err(|_| "invalid multipart disposition")?;
    let (kind, parameters) = split_header_parameters(value)?;
    if kind != "form-data" {
        return Err("invalid multipart disposition");
    }
    let name = parameters
        .get("name")
        .cloned()
        .ok_or("multipart field name is missing")?;
    let filename = parameters.get("filename").cloned();
    if parameters
        .keys()
        .any(|key| key != "name" && key != "filename")
    {
        return Err("unsupported multipart disposition parameter");
    }
    Ok((name, filename))
}

fn valid_media_type(value: &str) -> bool {
    let mut parts = value.split('/');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !major.is_empty()
        && !minor.is_empty()
        && major
            .bytes()
            .chain(minor.bytes())
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
}

fn parse_part<'a>(headers: &[u8], body: &'a [u8]) -> Result<MultipartPart<'a>, &'static str> {
    let headers = std::str::from_utf8(headers).map_err(|_| "invalid multipart part headers")?;
    let mut disposition = None;
    let mut media_type = None;
    for line in headers.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            return Err("invalid multipart part header");
        };
        let value = value.trim().as_bytes();
        if name.eq_ignore_ascii_case("content-disposition") {
            if disposition.replace(value).is_some() {
                return Err("duplicate multipart disposition");
            }
        } else if name.eq_ignore_ascii_case("content-type") {
            if media_type.replace(value).is_some() {
                return Err("duplicate multipart content type");
            }
        } else {
            return Err("unsupported multipart part header");
        }
    }
    let (name, filename) =
        parse_disposition(disposition.ok_or("multipart disposition is missing")?)?;
    let media_type = match media_type {
        Some(value) => Some(
            std::str::from_utf8(value)
                .map_err(|_| "invalid multipart content type")?
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase(),
        ),
        None => None,
    };
    if let Some(media_type) = &media_type {
        if !valid_media_type(media_type) {
            return Err("invalid multipart content type");
        }
    }
    Ok(MultipartPart {
        name,
        filename,
        media_type,
        body,
    })
}

fn parse_multipart_parts<'a>(
    body: &'a [u8],
    boundary: &[u8],
) -> Result<Vec<MultipartPart<'a>>, &'static str> {
    let mut first = b"--".to_vec();
    first.extend_from_slice(boundary);
    first.extend_from_slice(b"\r\n");
    if !body.starts_with(&first) {
        return Err("invalid multipart body start");
    }
    let mut marker = b"\r\n--".to_vec();
    marker.extend_from_slice(boundary);
    let mut cursor = first.len();
    let mut parts = Vec::new();
    loop {
        let headers_end = find_bytes(body, b"\r\n\r\n", cursor)
            .filter(|end| *end - cursor <= MAX_MULTIPART_PART_HEAD)
            .ok_or("multipart part headers are missing or too large")?;
        let part_body_start = headers_end + 4;
        let mut search = part_body_start;
        let (delimiter, suffix) = loop {
            let candidate =
                find_bytes(body, &marker, search).ok_or("multipart terminator is missing")?;
            let suffix_at = candidate + marker.len();
            if body.get(suffix_at..suffix_at + 2) == Some(b"\r\n") {
                break (candidate, false);
            }
            if body.get(suffix_at..suffix_at + 2) == Some(b"--") {
                break (candidate, true);
            }
            search = candidate + 1;
        };
        let part = parse_part(
            &body[cursor..headers_end],
            &body[part_body_start..delimiter],
        )?;
        parts.push(part);
        if parts.len() > MAX_AGENT_FILE_COUNT + 1 {
            return Err("too many multipart fields");
        }
        let suffix_at = delimiter + marker.len();
        if suffix {
            let tail = &body[suffix_at + 2..];
            if !tail.is_empty() && tail != b"\r\n" {
                return Err("unexpected multipart epilogue");
            }
            break;
        }
        cursor = suffix_at + 2;
    }
    Ok(parts)
}

fn safe_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename.len() <= 255
        && filename != "."
        && filename != ".."
        && !filename
            .chars()
            .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
}

fn validate_file_size(size: usize, total: &mut usize) -> Result<(), &'static str> {
    if size > MAX_AGENT_FILE_SIZE {
        return Err("Agent attachment exceeds the per-file limit");
    }
    *total = total
        .checked_add(size)
        .filter(|total| *total <= MAX_AGENT_FILES_TOTAL)
        .ok_or("Agent attachments exceed the total limit")?;
    Ok(())
}

fn parse_agent_multipart(parts: Vec<MultipartPart<'_>>) -> Result<PageRequest, &'static str> {
    let mut metadata = None;
    let mut files = Vec::new();
    let mut total = 0;
    for part in parts {
        match part.name.as_str() {
            "request" => {
                if metadata.is_some()
                    || part.filename.is_some()
                    || part.body.len() > MAX_AGENT_METADATA
                {
                    return Err("invalid or duplicate Agent metadata field");
                }
                if part
                    .media_type
                    .as_deref()
                    .is_some_and(|value| value != "application/json" && value != "text/plain")
                {
                    return Err("Agent metadata must be JSON or plain text");
                }
                metadata = Some(part.body);
            }
            "attachment" => {
                let filename = part.filename.ok_or("attachment filename is missing")?;
                if !safe_filename(&filename) {
                    return Err("attachment filename must be a safe leaf name");
                }
                validate_file_size(part.body.len(), &mut total)?;
                files.push((
                    filename,
                    part.media_type
                        .unwrap_or_else(|| "application/octet-stream".into()),
                    part.body,
                ));
                if files.len() > MAX_AGENT_FILE_COUNT {
                    return Err("too many Agent attachments");
                }
            }
            _ => return Err("unknown multipart field"),
        }
    }
    let metadata = metadata.ok_or("Agent metadata field is missing")?;
    let mut request: PageRequest =
        serde_json::from_slice(metadata).map_err(|_| "invalid Agent metadata JSON")?;
    let accepts_files = matches!(
        request.method.as_str(),
        "agent.tasks.start" | "agent.chats.send"
    );
    if !files.is_empty() && !accepts_files {
        return Err("attachments are only accepted with a task or chat send");
    }
    if accepts_files {
        let input = request
            .args
            .get_mut("input")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or("Agent input is missing")?;
        if input.contains_key("attachments") || input.contains_key("images") {
            return Err("attachments must be sent as multipart file parts");
        }
        if !files.is_empty() {
            let normalized = files
                .into_iter()
                .map(|(name, media_type, bytes)| {
                    serde_json::json!({
                        "name": name,
                        "mediaType": media_type,
                        "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                    })
                })
                .collect();
            input.insert("attachments".into(), serde_json::Value::Array(normalized));
        }
    }
    Ok(request)
}

fn read_part_headers(
    file: &mut std::fs::File,
    start: u64,
    length: u64,
) -> Result<(Vec<u8>, u64), &'static str> {
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "could not seek multipart headers")?;
    let mut header = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let count = file
            .read(&mut chunk)
            .map_err(|_| "could not read multipart headers")?;
        if count == 0 {
            return Err("multipart part headers are missing");
        }
        header.extend_from_slice(&chunk[..count]);
        if let Some(end) = find_bytes(&header, b"\r\n\r\n", 0) {
            if end > MAX_MULTIPART_PART_HEAD {
                return Err("multipart part headers are too large");
            }
            let body_start = start + end as u64 + 4;
            if body_start > length {
                return Err("multipart part headers exceed the request body");
            }
            header.truncate(end);
            file.seek(SeekFrom::Start(body_start))
                .map_err(|_| "could not seek multipart body")?;
            return Ok((header, body_start));
        }
        if header.len() > MAX_MULTIPART_PART_HEAD + 3 {
            return Err("multipart part headers are too large");
        }
    }
}

fn find_spooled_delimiter(
    file: &mut std::fs::File,
    length: u64,
    start: u64,
    marker: &[u8],
) -> Result<(u64, bool), &'static str> {
    if marker.is_empty() || start > length {
        return Err("invalid multipart delimiter scan");
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|_| "could not seek multipart body")?;
    let mut position = start;
    let mut carry = Vec::new();
    let mut chunk = [0_u8; 8192];
    while position < length {
        let limit = (length - position).min(chunk.len() as u64) as usize;
        let count = file
            .read(&mut chunk[..limit])
            .map_err(|_| "could not scan multipart body")?;
        if count == 0 {
            break;
        }
        let mut joined = Vec::with_capacity(carry.len() + count);
        joined.extend_from_slice(&carry);
        joined.extend_from_slice(&chunk[..count]);
        let joined_start = position - carry.len() as u64;
        let saved_cursor = position + count as u64;
        let mut search = 0;
        while let Some(candidate_offset) = find_bytes(&joined, marker, search) {
            let candidate = joined_start + candidate_offset as u64;
            let suffix_at = candidate + marker.len() as u64;
            if suffix_at + 2 <= length {
                let suffix_offset = (suffix_at - joined_start) as usize;
                let suffix = if suffix_offset + 2 <= joined.len() {
                    [joined[suffix_offset], joined[suffix_offset + 1]]
                } else {
                    file.seek(SeekFrom::Start(suffix_at))
                        .map_err(|_| "could not inspect multipart delimiter")?;
                    let mut suffix = [0_u8; 2];
                    file.read_exact(&mut suffix)
                        .map_err(|_| "could not inspect multipart delimiter")?;
                    file.seek(SeekFrom::Start(saved_cursor))
                        .map_err(|_| "could not resume multipart scan")?;
                    suffix
                };
                if suffix == *b"\r\n" {
                    return Ok((candidate, false));
                }
                if suffix == *b"--" {
                    return Ok((candidate, true));
                }
            }
            search = candidate_offset + 1;
        }
        let keep = marker.len().saturating_sub(1).min(joined.len());
        carry.clear();
        carry.extend_from_slice(&joined[joined.len() - keep..]);
        position = saved_cursor;
    }
    Err("multipart terminator is missing")
}

fn scan_multipart_spool(
    file: &mut std::fs::File,
    length: u64,
    boundary: &[u8],
) -> Result<Vec<SpooledPart>, &'static str> {
    let mut first = b"--".to_vec();
    first.extend_from_slice(boundary);
    first.extend_from_slice(b"\r\n");
    if first.len() as u64 > length {
        return Err("invalid multipart body start");
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| "could not seek multipart body")?;
    let mut opening = vec![0_u8; first.len()];
    file.read_exact(&mut opening)
        .map_err(|_| "invalid multipart body start")?;
    if opening != first {
        return Err("invalid multipart body start");
    }

    let mut marker = b"\r\n--".to_vec();
    marker.extend_from_slice(boundary);
    let mut cursor = first.len() as u64;
    let mut parts = Vec::new();
    loop {
        let (headers, body_start) = read_part_headers(file, cursor, length)?;
        let part = parse_part(&headers, &[])?;
        let (body_end, closing) = find_spooled_delimiter(file, length, body_start, &marker)?;
        parts.push(SpooledPart {
            name: part.name,
            filename: part.filename,
            media_type: part.media_type,
            body_start,
            body_end,
        });
        if parts.len() > MAX_AGENT_FILE_COUNT + 1 {
            return Err("too many multipart fields");
        }
        let suffix_at = body_end + marker.len() as u64;
        if closing {
            let tail_length = length.saturating_sub(suffix_at + 2);
            if tail_length == 2 {
                file.seek(SeekFrom::Start(suffix_at + 2))
                    .map_err(|_| "could not inspect multipart epilogue")?;
                let mut tail = [0_u8; 2];
                file.read_exact(&mut tail)
                    .map_err(|_| "could not inspect multipart epilogue")?;
                if tail != *b"\r\n" {
                    return Err("unexpected multipart epilogue");
                }
            } else if tail_length != 0 {
                return Err("unexpected multipart epilogue");
            }
            break;
        }
        cursor = suffix_at + 2;
        if cursor >= length {
            return Err("multipart terminator is missing");
        }
    }
    Ok(parts)
}

fn read_spooled_part(
    file: &mut std::fs::File,
    part: &SpooledPart,
    maximum: usize,
) -> Result<Vec<u8>, &'static str> {
    let length = usize::try_from(part.body_end.saturating_sub(part.body_start))
        .map_err(|_| "multipart part is too large")?;
    if length > maximum {
        return Err("multipart part exceeds its size limit");
    }
    file.seek(SeekFrom::Start(part.body_start))
        .map_err(|_| "could not seek multipart part")?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)
        .map_err(|_| "could not read multipart part")?;
    Ok(bytes)
}

fn parse_agent_spool(
    file: &mut std::fs::File,
    parts: Vec<SpooledPart>,
) -> Result<PageRequest, &'static str> {
    let mut metadata = None;
    let mut files = Vec::new();
    let mut total = 0;
    for part in &parts {
        let size = usize::try_from(part.body_end.saturating_sub(part.body_start))
            .map_err(|_| "multipart part is too large")?;
        match part.name.as_str() {
            "request" => {
                if metadata.is_some() || part.filename.is_some() || size > MAX_AGENT_METADATA {
                    return Err("invalid or duplicate Agent metadata field");
                }
                if part
                    .media_type
                    .as_deref()
                    .is_some_and(|value| value != "application/json" && value != "text/plain")
                {
                    return Err("Agent metadata must be JSON or plain text");
                }
                metadata = Some(part);
            }
            "attachment" => {
                let filename = part
                    .filename
                    .as_deref()
                    .ok_or("attachment filename is missing")?;
                if !safe_filename(filename) {
                    return Err("attachment filename must be a safe leaf name");
                }
                validate_file_size(size, &mut total)?;
                files.push(part);
                if files.len() > MAX_AGENT_FILE_COUNT {
                    return Err("too many Agent attachments");
                }
            }
            _ => return Err("unknown multipart field"),
        }
    }
    let metadata = metadata.ok_or("Agent metadata field is missing")?;
    let metadata = read_spooled_part(file, metadata, MAX_AGENT_METADATA)?;
    let mut request: PageRequest =
        serde_json::from_slice(&metadata).map_err(|_| "invalid Agent metadata JSON")?;
    drop(metadata);
    let accepts_files = matches!(
        request.method.as_str(),
        "agent.tasks.start" | "agent.chats.send"
    );
    if !files.is_empty() && !accepts_files {
        return Err("attachments are only accepted with a task or chat send");
    }
    if accepts_files {
        let input = request
            .args
            .get_mut("input")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or("Agent input is missing")?;
        if input.contains_key("attachments") || input.contains_key("images") {
            return Err("attachments must be sent as multipart file parts");
        }
        if !files.is_empty() {
            let mut normalized = Vec::with_capacity(files.len());
            for part in files {
                let name = part
                    .filename
                    .clone()
                    .ok_or("attachment filename is missing")?;
                let media_type = part
                    .media_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".into());
                let bytes = read_spooled_part(file, part, MAX_AGENT_FILE_SIZE)?;
                let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                drop(bytes);
                normalized.push(serde_json::json!({
                    "name": name,
                    "mediaType": media_type,
                    "base64": encoded,
                }));
            }
            input.insert("attachments".into(), serde_json::Value::Array(normalized));
        }
    }
    Ok(request)
}

fn write_json_value(
    value: serde_json::Value,
    output: &mut String,
) -> Result<(), serde_json::Error> {
    use serde_json::Value;
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value)
            if value
                .bytes()
                .all(|byte| byte >= 0x20 && byte != b'"' && byte != b'\\') =>
        {
            // Base64 and ordinary printable strings need no JSON escaping.
            // Move their allocation directly into the output instead of
            // retaining one full encoded DTO beside a second serialized copy.
            output.push('"');
            output.push_str(&value);
            output.push('"');
        }
        Value::String(value) => output.push_str(&serde_json::to_string(&value)?),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_json_value(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            for (index, (key, value)) in values.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                output.push_str(&serde_json::to_string(&key)?);
                output.push(':');
                write_json_value(value, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

fn compact_json(value: serde_json::Value) -> Result<String, serde_json::Error> {
    let mut output = String::new();
    write_json_value(value, &mut output)?;
    Ok(output)
}

async fn serve(mut socket: TcpStream, route: &Route) -> std::io::Result<()> {
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
            let target = if relative == "__local_app_sdk.js" {
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
    if relative == "__local_app_sdk.js" || relative == "__host/sdk.js" {
        if method != "GET" && method != "HEAD" {
            return error(&mut socket, 405, "SDK accepts GET and HEAD").await;
        }
        let Some(host) = &route.host else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        let config = serde_json::json!({"appId":host.app_id,"name":host.name,"revision":host.revision,"hostPlatform":host.platform,"runId":route.run_id,"capabilities":route.capabilities,"services":route.services.keys().collect::<Vec<_>>()});
        let script = include_str!("browser_sdk.js")
            .replace("__LINGXI_APP_CONFIGURATION__", &config.to_string());
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/javascript; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{}\r\n", script.len(), security_headers(route)).as_bytes()).await?;
        if method == "GET" {
            socket.write_all(script.as_bytes()).await?;
        }
        return Ok(());
    }
    if relative == "__host/recovery-sw.js" {
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
        let boundary = match multipart_boundary(content_types[0].value) {
            Ok(boundary) => boundary,
            Err(_) => return error(&mut socket, 400, "invalid Agent multipart content type").await,
        };
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
        let Ok(_upload_permit) = AGENT_REQUESTS.try_acquire() else {
            return error(&mut socket, 429, "application host is busy; retry later").await;
        };
        if length == 0 {
            return error(&mut socket, 400, "invalid request body").await;
        }
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
            let parts = match scan_multipart_spool(&mut spool, length as u64, &boundary) {
                Ok(parts) => parts,
                Err(_) => return error(&mut socket, 400, "invalid Agent multipart request").await,
            };
            match parse_agent_spool(&mut spool, parts) {
                Ok(input) => input,
                Err(_) => return error(&mut socket, 400, "invalid Agent request").await,
            }
        };
        // The raw wire body stays on the anonymous tempfile. Parsing scans it
        // in bounded chunks and reads one capped part at a time; the spool is
        // released before native JSON serialization. Timeout, parse rejection,
        // cancellation, and normal return all close it through RAII.
        if !matches!(
            input.method.as_str(),
            "agent.tasks.start"
                | "agent.tasks.get"
                | "agent.tasks.list"
                | "agent.tasks.cancel"
                | "agent.chats.create"
                | "agent.chats.get"
                | "agent.chats.list"
                | "agent.chats.archive"
                | "agent.chats.delete"
                | "agent.chats.history"
                | "agent.chats.send"
                | "agent.chats.cancel"
        ) {
            return error(
                &mut socket,
                403,
                "host operation is not available to applications",
            )
            .await;
        }
        let Some((host, owner)) = route
            .host
            .as_ref()
            .and_then(|host| host.owner.upgrade().map(|owner| (host, owner)))
        else {
            return error(&mut socket, 503, "application host unavailable").await;
        };
        let PageRequest { method, args } = input;
        let arguments = match compact_json(args) {
            Ok(arguments) => arguments,
            Err(_) => return error(&mut socket, 500, "could not serialize Agent request").await,
        };
        let result = owner
            .agent_page_json(host.app_id.clone(), route.run_id.clone(), method, arguments)
            .await;
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
    let content_type = match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
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
    };
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
    format!("Content-Security-Policy: sandbox allow-scripts allow-forms allow-modals allow-downloads allow-same-origin allow-top-navigation-to-custom-protocols; default-src 'self' data: blob:{remote_scripts}; script-src 'self' 'unsafe-inline' 'unsafe-eval' blob:{remote_scripts}; style-src 'self' 'unsafe-inline'{remote_scripts}; connect-src 'self' ws://127.0.0.1:{}{remote_sources}; worker-src 'self'; object-src 'none'; base-uri 'none'; frame-src 'none'; frame-ancestors 'none'\r\nPermissions-Policy: camera={camera}, microphone={microphone}, geolocation=()\r\nReferrer-Policy: same-origin\r\nX-Frame-Options: DENY\r\n", route.port)
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
    async fn exchange(port: u16, bytes: Vec<u8>) -> Vec<u8> {
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
        cancel.cancel();
        task.await.unwrap();
    }
}

#[cfg(test)]
mod agent_multipart_tests {
    use super::*;
    use std::io::Write;

    const BOUNDARY: &[u8] = b"agent-boundary";

    #[tokio::test]
    async fn upload_body_spools_in_chunks_and_drops_tempfiles_on_timeout_or_eof() {
        let scratch = tempfile::tempdir().unwrap();
        let scratch_path = scratch.path().canonicalize().unwrap();
        let spool_path = scratch_path.join("host-owned-upload-spool");
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

    fn multipart_request(metadata: &[u8], filename: &str, bytes: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"--agent-boundary\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n");
        body.extend_from_slice(metadata);
        body.extend_from_slice(b"\r\n--agent-boundary\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"");
        body.extend_from_slice(filename.as_bytes());
        body.extend_from_slice(b"\"\r\nContent-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n--agent-boundary--\r\n");
        body
    }

    fn parse_spooled(body: &[u8]) -> Result<PageRequest, &'static str> {
        let directory = tempfile::tempdir().map_err(|_| "could not create test spool dir")?;
        let mut spool =
            tempfile::tempfile_in(directory.path()).map_err(|_| "could not create test spool")?;
        spool
            .write_all(body)
            .map_err(|_| "could not write test spool")?;
        let parts = scan_multipart_spool(&mut spool, body.len() as u64, BOUNDARY)?;
        parse_agent_spool(&mut spool, parts)
    }

    #[test]
    fn spooled_parser_finds_split_delimiter_and_converts_one_binary_part() {
        let metadata = br#"{"method":"agent.tasks.start","args":{"input":{"requestId":"spooled-1","prompt":"describe"}}}"#;
        let header_prefix = b"--agent-boundary\r\nContent-Disposition: form-data; name=\"request\"\r\nContent-Type: application/json\r\n\r\n";
        let attachment_prefix = b"\r\n--agent-boundary\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"meal.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n";
        let body_start = header_prefix.len() + metadata.len() + attachment_prefix.len();
        let marker = b"\r\n--agent-boundary";
        let target = 8192 - marker.len() / 2;
        let padding = (target + 8192 - body_start % 8192) % 8192;
        let mut bytes = vec![b'x'; padding];
        bytes.extend_from_slice(b"\r\n--agent-boundaryXstill-file");
        let body = multipart_request(metadata, "meal.bin", &bytes);
        let request = parse_spooled(&body).unwrap();
        assert_eq!(request.args["input"]["requestId"], "spooled-1");
        assert_eq!(
            request.args["input"]["attachments"][0]["base64"],
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
    }

    #[test]
    fn spooled_parser_rejects_duplicate_headers_and_unsafe_filenames() {
        let metadata = br#"{"method":"agent.tasks.start","args":{"input":{"requestId":"req-1","prompt":"x"}}}"#;
        let path = multipart_request(metadata, "../private.txt", b"x");
        assert!(parse_spooled(&path).is_err());
        let duplicate = String::from_utf8(multipart_request(metadata, "x.txt", b"x"))
            .unwrap()
            .replace(
                "Content-Disposition: form-data; name=\"attachment\"; filename=\"x.txt\"",
                "Content-Disposition: form-data; name=\"attachment\"; filename=\"x.txt\"\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"y.txt\"",
            );
        assert!(parse_spooled(duplicate.as_bytes()).is_err());
    }

    #[test]
    fn compact_serializer_preserves_escaped_metadata_and_base64_payloads() {
        let value = serde_json::json!({
            "prompt": "line one\n\"quoted\" \\ path",
            "attachments": [{"name": "餐食.jpg", "base64": "YWJj+/=="}],
            "outputSchema": {"required": ["items", "range"]}
        });
        let expected = value.clone();
        let serialized = compact_json(value).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&serialized).unwrap(),
            expected
        );
    }

    #[test]
    fn multipart_binary_is_converted_to_the_internal_attachment_dto() {
        let metadata = br#"{"method":"agent.tasks.start","args":{"input":{"requestId":"req-1","prompt":"describe"}}}"#;
        let bytes = b"\x00\xfffile\r\n--agent-boundaryXstill-file";
        let body = multipart_request(metadata, "meal.bin", bytes);
        let parts = parse_multipart_parts(&body, BOUNDARY).unwrap();
        let request = parse_agent_multipart(parts).unwrap();
        let attachment = &request.args["input"]["attachments"][0];
        assert_eq!(attachment["name"], "meal.bin");
        assert_eq!(attachment["mediaType"], "application/octet-stream");
        assert_eq!(
            attachment["base64"],
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        assert_eq!(request.args["input"]["requestId"], "req-1");
    }

    #[test]
    fn multipart_accepts_quoted_leaf_names_and_rejects_ambiguous_parameters() {
        let body = b"--agent-boundary\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n{\"method\":\"agent.tasks.start\",\"args\":{\"input\":{\"requestId\":\"req-1\",\"prompt\":\"x\"}}}\r\n--agent-boundary\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"notes; review.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--agent-boundary--\r\n";
        let parts = parse_multipart_parts(body, BOUNDARY).unwrap();
        assert_eq!(parts[1].filename.as_deref(), Some("notes; review.txt"));

        let duplicate_parameter = body
            .windows(b"filename=\"notes; review.txt\"".len())
            .position(|window| window == b"filename=\"notes; review.txt\"")
            .unwrap();
        let mut ambiguous = body.to_vec();
        ambiguous.splice(
            duplicate_parameter..duplicate_parameter + b"filename=\"notes; review.txt\"".len(),
            b"filename=\"first.txt\"; filename=\"second.txt\""
                .iter()
                .copied(),
        );
        assert!(parse_multipart_parts(&ambiguous, BOUNDARY).is_err());

        let duplicate_header = String::from_utf8_lossy(body).replace(
            "Content-Disposition: form-data; name=\"attachment\"; filename=\"notes; review.txt\"",
            "Content-Disposition: form-data; name=\"attachment\"; filename=\"notes.txt\"\r\nContent-Disposition: form-data; name=\"other\"",
        );
        assert!(parse_multipart_parts(duplicate_header.as_bytes(), BOUNDARY).is_err());
    }

    #[test]
    fn multipart_rejects_paths_inline_legacy_images_and_wrong_file_operations() {
        let metadata = br#"{"method":"agent.tasks.start","args":{"input":{"requestId":"req-1","prompt":"x"}}}"#;
        let path = multipart_request(metadata, "../private.txt", b"x");
        assert!(parse_multipart_parts(&path, BOUNDARY)
            .and_then(parse_agent_multipart)
            .is_err());

        let legacy = br#"{"method":"agent.tasks.start","args":{"input":{"requestId":"req-1","prompt":"x","images":[]}}}"#;
        let legacy_body = multipart_request(legacy, "file.txt", b"x");
        let parts = parse_multipart_parts(&legacy_body, BOUNDARY).unwrap();
        assert!(parse_agent_multipart(parts).is_err());

        let non_task = br#"{"method":"agent.tasks.get","args":{"runId":"r"}}"#;
        let non_task_body = multipart_request(non_task, "file.txt", b"x");
        let parts = parse_multipart_parts(&non_task_body, BOUNDARY).unwrap();
        assert!(parse_agent_multipart(parts).is_err());
    }

    #[test]
    fn multipart_enforces_file_count_and_size_limits() {
        let mut total = 0;
        assert!(validate_file_size(MAX_AGENT_FILE_SIZE + 1, &mut total).is_err());
        assert!(validate_file_size(MAX_AGENT_FILE_SIZE, &mut total).is_ok());
        assert!(validate_file_size(MAX_AGENT_FILE_SIZE, &mut total).is_ok());
        assert!(validate_file_size(1, &mut total).is_err());

        let metadata = br#"{"method":"agent.tasks.start","args":{"input":{"requestId":"req-1","prompt":"x"}}}"#;
        let mut body = Vec::new();
        body.extend_from_slice(
            b"--agent-boundary\r\nContent-Disposition: form-data; name=\"request\"\r\n\r\n",
        );
        body.extend_from_slice(metadata);
        for _ in 0..=MAX_AGENT_FILE_COUNT {
            body.extend_from_slice(b"\r\n--agent-boundary\r\nContent-Disposition: form-data; name=\"attachment\"; filename=\"x.txt\"\r\n\r\nx");
        }
        body.extend_from_slice(b"\r\n--agent-boundary--\r\n");
        assert!(parse_multipart_parts(&body, BOUNDARY)
            .and_then(parse_agent_multipart)
            .is_err());
    }
}
