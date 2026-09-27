//! `tokio_util::codec` impls for JSON-RPC framing.
//!
//! `LspCodec` — LSP-style `Content-Length: N\r\n\r\n<json>` framing.
//! `LineCodec` — NDJSON: one JSON object per `\n`-terminated line.

use bytes::{Buf, BufMut, BytesMut};
use serde_json::Value;
use thiserror::Error;
use tokio_util::codec::{Decoder, Encoder};

use crate::messages::Message;

/// Errors produced by codecs.
#[derive(Debug, Error)]
pub enum CodecError {
    /// Frame exceeded the configured max size.
    #[error("frame too large: {0} bytes (max {1})")]
    FrameTooLarge(usize, usize),
    /// Malformed Content-Length header (missing, non-numeric, etc.).
    #[error("malformed framing header: {0}")]
    MalformedHeader(String),
    /// JSON parse failed.
    #[error("json parse error: {0}")]
    Json(#[from] serde_json::Error),
    /// Underlying I/O error from the framed transport.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Default maximum header size — matches Claude Code's 64 KiB LSP framing cap.
pub const DEFAULT_MAX_HEADER_SIZE: usize = 64 * 1024;

/// Default maximum frame size — matches Claude Code's 32 MiB LSP body cap.
pub const DEFAULT_MAX_FRAME_SIZE: usize = 32 * 1024 * 1024;

/// `Content-Length: N\r\n\r\n<json>` codec — LSP and modern MCP framing.
///
/// On encode emits exactly:
/// ```text
/// Content-Length: <N>\r\n\r\n<json-body>
/// ```
/// No `Content-Type` header. No other headers.
///
/// On decode accepts `Content-Length` and optional `Content-Type`
/// case-insensitively, with RFC token header names. Any other header is treated
/// as stdout desynchronization and rejected.
#[derive(Debug)]
pub struct LspCodec {
    max_header_size: usize,
    max_frame_size: usize,
    /// Parser state — if we've parsed the header but not yet seen the full
    /// body, remember the body length so the next decode call can pick up.
    pending_body_len: Option<usize>,
}

impl Default for LspCodec {
    fn default() -> Self {
        Self {
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            pending_body_len: None,
        }
    }
}

impl LspCodec {
    /// Construct a codec with a custom max frame size in bytes.
    #[must_use]
    pub fn with_max_frame_size(max_frame_size: usize) -> Self {
        Self {
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            max_frame_size,
            pending_body_len: None,
        }
    }
}

impl Encoder<Message> for LspCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let body = serde_json::to_vec(&item)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        dst.reserve(header.len() + body.len());
        dst.put_slice(header.as_bytes());
        dst.put_slice(&body);
        Ok(())
    }
}

