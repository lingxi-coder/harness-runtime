//! FileRead PDF page-image extraction: spawn `pdftoppm` (poppler-utils) to render
//! pages to JPEG, mirroring claude-code `src/utils/pdf.ts` `extractPDFPages`.
//! `pdftoppm` is a RUNTIME system dependency (no Rust crate). The pure helpers
//! (`build_pdftoppm_args`, `classify_pdftoppm_failure`) carry the logic
//! claude-code tests by mocking the subprocess; the spawn itself
//! (`render_pdf_pages`) is exercised by the availability-gated smoke test.

use once_cell::sync::Lazy;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::pdf_read::PDF_MAX_EXTRACT_SIZE;
use crate::read::run_read_job;

static PDF_RENDER_JOBS: Lazy<Arc<Semaphore>> = Lazy::new(|| Arc::new(Semaphore::new(2)));

/// pdftoppm render timeout (claude-code `execFileNoThrow` `timeout: 120_000`).
const PDFTOPPM_TIMEOUT: Duration = Duration::from_secs(120);

/// pdftoppm availability-probe timeout (claude-code `isPdftoppmAvailable`
/// `execFileNoThrow('pdftoppm', ['-v'], { timeout: 5000 })`).
const PDFTOPPM_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Structured failure of PDF page extraction — mirrors claude-code `PDFError`
/// (`src/utils/pdf.ts`). Each variant maps to a fixed user-facing message and a
/// stable telemetry code.
#[derive(Debug)]
pub enum PdfRenderError {
    /// 0-byte file.
    Empty,
    /// Larger than [`PDF_MAX_EXTRACT_SIZE`] (100 MB).
    TooLarge,
    /// `pdftoppm` (poppler-utils) is not installed / not on `PATH`.
    Unavailable,
    /// pdftoppm reported the PDF is password-protected.
    PasswordProtected,
    /// pdftoppm reported the PDF is damaged/corrupt/invalid.
    Corrupted,
    /// pdftoppm exited 0 but produced no `.jpg` pages.
    NoOutput,
    /// Any other non-zero exit; carries the captured stderr.
    Unknown(String),
}

impl PdfRenderError {
    /// The exact claude-code user-facing message for this failure. `path` is the
    /// PDF path (used only by [`Self::Empty`]).
    #[must_use]
    pub fn message(&self, path: &str) -> String {
        match self {
            Self::Empty => format!("PDF file is empty: {path}"),
            Self::TooLarge => format!(
                "PDF file exceeds maximum allowed size for text extraction ({}).",
                crate::read::format_file_size(PDF_MAX_EXTRACT_SIZE)
            ),
            Self::Unavailable => "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.".to_string(),
            Self::PasswordProtected => "PDF is password-protected. Please provide an unprotected version.".to_string(),
            Self::Corrupted => "PDF file is corrupted or invalid.".to_string(),
            Self::NoOutput => "pdftoppm produced no output pages. The PDF may be invalid.".to_string(),
            Self::Unknown(stderr) => format!("pdftoppm failed: {stderr}"),
        }
    }

    /// Stable telemetry code for `emit_failed`.
    #[must_use]
    pub fn telemetry_code(&self) -> &'static str {
        match self {
            Self::Empty => "pdf_empty",
            Self::TooLarge => "pdf_too_large",
            Self::Unavailable => "pdf_unavailable",
            Self::PasswordProtected => "pdf_password_protected",
            Self::Corrupted => "pdf_corrupted",
            Self::NoOutput => "pdf_no_output",
            Self::Unknown(_) => "pdf_unknown",
        }
    }
}

