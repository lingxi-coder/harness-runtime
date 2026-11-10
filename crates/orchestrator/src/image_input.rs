//! Load a pasted image file into a canonical [`lingxi_core::types::ImageSource::Base64`].
//!
//! The TUI records pasted image *paths*; the orchestrator reads + base64-encodes
//! them here, just before building the outgoing user message, so a read failure
//! surfaces as a turn error (and the TUI never blocks on disk I/O during paste).

use crate::error::OrchestratorError;
use lingxi_core::types::ImageSource;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

/// Decode one ordered batch without blocking a runtime worker. A running file
/// read cannot be interrupted, but cancellation can discard it before admission
/// and stops the worker before it starts the next image.
pub(crate) async fn load_image_sources(
    image_paths: &[PathBuf],
    cancel: &CancellationToken,
) -> Result<Option<Vec<ImageSource>>, OrchestratorError> {
    load_image_sources_with(image_paths, cancel, load_image_source).await
}

async fn load_image_sources_with<L>(
    image_paths: &[PathBuf],
    cancel: &CancellationToken,
    mut load: L,
) -> Result<Option<Vec<ImageSource>>, OrchestratorError>
where
    L: FnMut(&Path) -> Result<ImageSource, OrchestratorError> + Send + 'static,
{
    if cancel.is_cancelled() {
        return Ok(None);
    }
    if image_paths.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let image_paths = image_paths.to_vec();
    let load_cancel = cancel.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let mut images = Vec::with_capacity(image_paths.len());
        for path in image_paths {
            if load_cancel.is_cancelled() {
                return Ok(None);
            }
            images.push(load(&path)?);
        }
        Ok(Some(images))
    });
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            // This prevents a queued job from starting; an active decoder is
            // read-only and will observe the token before the next image.
            task.abort();
            Ok(None)
        }
        result = &mut task => {
            if cancel.is_cancelled() {
                return Ok(None);
            }
            result.map_err(|error| {
                OrchestratorError::Internal(format!("could not load image attachments: {error}"))
            })?
        }
    }
}

/// Detect the image MIME type from a file extension. `None` for unsupported
/// extensions.
///
/// This is only a *fallback hint*: the real media type comes from sniffing the
/// file's magic bytes ([`detect_media_type_from_bytes`]), mirroring TS where the
/// sent media type is `detectImageFormatFromBase64` (imagePaste.ts:409) and the
/// extension is only used as a `metadata.format ?? ext` fallback
/// (imageResizer.ts:186) with `jpg` normalized to `jpeg` (imageResizer.ts:188).
fn media_type_for(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        // `jpg` → `jpeg` normalization (imageResizer.ts:188).
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

/// Detect the image MIME type from the file's leading *magic bytes*, the way the
/// model sees it — so a file whose extension lies about its real format (e.g. a
/// `.png` that actually holds JPEG bytes) is sent with the correct media type.
///
/// Mirrors TS `detectImageFormatFromBuffer` (imageResizer.ts:769-812): PNG
/// `89 50 4e 47`, JPEG `ff d8 ff`, GIF `47 49 46`, WEBP `RIFF`(`52 49 46 46`)…
/// `WEBP`(`57 45 42 50`) at offset 8. Returns `None` for anything unrecognized
/// (or fewer than 4 bytes); the caller then falls back to the extension hint and
/// finally to `image/png`, matching the TS default.
fn detect_media_type_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 4 {
        return None;
    }
    // PNG signature: 89 50 4e 47
    if bytes[0] == 0x89 && bytes[1] == 0x50 && bytes[2] == 0x4e && bytes[3] == 0x47 {
        return Some("image/png");
    }
    // JPEG signature: ff d8 ff
    if bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return Some("image/jpeg");
    }
    // GIF signature: 47 49 46 ("GIF", covers GIF87a / GIF89a)
    if bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 {
        return Some("image/gif");
    }
    // WEBP signature: "RIFF" (52 49 46 46) then "WEBP" (57 45 42 50) at offset 8.
    if bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes.len() >= 12
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
    {
        return Some("image/webp");
    }
    None
}