impl Decoder for LspCodec {
    type Item = Message;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            if let Some(body_len) = self.pending_body_len {
                // Header already parsed; wait for full body.
                if src.len() < body_len {
                    return Ok(None);
                }
                let body = src.split_to(body_len);
                self.pending_body_len = None;
                let v: Value = match serde_json::from_slice(&body) {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error, "LSP: dropped unparseable message body");
                        continue;
                    }
                };
                if !v.is_object() {
                    tracing::warn!("LSP: dropped message body that is not an object");
                    continue;
                }
                match serde_json::from_value(v) {
                    Ok(message) => return Ok(Some(message)),
                    Err(error) => {
                        tracing::warn!(%error, "LSP: dropped unrecognized JSON-RPC message body");
                        continue;
                    }
                }
            }

            // Locate `\r\n\r\n` separator.
            let Some(sep_pos) = find_crlf_crlf(src) else {
                if src.len() > self.max_header_size {
                    return Err(CodecError::FrameTooLarge(src.len(), self.max_header_size));
                }
                return Ok(None);
            };
            if sep_pos + 4 > self.max_header_size {
                return Err(CodecError::FrameTooLarge(sep_pos + 4, self.max_header_size));
            }
            let header_bytes = &src[..sep_pos];
            let header_str: String = header_bytes.iter().map(|byte| char::from(*byte)).collect();

            let mut body_len: Option<usize> = None;
            for line in header_str.split("\r\n") {
                if line.is_empty() {
                    continue;
                }
                let Some((name, value)) = line.split_once(':') else {
                    return Err(CodecError::MalformedHeader(format!(
                        "missing ':' in header line: {line:?}"
                    )));
                };
                let name = name.trim_end_matches([' ', '\t']);
                if !is_header_name_token(name) {
                    return Err(CodecError::MalformedHeader(format!(
                        "invalid header name: {name:?}"
                    )));
                }

                if name.eq_ignore_ascii_case("Content-Length") {
                    let value = value.trim();
                    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(CodecError::MalformedHeader(
                            "Content-Length is not a number".into(),
                        ));
                    }
                    let n: usize = value
                        .parse()
                        .map_err(|e| CodecError::MalformedHeader(format!("Content-Length: {e}")))?;
                    // vscode-jsonrpc/Claude Code accepts duplicate protocol
                    // headers and uses the final Content-Length value.
                    body_len = Some(n);
                    continue;
                }

                if name.eq_ignore_ascii_case("Content-Type") {
                    continue;
                }

                return Err(CodecError::MalformedHeader(format!(
                    "unrecognized header line: {line:?}"
                )));
            }

            let body_len = body_len.ok_or_else(|| {
                CodecError::MalformedHeader("missing Content-Length header".into())
            })?;

            if body_len > self.max_frame_size {
                src.advance(sep_pos + 4);
                return Err(CodecError::FrameTooLarge(body_len, self.max_frame_size));
            }

            // Consume the header plus the CRLFCRLF separator.
            src.advance(sep_pos + 4);
            self.pending_body_len = Some(body_len);
            // Fall through to the body branch at the top of the loop.
        }
    }
}

fn find_crlf_crlf(buf: &BytesMut) -> Option<usize> {
    let needle = b"\r\n\r\n";
    if buf.len() < needle.len() {
        return None;
    }
    buf.windows(needle.len()).position(|w| w == needle)
}

fn is_header_name_token(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// NDJSON / `\n`-delimited JSON codec — older MCP servers and IDE bridge JSON-text frames.
///
/// Emit: one JSON object followed by a single `\n` (LF, `0x0A`).
/// Parse: split on `\n`; tolerate an optional `\r` immediately preceding each
/// `\n`; skip empty lines.
#[derive(Debug)]
pub struct LineCodec {
    max_frame_size: usize,
}

impl Default for LineCodec {
    fn default() -> Self {
        Self {
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
        }
    }
}

impl LineCodec {
    /// Construct a codec with a custom max frame size in bytes.
    #[must_use]
    pub fn with_max_frame_size(max_frame_size: usize) -> Self {
        Self { max_frame_size }
    }
}

impl Encoder<Message> for LineCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let body = serde_json::to_vec(&item)?;
        dst.reserve(body.len() + 1);
        dst.put_slice(&body);
        dst.put_u8(b'\n');
        Ok(())
    }
}

impl Decoder for LineCodec {
    type Item = Message;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            // Find the next LF.
            let Some(lf_pos) = src.iter().position(|&b| b == b'\n') else {
                if src.len() > self.max_frame_size {
                    return Err(CodecError::FrameTooLarge(src.len(), self.max_frame_size));
                }
                return Ok(None);
            };

            // Take the line (without the LF).
            let mut line = src.split_to(lf_pos + 1);
            // Drop the LF terminator.
            line.truncate(line.len() - 1);
            // Drop an optional trailing CR.
            if line.last() == Some(&b'\r') {
                line.truncate(line.len() - 1);
            }

            if line.is_empty() {
                // Skip blank lines.
                continue;
            }

            if line.len() > self.max_frame_size {
                return Err(CodecError::FrameTooLarge(line.len(), self.max_frame_size));
            }

            let v: Value = serde_json::from_slice(&line)?;
            let msg: Message = serde_json::from_value(v)?;
            return Ok(Some(msg));
        }
    }
}

#[cfg(test)]
mod lsp_codec_tests {
    use super::*;
    use crate::messages::{Id, Notification, Request, JSONRPC_VERSION};
    use serde_json::json;