/// Build the `pdftoppm` argument vector (claude-code `src/utils/pdf.ts:222-230`).
/// `-jpeg -r 100` are always present. Then `-f first` is pushed ONLY when
/// `first != 0` — mirroring claude-code's `if (options?.firstPage)` (pdf.ts:224-226),
/// where `0` is JS-falsy/undefined so the flag is omitted. Then `-l last` UNLESS
/// `last == u32::MAX` (the open-ended `"N-"` range — claude-code's `Infinity` —
/// renders to the end). Finally the input path and the output `<dir>/page` prefix.
/// In practice the pages path always supplies `first >= 1`, so `-f` is present
/// for every real call; the `first == 0` branch exists purely for faithful parity.
#[must_use]
pub fn build_pdftoppm_args(first: u32, last: u32, input: &Path, prefix: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-jpeg".into(), "-r".into(), "100".into()];
    if first != 0 {
        args.push("-f".into());
        args.push(first.to_string().into());
    }
    if last != u32::MAX {
        args.push("-l".into());
        args.push(last.to_string().into());
    }
    args.push(input.as_os_str().to_owned());
    args.push(prefix.as_os_str().to_owned());
    args
}

/// Classify a non-zero `pdftoppm` exit by its stderr (claude-code
/// `src/utils/pdf.ts:236-259`): `/password/i` → password-protected;
/// `/damaged|corrupt|invalid/i` → corrupted; else `Unknown(stderr)`.
#[must_use]
pub fn classify_pdftoppm_failure(stderr: &str) -> PdfRenderError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("password") {
        PdfRenderError::PasswordProtected
    } else if lower.contains("damaged") || lower.contains("corrupt") || lower.contains("invalid") {
        PdfRenderError::Corrupted
    } else {
        PdfRenderError::Unknown(stderr.to_string())
    }
}

/// Whether `pdftoppm -v` runs (poppler-utils installed). A supervised
/// probe kills and reaps the process on timeout or when its caller is dropped.
pub async fn is_pdftoppm_available() -> bool {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    tokio::spawn(async move { probe_pdftoppm(&cancel).await })
        .await
        .unwrap_or(false)
}

async fn probe_pdftoppm(cancel: &CancellationToken) -> bool {
    let child = Command::new("pdftoppm")
        .arg("-v")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let Ok(child) = child else {
        return false;
    };
    match wait_for_process(child, cancel, PDFTOPPM_PROBE_TIMEOUT).await {
        Ok(out) => out.status.success() || !out.stderr.is_empty(),
        Err(_) => false,
    }
}

/// Render the already-authorized PDF snapshot to page images. The caller
/// retains its logical path for tool payloads and errors. pdftoppm opens a
/// private copy of `bytes`, so a later path swap cannot select another input.
/// Its inherited stderr diagnostics can name that private staged file.
///
/// # Errors
/// [`PdfRenderError`] for too-large input, missing pdftoppm, an invalid PDF,
/// no output pages, a render timeout, or any I/O failure.
pub async fn render_pdf_pages(
    bytes: Vec<u8>,
    original_size: u64,
    first: u32,
    last: u32,
) -> Result<Vec<Vec<u8>>, PdfRenderError> {
    if original_size == 0 {
        return Err(PdfRenderError::Empty);
    }
    if original_size > PDF_MAX_EXTRACT_SIZE {
        return Err(PdfRenderError::TooLarge);
    }
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    // Dropping a JoinHandle detaches its task. This supervisor retains process
    // and scratch ownership after caller cancellation until kill+wait and
    // off-runtime cleanup finish; the caller itself can return immediately.
    tokio::spawn(async move { render_snapshot(bytes, first, last, cancel).await })
        .await
        .map_err(|error| PdfRenderError::Unknown(error.to_string()))?
}

struct StagedPdf {
    dir: tempfile::TempDir,
    input: PathBuf,
    prefix: PathBuf,
}