/// Standard base64 (RFC 4648) encoder with padding. No `data:` prefix.
fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(b2 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Read an image file from disk and build a base64 [`ImageSource`].
///
/// The media type is determined from the file's actual *magic bytes*
/// ([`detect_media_type_from_bytes`], the authoritative source — TS
/// imagePaste.ts:409), falling back to the extension hint
/// ([`media_type_for`] — TS imageResizer.ts:186 `metadata.format ?? ext`) and
/// finally to `image/png` (the TS default). This means a `.png` file that
/// actually holds JPEG bytes is sent with `image/jpeg`, and an unrecognized file
/// is never rejected for "unsupported type" — it defaults to PNG like TS.
///
/// # Errors
/// Returns [`OrchestratorError::Internal`] only if the file cannot be read.
pub fn load_image_source(path: &Path) -> Result<ImageSource, OrchestratorError> {
    const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > MAX_IMAGE_BYTES {
            return Err(OrchestratorError::Internal(format!(
                "image {} exceeds {MAX_IMAGE_BYTES} bytes",
                path.display()
            )));
        }
    }
    let bytes = std::fs::read(path).map_err(|e| {
        OrchestratorError::Internal(format!("could not read image {}: {e}", path.display()))
    })?;
    // Magic bytes win; extension is only a fallback hint; default PNG.
    let media_type = detect_media_type_from_bytes(&bytes)
        .or_else(|| media_type_for(path))
        .unwrap_or("image/png");
    Ok(ImageSource::Base64 {
        media_type: media_type.to_string(),
        data: base64_encode(&bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    struct ReleaseDecoderOnDrop(Option<mpsc::SyncSender<()>>);

    impl ReleaseDecoderOnDrop {
        fn release(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.try_send(());
            }
        }
    }

    impl Drop for ReleaseDecoderOnDrop {
        fn drop(&mut self) {
            self.release();
        }
    }

    struct DecoderFinished(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DecoderFinished {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn image_batch_yields_and_cancels_before_a_blocked_decoder_finishes() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut release = ReleaseDecoderOnDrop(Some(release_tx));
        let calls = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let pending = tokio::spawn({
            let cancel = cancel.clone();
            let calls = calls.clone();
            async move {
                let mut started_tx = Some(started_tx);
                let finished = DecoderFinished(Some(finished_tx));
                load_image_sources_with(
                    &[PathBuf::from("first.png"), PathBuf::from("second.png")],
                    &cancel,
                    move |_| {
                        let _keep_finished_alive = &finished;
                        calls.fetch_add(1, Ordering::SeqCst);
                        if let Some(sender) = started_tx.take() {
                            let _ = sender.send(());
                            release_rx
                                .recv_timeout(Duration::from_secs(10))
                                .expect("test must release the blocking decoder");
                        }
                        Ok(ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "iVBORw==".into(),
                        })
                    },
                )
                .await
            }
        });
        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("decoder should start")
            .unwrap();
        let heartbeat = tokio::spawn(async { 42 });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), heartbeat)
                .await
                .expect("the runtime must remain responsive during decoding")
                .unwrap(),
            42
        );
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .expect("cancellation must not wait for the blocked decoder")
            .unwrap()
            .unwrap()
            .is_none());
        release.release();
        tokio::time::timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("the read-only worker should finish after release")
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn image_batch_cancellation_wins_over_decoder_completion_or_error() {
        for fail in [false, true] {
            let cancel = CancellationToken::new();
            let decoder_cancel = cancel.clone();
            let result =
                load_image_sources_with(&[PathBuf::from("completed.png")], &cancel, move |_| {
                    decoder_cancel.cancel();
                    if fail {
                        return Err(OrchestratorError::Internal("cancelled decode error".into()));
                    }
                    Ok(ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "iVBORw==".into(),
                    })
                })
                .await
                .unwrap();
            assert!(result.is_none(), "cancellation wins even when fail={fail}");
        }
    }

    #[tokio::test]
    async fn image_batch_preserves_source_order_bytes_and_first_error() {
        let dir = tempfile::tempdir().unwrap();
        let jpeg = dir.path().join("jpeg-with-png-extension.png");
        let png = dir.path().join("png-with-jpg-extension.jpg");
        std::fs::write(&jpeg, b"\xff\xd8\xff\xe0").unwrap();
        std::fs::write(&png, b"\x89PNG").unwrap();
        let images = load_image_sources(&[jpeg, png], &CancellationToken::new())
            .await
            .unwrap()
            .unwrap();
        let decoded: Vec<_> = images
            .into_iter()
            .map(|source| match source {
                ImageSource::Base64 { media_type, data } => (media_type, data),
                ImageSource::Url { .. } => panic!("expected base64"),
            })
            .collect();
        assert_eq!(
            decoded,
            vec![
                ("image/jpeg".to_string(), "/9j/4A==".to_string()),
                ("image/png".to_string(), "iVBORw==".to_string()),
            ]
        );

        let paths = ["first", "failing", "unvisited"].map(PathBuf::from);
        let visited = Arc::new(Mutex::new(Vec::new()));
        let worker_visited = visited.clone();
        let error = load_image_sources_with(&paths, &CancellationToken::new(), move |path| {
            worker_visited.lock().unwrap().push(path.to_path_buf());
            if path == Path::new("failing") {
                return Err(OrchestratorError::Internal("first image failure".into()));
            }
            Ok(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: String::new(),
            })
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("first image failure"));
        assert_eq!(*visited.lock().unwrap(), paths[..2]);
    }

    #[test]
    fn base64_encodes_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello world!"), "aGVsbG8gd29ybGQh");
    }

    #[test]
    fn media_type_detection() {
        // Extension hint map (jpg normalized to jpeg).
        assert_eq!(media_type_for(Path::new("a.PNG")), Some("image/png"));
        assert_eq!(media_type_for(Path::new("a.jpg")), Some("image/jpeg"));
        assert_eq!(media_type_for(Path::new("a.jpeg")), Some("image/jpeg"));
        assert_eq!(media_type_for(Path::new("a.webp")), Some("image/webp"));
        assert_eq!(media_type_for(Path::new("a.txt")), None);
        assert_eq!(media_type_for(Path::new("noext")), None);
    }

    // Magic-byte prefixes for each recognized format (TS imageResizer.ts:769-812).
    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG_MAGIC: &[u8] = b"\xff\xd8\xff\xe0";
    const GIF_MAGIC: &[u8] = b"GIF89a";
    // "RIFF" + 4-byte size + "WEBP".
    const WEBP_MAGIC: &[u8] = b"RIFF\x00\x00\x00\x00WEBP";

    #[test]
    fn sniffs_each_format_from_bytes() {
        assert_eq!(detect_media_type_from_bytes(PNG_MAGIC), Some("image/png"));
        assert_eq!(detect_media_type_from_bytes(JPEG_MAGIC), Some("image/jpeg"));
        assert_eq!(detect_media_type_from_bytes(GIF_MAGIC), Some("image/gif"));
        assert_eq!(detect_media_type_from_bytes(WEBP_MAGIC), Some("image/webp"));
        // Unrecognized / too short → None (caller defaults to PNG).
        assert_eq!(detect_media_type_from_bytes(b"not an image"), None);
        assert_eq!(detect_media_type_from_bytes(b"ab"), None);
        // "RIFF" without the "WEBP" fourcc is not WebP.
        assert_eq!(
            detect_media_type_from_bytes(b"RIFF\x00\x00\x00\x00AVI "),
            None
        );
    }

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("lingxi_img_test_{name}"));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    fn loaded_media_type(p: &Path) -> String {
        match load_image_source(p).unwrap() {
            ImageSource::Base64 { media_type, data } => {
                assert!(!data.is_empty());
                media_type
            }
            ImageSource::Url { .. } => panic!("expected base64"),
        }
    }

    #[test]
    fn png_extension_holding_jpeg_bytes_is_sent_as_jpeg() {
        // The key parity case: the extension lies, the bytes are authoritative.
        let p = write_temp("liar.png", JPEG_MAGIC);
        assert_eq!(loaded_media_type(&p), "image/jpeg");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn each_format_sniffed_through_load() {
        for (name, magic, expect) in [
            ("a.png", PNG_MAGIC, "image/png"),
            ("a.jpg", JPEG_MAGIC, "image/jpeg"),
            ("a.gif", GIF_MAGIC, "image/gif"),
            ("a.webp", WEBP_MAGIC, "image/webp"),
        ] {
            let p = write_temp(name, magic);
            assert_eq!(loaded_media_type(&p), expect, "for {name}");
            let _ = std::fs::remove_file(&p);
        }
    }

    #[test]
    fn unknown_bytes_default_to_png() {
        // Unrecognized bytes + unrecognized extension → PNG default (TS default).
        let p = write_temp("mystery.dat", b"definitely not an image");
        assert_eq!(loaded_media_type(&p), "image/png");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unknown_bytes_fall_back_to_extension_hint() {
        // Unrecognized bytes but a known extension → extension hint is used.
        let p = write_temp("hint.gif", b"xx");
        assert_eq!(loaded_media_type(&p), "image/gif");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn unreadable_file_errors() {
        // Read failure (missing file) is still the only error path.
        assert!(load_image_source(Path::new("/tmp/lingxi_nonexistent_xyz.bmp")).is_err());
    }
}