    fn encode_and_decode(msg: Message) -> Message {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(msg, &mut buf).expect("encode");
        codec
            .decode(&mut buf)
            .expect("no decode error")
            .expect("frame")
    }

    #[test]
    fn encode_request_uses_content_length_header() {
        let req = Request::new("ping", Some(json!({})), Id::Number(1));
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Message::Request(req), &mut buf).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        // Header literal must be `Content-Length: <n>\r\n\r\n`.
        assert!(s.starts_with("Content-Length: "), "got: {s:?}");
        assert!(s.contains("\r\n\r\n"), "missing CRLFCRLF separator: {s:?}");
        // Length field must match the JSON body length.
        let header_end = s.find("\r\n\r\n").unwrap() + 4;
        let body = &s[header_end..];
        let claimed = s[("Content-Length: ".len())..s.find("\r\n").unwrap()]
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            claimed,
            body.len(),
            "Content-Length must match body byte length"
        );
    }

    #[test]
    fn roundtrip_request() {
        let req = Request::new("tools/list", None, Id::Number(7));
        let back = encode_and_decode(Message::Request(req.clone()));
        match back {
            Message::Request(r) => {
                assert_eq!(r.method, "tools/list");
                assert_eq!(r.id, Id::Number(7));
                assert_eq!(r.jsonrpc, JSONRPC_VERSION);
            }
            other => panic!("expected Request, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_notification() {
        let n = Notification::new("notifications/cancelled", Some(json!({"requestId": 1})));
        let back = encode_and_decode(Message::Notification(n.clone()));
        assert!(matches!(back, Message::Notification(_)));
    }

    #[test]
    fn decode_returns_none_on_incomplete_header() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(&b"Content-Len"[..]);
        let result = codec.decode(&mut buf).unwrap();
        assert!(
            result.is_none(),
            "partial header must yield None, not error"
        );
    }

    #[test]
    fn decode_returns_none_on_incomplete_body() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(&b"Content-Length: 100\r\n\r\n{\"jsonr"[..]);
        let result = codec.decode(&mut buf).unwrap();
        assert!(result.is_none(), "partial body must yield None, not error");
    }

    #[test]
    fn decode_two_frames_back_to_back() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(
                Message::Request(Request::new("a", None, Id::Number(1))),
                &mut buf,
            )
            .unwrap();
        codec
            .encode(
                Message::Request(Request::new("b", None, Id::Number(2))),
                &mut buf,
            )
            .unwrap();

        let first = codec.decode(&mut buf).unwrap().expect("frame 1");
        let second = codec.decode(&mut buf).unwrap().expect("frame 2");
        match (first, second) {
            (Message::Request(a), Message::Request(b)) => {
                assert_eq!(a.method, "a");
                assert_eq!(b.method, "b");
            }
            _ => panic!("expected two requests"),
        }
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn decode_rejects_non_numeric_content_length() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(&b"Content-Length: abc\r\n\r\n{}"[..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::MalformedHeader(_)));
    }

    #[test]
    fn decode_rejects_frame_over_max_size() {
        let mut codec = LspCodec::with_max_frame_size(16);
        let mut buf = BytesMut::from(&b"Content-Length: 1024\r\n\r\n"[..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge(1024, 16)));
    }

    #[test]
    fn decode_accepts_content_headers_case_insensitively() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(
            &b"content-length: 37\r\nCoNtEnT-TyPe: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"m\",\"id\":1}"[..],
        );
        let msg = codec.decode(&mut buf).unwrap().expect("frame");
        assert!(matches!(msg, Message::Request(_)));
    }

    #[test]
    fn decode_rejects_unknown_headers() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(&b"Content-Length: 2\r\nX-Foo: bar\r\n\r\n{}"[..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::MalformedHeader(_)));
    }

    #[test]
    fn decode_rejects_invalid_header_name_token() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(&b"Content Length: 2\r\n\r\n{}"[..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::MalformedHeader(_)));
    }

    #[test]
    fn decode_requires_crlfcrlf_separator() {
        let mut codec = LspCodec::default();
        let mut buf = BytesMut::from(&b"Content-Length: 2\n\n{}"[..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn decode_rejects_header_over_default_cap() {
        let mut codec = LspCodec::default();
        let oversized = vec![b'a'; DEFAULT_MAX_HEADER_SIZE + 1];
        let mut buf = BytesMut::from(&oversized[..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            CodecError::FrameTooLarge(actual, DEFAULT_MAX_HEADER_SIZE)
                if actual == DEFAULT_MAX_HEADER_SIZE + 1
        ));
    }

    #[test]
    fn decode_rejects_body_over_default_cap() {
        let mut codec = LspCodec::default();
        let oversized = DEFAULT_MAX_FRAME_SIZE + 1;
        let frame = format!("Content-Length: {oversized}\r\n\r\n");
        let mut buf = BytesMut::from(frame.as_bytes());
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            CodecError::FrameTooLarge(n, DEFAULT_MAX_FRAME_SIZE) if n == oversized
        ));
    }
}

