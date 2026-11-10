//! Streaming text projections. Skipped lines never need an unbounded line buffer.

use crate::{map_io, open_read_file_pinned, FsError, RootIdentity};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;

const SCAN_BYTES: usize = 64 * 1024;

/// A line window, or a bounded UTF-8 tail, with the whole file's line count.
#[derive(Debug)]
pub struct TextWindow {
    pub content: String,
    pub total_lines: u64,
    /// Bytes removed from the front of an unwindowed read, including any
    /// continuation bytes at the requested tail boundary.
    pub omitted_bytes: u64,
    /// Backend truncation independently of the requested tail retention.
    pub truncated: bool,
}

/// Read through one no-follow descriptor under the pinned root. A full read
/// retains only `tail_bytes`; an explicit line window retains only those lines.
/// Both scan the entire stream to validate UTF-8 and count its lines exactly.
pub fn read_text_window_pinned(
    root: &Path,
    relative: &Path,
    expected: Option<&RootIdentity>,
    offset: Option<u64>,
    limit: Option<u64>,
    tail_bytes: u64,
) -> Result<TextWindow, FsError> {
    let file = open_read_file_pinned(root, relative, expected)?;
    // A continuously appending producer must not extend this scan forever.
    // Tail retention and line totals describe the same opened-file snapshot.
    let length = file
        .metadata()
        .map_err(|error| map_io(relative, error))?
        .len();
    read_text_window(file.take(length), relative, offset, limit, tail_bytes)
}

/// Stream a text projection from an already-opened descriptor. Invalid UTF-8,
/// including in skipped content, has the same I/O error as `read_to_string`.
pub fn read_text_window(
    reader: impl Read,
    path: &Path,
    offset: Option<u64>,
    limit: Option<u64>,
    tail_bytes: u64,
) -> Result<TextWindow, FsError> {
    scan(reader, path, offset, limit, tail_bytes, true)
}

/// Preserve a linked transcript's historical lossy full-read decoding, while
/// counting the whole stream and retaining only its bounded byte tail.
pub fn read_text_tail_lossy(
    reader: impl Read,
    path: &Path,
    tail_bytes: u64,
) -> Result<TextWindow, FsError> {
    scan(reader, path, None, None, tail_bytes, false)
}

fn invalid_utf8(path: &Path) -> FsError {
    map_io(
        path,
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ),
    )
}