fn stage_snapshot(bytes: Vec<u8>, cancel: &CancellationToken) -> Result<StagedPdf, PdfRenderError> {
    let dir = tempfile::Builder::new()
        .prefix("lingxi-pdf-")
        .tempdir()
        .map_err(|error| PdfRenderError::Unknown(error.to_string()))?;
    let input = dir.path().join("input.pdf");
    let prefix = dir.path().join("page");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&input)
        .map_err(|error| PdfRenderError::Unknown(error.to_string()))?;
    for chunk in bytes.chunks(64 * 1024) {
        if cancel.is_cancelled() {
            return Err(PdfRenderError::Unknown("aborted".to_string()));
        }
        file.write_all(chunk)
            .map_err(|error| PdfRenderError::Unknown(error.to_string()))?;
    }
    file.flush()
        .map_err(|error| PdfRenderError::Unknown(error.to_string()))?;
    drop(file);
    Ok(StagedPdf { dir, input, prefix })
}

async fn render_snapshot(
    bytes: Vec<u8>,
    first: u32,
    last: u32,
    cancel: CancellationToken,
) -> Result<Vec<Vec<u8>>, PdfRenderError> {
    let permit = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(PdfRenderError::Unknown("aborted".to_string())),
        permit = PDF_RENDER_JOBS.clone().acquire_owned() =>
            permit.map_err(|error| PdfRenderError::Unknown(error.to_string()))?,
    };
    if !probe_pdftoppm(&cancel).await {
        return Err(PdfRenderError::Unavailable);
    }
    // The supervisor must await staging/cleanup even after cancellation. Their
    // chunk loops observe the request token, while this uncancelled job token
    // prevents early return from releasing the process slot before cleanup.
    let job_cancel = CancellationToken::new();
    let staged = run_read_job(&job_cancel, {
        let cancel = cancel.clone();
        move |_| Ok(stage_snapshot(bytes, &cancel))
    })
    .await
    .map_err(read_job_error)??;
    let args = build_pdftoppm_args(first, last, &staged.input, &staged.prefix);
    let mut command = Command::new("pdftoppm");
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    supervise_staged_render(staged, permit, command, cancel, PDFTOPPM_TIMEOUT).await
}

async fn supervise_staged_render(
    staged: StagedPdf,
    permit: OwnedSemaphorePermit,
    mut command: Command,
    cancel: CancellationToken,
    timeout: Duration,
) -> Result<Vec<Vec<u8>>, PdfRenderError> {
    let rendered = if cancel.is_cancelled() {
        Err(PdfRenderError::Unknown("aborted".to_string()))
    } else {
        match command.spawn() {
            Ok(child) => wait_for_process(child, &cancel, timeout)
                .await
                .and_then(|output| {
                    if output.status.success() {
                        Ok(())
                    } else {
                        // Rendering inherits stderr; retain the accepted empty-
                        // stderr classifier and its legacy Unknown("") result.
                        Err(classify_pdftoppm_failure(""))
                    }
                }),
            Err(error) => Err(PdfRenderError::Unknown(error.to_string())),
        }
    };
    // Child termination has been awaited before any scratch files are read or
    // removed. Keep the process permit until cleanup completes on a worker.
    run_read_job(&CancellationToken::new(), move |_| {
        let result = rendered.and_then(|()| collect_pages(staged.dir.path(), &cancel));
        let cleaned = staged.dir.close();
        drop(permit);
        if let Err(error) = cleaned {
            // TempDir::drop previously ignored cleanup errors. Retain the
            // render/collection result even when explicit cleanup fails.
            tracing::warn!(%error, "PDF scratch cleanup failed");
        }
        Ok(result)
    })
    .await
    .map_err(read_job_error)?
}

async fn read_child_pipe<R: AsyncRead + Unpin>(pipe: Option<R>) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut bytes).await?;
    }
    Ok(bytes)
}