#[cfg(test)]
mod line_codec_tests {
    use super::*;
    use crate::messages::{Id, Request};
    use serde_json::json;

    #[test]
    fn encode_appends_lf_only() {
        let req = Request::new("ping", None, Id::Number(1));
        let mut codec = LineCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(Message::Request(req), &mut buf).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'));
        // Must NOT emit CRLF — pure LF per NDJSON convention.
        assert!(!buf.windows(2).any(|w| w == b"\r\n"));
    }

    #[test]
    fn decode_one_line() {
        let mut codec = LineCodec::default();
        let mut buf = BytesMut::from(&br#"{"jsonrpc":"2.0","method":"m","id":1}"#[..]);
        buf.extend_from_slice(b"\n");
        let m = codec.decode(&mut buf).unwrap().expect("frame");
        assert!(matches!(m, Message::Request(_)));
    }

    #[test]
    fn decode_tolerates_trailing_cr_before_lf() {
        // Some servers send CRLF; our parser tolerates an optional \r before \n.
        let mut codec = LineCodec::default();
        let mut buf = BytesMut::from(&br#"{"jsonrpc":"2.0","method":"m","id":1}"#[..]);
        buf.extend_from_slice(b"\r\n");
        let m = codec.decode(&mut buf).unwrap().expect("frame");
        assert!(matches!(m, Message::Request(_)));
    }

    #[test]
    fn decode_returns_none_on_no_lf() {
        let mut codec = LineCodec::default();
        let mut buf = BytesMut::from(&br#"{"jsonrpc":"2.0","method":"m","#[..]);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn decode_skips_empty_lines() {
        let mut codec = LineCodec::default();
        let mut buf = BytesMut::from(&b"\n\n"[..]);
        buf.extend_from_slice(br#"{"jsonrpc":"2.0","method":"m","id":1}"#);
        buf.extend_from_slice(b"\n");
        let m = codec.decode(&mut buf).unwrap().expect("frame");
        assert!(matches!(m, Message::Request(_)));
    }

    #[test]
    fn roundtrip_two_messages() {
        let mut codec = LineCodec::default();
        let mut buf = BytesMut::new();
        codec
            .encode(
                Message::Request(Request::new("a", None, Id::Number(1))),
                &mut buf,
            )
            .unwrap();
        codec
            .encode(
                Message::Request(Request::new("b", Some(json!({})), Id::Number(2))),
                &mut buf,
            )
            .unwrap();
        let a = codec.decode(&mut buf).unwrap().expect("frame 1");
        let b = codec.decode(&mut buf).unwrap().expect("frame 2");
        match (a, b) {
            (Message::Request(a), Message::Request(b)) => {
                assert_eq!(a.method, "a");
                assert_eq!(b.method, "b");
            }
            _ => panic!("expected two requests"),
        }
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn decode_rejects_oversize_line_after_buffer_grows() {
        let mut codec = LineCodec::with_max_frame_size(16);
        // 32 bytes with no LF — exceeds the cap.
        let mut buf = BytesMut::from(&[b'x'; 32][..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge(32, 16)));
    }
}