fn scan(
    mut reader: impl Read,
    path: &Path,
    offset: Option<u64>,
    limit: Option<u64>,
    tail_bytes: u64,
    strict: bool,
) -> Result<TextWindow, FsError> {
    let windowed = offset.is_some() || limit.is_some();
    let tail_limit = usize::try_from(tail_bytes).unwrap_or(usize::MAX);
    let mut tail = VecDeque::new();
    let mut output = Vec::new();
    // At most three incomplete UTF-8 bytes survive between scans.
    let mut pending = Vec::with_capacity(SCAN_BYTES + 3);
    let mut buffer = [0; SCAN_BYTES];
    let mut total_bytes = 0u64;
    let mut newlines = 0u64;
    let mut last_byte = None;
    let skip = offset.unwrap_or(0);
    let take = limit.unwrap_or(u64::MAX);
    let mut selected_lines = 0u64;
    let mut line_started = false;
    let mut line_start = 0;
    loop {
        let n = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result.map_err(|error| map_io(path, error))?,
        };
        if n == 0 {
            break;
        }
        let bytes = &buffer[..n];
        if strict {
            pending.extend_from_slice(bytes);
            match std::str::from_utf8(&pending) {
                Ok(_) => pending.clear(),
                Err(error) if error.error_len().is_none() => {
                    let valid = error.valid_up_to();
                    pending.copy_within(valid.., 0);
                    pending.truncate(pending.len() - valid);
                }
                Err(_) => return Err(invalid_utf8(path)),
            }
        }
        total_bytes = total_bytes.saturating_add(n as u64);
        last_byte = bytes.last().copied();
        if !windowed {
            let keep = n.min(tail_limit);
            let retained = tail.len().min(tail_limit - keep);
            tail.drain(..tail.len() - retained);
            tail.extend(&bytes[n - keep..]);
            newlines += bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
            continue;
        }
        let mut start = 0;
        while start < bytes.len() {
            let end = bytes[start..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |at| start + at);
            let newline = end < bytes.len();
            if newlines >= skip && newlines - skip < take {
                if !line_started {
                    if selected_lines > 0 {
                        output.push(b'\n');
                    }
                    line_start = output.len();
                    selected_lines += 1;
                    line_started = true;
                }
                output.extend_from_slice(&bytes[start..end]);
                // `str::lines()` and `BufRead::lines()` remove CR only when
                // followed by LF, including CR/LF split across scan buffers.
                if newline && output.len() > line_start && output.last() == Some(&b'\r') {
                    output.pop();
                }
            }
            if newline {
                newlines += 1;
                line_started = false;
            }
            start = end + usize::from(newline);
        }
    }
    if strict && !pending.is_empty() {
        return Err(invalid_utf8(path));
    }
    let total_lines = newlines + u64::from(last_byte.is_some_and(|byte| byte != b'\n'));
    let (content, omitted_bytes) = if windowed {
        (
            String::from_utf8(output).map_err(|_| invalid_utf8(path))?,
            0,
        )
    } else {
        let mut omitted = total_bytes.saturating_sub(tail.len() as u64);
        if omitted > 0 {
            while tail.front().is_some_and(|byte| *byte & 0xc0 == 0x80) {
                tail.pop_front();
                omitted += 1;
            }
        }
        let bytes = tail.make_contiguous();
        let content = if strict {
            std::str::from_utf8(bytes)
                .map_err(|_| invalid_utf8(path))?
                .to_owned()
        } else {
            String::from_utf8_lossy(bytes).into_owned()
        };
        (content, omitted)
    };
    Ok(TextWindow {
        content,
        total_lines,
        omitted_bytes,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SmallChunks<'a> {
        bytes: &'a [u8],
        chunk: usize,
    }
    impl Read for SmallChunks<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let n = self.bytes.len().min(output.len()).min(self.chunk);
            output[..n].copy_from_slice(&self.bytes[..n]);
            self.bytes = &self.bytes[n..];
            Ok(n)
        }
    }

    #[test]
    fn windows_match_lines_across_unicode_and_crlf_boundaries() {
        for body in ["", "\n", "\r\n", "a\n\n", "a\rb", "🙂\r\n中\n\r", "a\n中"] {
            for chunk in 1..=5 {
                for skip in 0..=4 {
                    for limit in [None, Some(0), Some(1), Some(3)] {
                        let result = read_text_window(
                            SmallChunks {
                                bytes: body.as_bytes(),
                                chunk,
                            },
                            Path::new("spool"),
                            Some(skip),
                            limit,
                            1,
                        )
                        .unwrap();
                        let expected = body
                            .lines()
                            .skip(skip as usize)
                            .take(limit.unwrap_or(u64::MAX) as usize)
                            .collect::<Vec<_>>()
                            .join("\n");
                        assert_eq!(
                            result.content, expected,
                            "{body:?}, chunk={chunk}, skip={skip}, limit={limit:?}"
                        );
                        assert_eq!(result.total_lines, body.lines().count() as u64);
                        assert_eq!(result.omitted_bytes, 0);
                    }
                }
            }
        }
    }

    #[test]
    fn tail_aligns_utf8_and_counts_all_lines() {
        let body = "head\n🙂中\ntail\n";
        let result = read_text_window(body.as_bytes(), Path::new("spool"), None, None, 10).unwrap();
        assert_eq!(result.content, "中\ntail\n");
        assert_eq!(result.total_lines, 3);
        assert_eq!(
            result.omitted_bytes,
            (body.len() - result.content.len()) as u64
        );
    }

    #[test]
    fn validates_invalid_utf8_even_outside_retained_projection() {
        for bytes in [&b"\xff\nvalid\n"[..], &b"valid\n\xf0\x9f"[..]] {
            for options in [(None, None), (Some(1), Some(0))] {
                let error = read_text_window(
                    SmallChunks { bytes, chunk: 1 },
                    Path::new("spool"),
                    options.0,
                    options.1,
                    2,
                )
                .unwrap_err();
                assert!(matches!(error, FsError::Io(_)));
            }
        }
    }

    #[test]
    fn skipped_giant_line_and_full_read_use_bounded_scan_buffers() {
        struct GiantLine {
            remaining: usize,
        }
        impl Read for GiantLine {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                assert!(bytes.len() <= SCAN_BYTES);
                let n = bytes.len().min(self.remaining);
                bytes[..n].fill(b'x');
                self.remaining -= n;
                Ok(n)
            }
        }
        let size = 16 * 1024 * 1024;
        let window = read_text_window(
            GiantLine { remaining: size },
            Path::new("spool"),
            Some(1),
            Some(1),
            31,
        )
        .unwrap();
        assert!(window.content.is_empty());
        assert_eq!(window.total_lines, 1);
        let tail = read_text_window(
            GiantLine { remaining: size },
            Path::new("spool"),
            None,
            None,
            31,
        )
        .unwrap();
        assert_eq!(tail.content, "x".repeat(31));
        assert_eq!(tail.omitted_bytes, size as u64 - 31);
    }
}