async fn wait_for_process(
    mut child: Child,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<Output, PdfRenderError> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let result = {
        // One deadline covers process exit AND pipe EOF, matching the old
        // timeout(wait_with_output). Direct read futures own their pipes and
        // close them when this combined future is dropped on timeout/cancel.
        let completed = async {
            let (status, stdout, stderr) = tokio::try_join!(
                child.wait(),
                read_child_pipe(stdout),
                read_child_pipe(stderr),
            )
            .map_err(|error| PdfRenderError::Unknown(error.to_string()))?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(PdfRenderError::Unknown("aborted".to_string())),
            output = completed => output,
            () = tokio::time::sleep(timeout) => Err(PdfRenderError::Unknown("pdftoppm timed out".to_string())),
        }
    };
    if result.is_err() {
        let _ = child.start_kill();
        // The combined borrow/readers have been dropped. Reap the owned child
        // before its supervisor removes scratch or releases the process slot.
        let _ = child.wait().await;
    }
    result
}

fn read_job_error(error: tool_api::tool_trait::ToolError) -> PdfRenderError {
    PdfRenderError::Unknown(
        error
            .model_facing_message()
            .string_at("")
            .unwrap_or_default()
            .to_string(),
    )
}

fn collect_pages(dir: &Path, cancel: &CancellationToken) -> Result<Vec<Vec<u8>>, PdfRenderError> {
    // Collect *.jpg, sort by filename (claude-code `entries.filter(...).sort()`).
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| PdfRenderError::Unknown(e.to_string()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jpg"))
        .collect();
    files.sort();

    if files.is_empty() {
        return Err(PdfRenderError::NoOutput);
    }

    let mut pages = Vec::with_capacity(files.len());
    let mut chunk = vec![0; 64 * 1024];
    for f in files {
        let mut page =
            std::fs::File::open(&f).map_err(|e| PdfRenderError::Unknown(e.to_string()))?;
        let mut bytes = Vec::new();
        loop {
            if cancel.is_cancelled() {
                // The job supervisor maps its cancelled token to Aborted.
                return Err(PdfRenderError::Unknown("aborted".to_string()));
            }
            let length = match page.read(&mut chunk) {
                Ok(length) => length,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(PdfRenderError::Unknown(error.to_string())),
            };
            if length == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..length]);
        }
        pages.push(bytes);
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn staged_render_input_retains_snapshot_after_original_changes_or_disappears() {
        for remove in [false, true] {
            let original = tempfile::tempdir().unwrap();
            let path = original.path().join("original.pdf");
            std::fs::write(&path, MINIMAL_PDF).unwrap();
            let captured = std::fs::read(&path).unwrap();
            if remove {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, b"replacement content").unwrap();
            }
            let staged = stage_snapshot(captured.clone(), &CancellationToken::new()).unwrap();
            let args = build_pdftoppm_args(1, 1, &staged.input, &staged.prefix);
            let render_input = Path::new(&args[args.len() - 2]);
            assert_ne!(render_input, path.as_path());
            assert_eq!(render_input, staged.input.as_path());
            assert_eq!(std::fs::read(render_input).unwrap(), captured);
        }
    }

    fn held_input_command(staged: &StagedPdf, ready: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "pdf_render::tests::render_supervisor_child_process",
                "--ignored",
                "--nocapture",
            ])
            .env("LINGXI_PDF_SUPERVISOR_TEST_INPUT", &staged.input)
            .env("LINGXI_PDF_SUPERVISOR_TEST_READY", ready)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }

    #[test]
    #[ignore = "subprocess helper for supervisor lifecycle tests"]
    fn render_supervisor_child_process() {
        let Some(input) = std::env::var_os("LINGXI_PDF_SUPERVISOR_TEST_INPUT") else {
            return;
        };
        let mode = std::env::var_os("LINGXI_PDF_SUPERVISOR_TEST_MODE");
        if mode.as_deref() == Some(std::ffi::OsStr::new("fail")) {
            eprintln!("could not read {}", Path::new(&input).display());
            std::process::exit(1);
        }
        let ready = std::env::var_os("LINGXI_PDF_SUPERVISOR_TEST_READY").unwrap();
        if mode.as_deref() == Some(std::ffi::OsStr::new("pipe-parent")) {
            let _descendant = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pdf_render::tests::render_supervisor_child_process",
                    "--ignored",
                    "--nocapture",
                ])
                .env("LINGXI_PDF_SUPERVISOR_TEST_MODE", "pipe-holder")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap();
            return;
        }
        if mode.as_deref() == Some(std::ffi::OsStr::new("pipe-holder")) {
            let release = std::env::var_os("LINGXI_PDF_SUPERVISOR_TEST_RELEASE").unwrap();
            let done = std::env::var_os("LINGXI_PDF_SUPERVISOR_TEST_DONE").unwrap();
            std::fs::write(ready, b"descendant holds stderr").unwrap();
            let escape = std::time::Instant::now() + Duration::from_secs(30);
            while !Path::new(&release).exists() && std::time::Instant::now() < escape {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = std::fs::write(done, b"released");
            return;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let _held_input = options.open(input).unwrap();
        std::fs::write(ready, b"child holds input").unwrap();
        loop {
            std::thread::park();
        }
    }

    async fn await_child_ready(ready: &Path) {
        tokio::time::timeout(Duration::from_secs(30), async {
            while !tokio::fs::try_exists(ready).await.unwrap_or(false) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("child opens the staged input");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropped_render_caller_reaps_child_before_cleanup_and_permit_release() {
        let cancel = CancellationToken::new();
        let staged = stage_snapshot(MINIMAL_PDF.to_vec(), &cancel).unwrap();
        let scratch = staged.dir.path().to_path_buf();
        let ready = scratch.join("child-ready");
        let command = held_input_command(&staged, &ready);
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let caller = tokio::spawn(async move {
            let _cancel_on_drop = cancel.clone().drop_guard();
            tokio::spawn(supervise_staged_render(
                staged,
                permit,
                command,
                cancel,
                Duration::from_secs(60),
            ))
            .await
        });
        await_child_ready(&ready).await;
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        let _released = tokio::time::timeout(Duration::from_secs(30), slots.acquire_owned())
            .await
            .expect("supervisor kills and reaps the child")
            .unwrap();
        assert!(
            !scratch.exists(),
            "cleanup completes before the slot is released"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn render_timeout_reaps_child_before_cleanup_and_keeps_error_text() {
        let cancel = CancellationToken::new();
        let staged = stage_snapshot(MINIMAL_PDF.to_vec(), &cancel).unwrap();
        let scratch = staged.dir.path().to_path_buf();
        let command = held_input_command(&staged, &scratch.join("child-ready"));
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            supervise_staged_render(staged, permit, command, cancel, Duration::ZERO),
        )
        .await
        .expect("timeout supervisor completes cleanup");
        assert!(
            matches!(result, Err(PdfRenderError::Unknown(error)) if error == "pdftoppm timed out")
        );
        assert_eq!(slots.available_permits(), 1);
        assert!(!scratch.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_failures_preserve_the_legacy_empty_stderr_error() {
        let cancel = CancellationToken::new();
        let staged = stage_snapshot(MINIMAL_PDF.to_vec(), &cancel).unwrap();
        let scratch = staged.dir.path().to_path_buf();
        let mut command = held_input_command(&staged, &scratch.join("child-ready"));
        command.env("LINGXI_PDF_SUPERVISOR_TEST_MODE", "fail");
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.acquire_owned().await.unwrap();
        let result =
            supervise_staged_render(staged, permit, command, cancel, Duration::from_secs(30)).await;
        assert!(matches!(result, Err(PdfRenderError::Unknown(error)) if error.is_empty()));
        assert!(!scratch.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn probe_deadline_includes_descendant_held_pipe_drain() {
        struct ReleaseOnDrop(PathBuf);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                let _ = std::fs::write(&self.0, b"release");
            }
        }

        let cancel = CancellationToken::new();
        let staged = stage_snapshot(MINIMAL_PDF.to_vec(), &cancel).unwrap();
        let ready = staged.dir.path().join("pipe-ready");
        let done = staged.dir.path().join("pipe-done");
        let release = staged.dir.path().join("pipe-release");
        let release_on_drop = ReleaseOnDrop(release.clone());
        let mut command = held_input_command(&staged, &ready);
        command
            .env("LINGXI_PDF_SUPERVISOR_TEST_MODE", "pipe-parent")
            .env("LINGXI_PDF_SUPERVISOR_TEST_RELEASE", &release)
            .env("LINGXI_PDF_SUPERVISOR_TEST_DONE", &done)
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        await_child_ready(&ready).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            while child.try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("probe parent has exited while its descendant retains stderr");

        let result = tokio::time::timeout(
            Duration::from_secs(3),
            wait_for_process(child, &cancel, Duration::from_millis(25)),
        )
        .await
        .expect("the common deadline also bounds pipe EOF");
        drop(release_on_drop);
        await_child_ready(&done).await;
        assert!(
            matches!(result, Err(PdfRenderError::Unknown(error)) if error == "pdftoppm timed out")
        );
    }

    #[test]
    fn collected_pages_preserve_sorted_filename_order_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("page-10.jpg"), b"tenth").unwrap();
        std::fs::write(dir.path().join("page-2.jpg"), b"second").unwrap();
        std::fs::write(dir.path().join("page-1.jpg"), b"first").unwrap();
        std::fs::write(dir.path().join("ignored.png"), b"ignored").unwrap();
        let pages = collect_pages(dir.path(), &CancellationToken::new()).unwrap();
        assert_eq!(
            pages,
            vec![b"first".to_vec(), b"tenth".to_vec(), b"second".to_vec()]
        );
    }

    fn args_as_strings(a: &[std::ffi::OsString]) -> Vec<String> {
        a.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn args_single_page_pushes_f_and_l() {
        let a = build_pdftoppm_args(5, 5, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec![
                "-jpeg",
                "-r",
                "100",
                "-f",
                "5",
                "-l",
                "5",
                "/in.pdf",
                "/tmp/x/page"
            ]
        );
    }

    #[test]
    fn args_range_pushes_f_and_l() {
        let a = build_pdftoppm_args(2, 7, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec![
                "-jpeg",
                "-r",
                "100",
                "-f",
                "2",
                "-l",
                "7",
                "/in.pdf",
                "/tmp/x/page"
            ]
        );
    }

    #[test]
    fn args_first_zero_omits_f() {
        // first == 0 models claude-code's JS-falsy `firstPage` (pdf.ts:224-226):
        // the `if (options?.firstPage)` guard is false, so -f is NOT pushed.
        let a = build_pdftoppm_args(0, 5, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec!["-jpeg", "-r", "100", "-l", "5", "/in.pdf", "/tmp/x/page"]
        );
    }

    #[test]
    fn args_open_ended_omits_l() {
        // u32::MAX models the open-ended "N-" range (claude-code Infinity): -l is
        // NOT pushed, so pdftoppm renders from firstPage to the end of the doc.
        let a = build_pdftoppm_args(3, u32::MAX, Path::new("/in.pdf"), Path::new("/tmp/x/page"));
        assert_eq!(
            args_as_strings(&a),
            vec!["-jpeg", "-r", "100", "-f", "3", "/in.pdf", "/tmp/x/page"]
        );
    }

    #[test]
    fn classify_password() {
        assert!(matches!(
            classify_pdftoppm_failure("Error: PDF file is password protected"),
            PdfRenderError::PasswordProtected
        ));
        // case-insensitive (claude-code /password/i)
        assert!(matches!(
            classify_pdftoppm_failure("Command Line Error: Incorrect PASSWORD"),
            PdfRenderError::PasswordProtected
        ));
    }

    #[test]
    fn classify_corrupted() {
        for s in [
            "the file is damaged",
            "May not be a PDF (corrupt)",
            "Invalid XRef",
        ] {
            assert!(
                matches!(classify_pdftoppm_failure(s), PdfRenderError::Corrupted),
                "stderr {s:?} should classify corrupted"
            );
        }
    }

    #[test]
    fn classify_unknown_carries_stderr() {
        match classify_pdftoppm_failure("some other failure") {
            PdfRenderError::Unknown(s) => assert_eq!(s, "some other failure"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn error_messages_match_claude_code() {
        assert_eq!(
            PdfRenderError::Empty.message("/a.pdf"),
            "PDF file is empty: /a.pdf"
        );
        assert_eq!(
            PdfRenderError::TooLarge.message("/a.pdf"),
            "PDF file exceeds maximum allowed size for text extraction (100MB)."
        );
        assert_eq!(
            PdfRenderError::Unavailable.message("/a.pdf"),
            "pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering."
        );
        assert_eq!(
            PdfRenderError::PasswordProtected.message("/a.pdf"),
            "PDF is password-protected. Please provide an unprotected version."
        );
        assert_eq!(
            PdfRenderError::Corrupted.message("/a.pdf"),
            "PDF file is corrupted or invalid."
        );
        assert_eq!(
            PdfRenderError::NoOutput.message("/a.pdf"),
            "pdftoppm produced no output pages. The PDF may be invalid."
        );
        assert_eq!(
            PdfRenderError::Unknown("boom".to_string()).message("/a.pdf"),
            "pdftoppm failed: boom"
        );
    }

    #[test]
    fn telemetry_codes_are_stable() {
        assert_eq!(PdfRenderError::Empty.telemetry_code(), "pdf_empty");
        assert_eq!(PdfRenderError::TooLarge.telemetry_code(), "pdf_too_large");
        assert_eq!(
            PdfRenderError::Unavailable.telemetry_code(),
            "pdf_unavailable"
        );
        assert_eq!(
            PdfRenderError::PasswordProtected.telemetry_code(),
            "pdf_password_protected"
        );
        assert_eq!(PdfRenderError::Corrupted.telemetry_code(), "pdf_corrupted");
        assert_eq!(PdfRenderError::NoOutput.telemetry_code(), "pdf_no_output");
        assert_eq!(
            PdfRenderError::Unknown(String::new()).telemetry_code(),
            "pdf_unknown"
        );
    }

    // A valid minimal 1-page PDF (same fixture pdf_read uses). pdftoppm renders one
    // blank page from it.
    const MINIMAL_PDF: &[u8] = b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\nxref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \ntrailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n186\n%%EOF\n";

    #[tokio::test]
    async fn render_pdf_pages_smoke() {
        // ENV-GATED: poppler-utils may be absent (dev machine + some CI). Skip cleanly
        // rather than fail — the parity-critical logic is covered by the pure-helper
        // and build_pages_payload tests; this only exercises the real spawn when it can.
        if !is_pdftoppm_available().await {
            eprintln!("skipping render_pdf_pages_smoke: pdftoppm not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("one.pdf");
        std::fs::write(&pdf, MINIMAL_PDF).unwrap();

        let pages = render_pdf_pages(MINIMAL_PDF.to_vec(), MINIMAL_PDF.len() as u64, 1, 1)
            .await
            .expect("render a 1-page PDF");
        assert_eq!(pages.len(), 1, "one JPEG for a 1-page PDF");
        assert!(pages[0].starts_with(&[0xFF, 0xD8]), "JPEG SOI marker");
    }

    #[tokio::test]
    async fn render_rejects_oversize() {
        // No pdftoppm needed: the size guard short-circuits before the spawn.
        let dir = tempfile::tempdir().unwrap();
        let pdf = dir.path().join("big.pdf");
        std::fs::write(&pdf, MINIMAL_PDF).unwrap();
        let err = render_pdf_pages(Vec::new(), PDF_MAX_EXTRACT_SIZE + 1, 1, 1)
            .await
            .unwrap_err();
        assert!(matches!(err, PdfRenderError::TooLarge));
    }
}
