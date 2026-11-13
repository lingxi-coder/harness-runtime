//! Explicit desktop system-rg opt-out. Candidates still come from the rooted,
//! permission-filtered walk; the executable receives only an already-pinned
//! stdin descriptor. Anonymous capture files avoid detached pipe-drain threads.

use super::{json_event_text, GrepContent, SearchControl};
use crate::shared::{SearchReader, SearchStop, SearchWalkError};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

pub(super) struct Options<'a> {
    pub(super) command: &'a Path,
    pub(super) pattern: &'a str,
    pub(super) case_insensitive: bool,
    pub(super) multiline: bool,
    pub(super) single_file: bool,
    pub(super) files_mode: bool,
    pub(super) count_mode: bool,
    pub(super) only_matching: bool,
    pub(super) context_before: Option<usize>,
    pub(super) context_after: Option<usize>,
}

struct OwnedChild(Option<Child>);

impl OwnedChild {
    fn exited(&self) -> std::io::Result<bool> {
        use rustix::process::{waitid, Pid, WaitId, WaitidOptions};
        let child = self.0.as_ref().expect("owned child is unreaped");
        let raw =
            i32::try_from(child.id()).map_err(|_| std::io::Error::other("child PID overflow"))?;
        let pid = Pid::from_raw(raw).ok_or_else(|| std::io::Error::other("invalid child PID"))?;
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitidOptions::EXITED | WaitidOptions::NOHANG | WaitidOptions::NOWAIT,
            ) {
                Ok(status) => return Ok(status.is_some()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn finish(&mut self) -> Result<ExitStatus, SearchWalkError> {
        let child = self.0.as_mut().expect("owned child is unreaped");
        // The leader is still unreaped, including on natural success. Its PID
        // therefore cannot be reused before all group members are retired.
        platform_posix::process::kill_tree::kill_tree_force(child.id())
            .map_err(|error| SearchWalkError::Engine(error.to_string()))?;
        let status = child.wait().map_err(engine_error)?;
        self.0 = None;
        Ok(status)
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = platform_posix::process::kill_tree::kill_tree_force(child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn engine_error(error: impl std::fmt::Display) -> SearchWalkError {
    SearchWalkError::Engine(format!("system ripgrep: {error}"))
}

/// Return match count and whether the child reached its deadline. Deadline
/// output is a bounded snapshot of bytes written before kill+wait completed;
/// cancellation drops that output and surfaces Aborted through the caller.
pub(super) fn search(
    file: &mut File,
    options: Options<'_>,
    control: &SearchControl,
    mut content: Option<&mut GrepContent<'_>>,
) -> Result<(usize, bool), SearchWalkError> {
    control
        .check_cancellation()
        .map_err(|_| SearchWalkError::Cancelled)?;
    if !options.single_file {
        // stdin is explicit input to rg. Directory-discovered binary candidates
        // have the native first-buffer binary gate before that explicit-input
        // execution. Keep BOM-based UTF-16 decoding available to rg itself.
        let mut prefix = Vec::new();
        SearchReader {
            inner: file,
            control,
        }
        .take(64 * 1024)
        .read_to_end(&mut prefix)
        .map_err(engine_error)?;
        file.seek(SeekFrom::Start(0)).map_err(engine_error)?;
        if !prefix.starts_with(&[0xff, 0xfe])
            && !prefix.starts_with(&[0xfe, 0xff])
            && prefix.contains(&0)
        {
            return Ok((0, false));
        }
    }
    control
        .check_cancellation()
        .map_err(|_| SearchWalkError::Cancelled)?;
    let mut stdout = tempfile::tempfile().map_err(engine_error)?;
    let mut stderr = tempfile::tempfile().map_err(engine_error)?;
    let mut command = Command::new(options.command);
    command.args(["--max-columns", "500"]);
    if options.files_mode {
        command.args(["--files-with-matches", "--null"]);
    } else if options.count_mode {
        command.args(["--count", "--with-filename", "--null"]);
    } else {
        command.args(["--json", "--line-number"]);
    }
    if options.case_insensitive {
        command.arg("--ignore-case");
    }
    if options.multiline {
        command.args(["--multiline", "--multiline-dotall"]);
    }
    if options.only_matching {
        command.arg("--only-matching");
    }
    if let Some(before) = options.context_before {
        command.arg("--before-context").arg(before.to_string());
    }
    if let Some(after) = options.context_after {
        command.arg("--after-context").arg(after.to_string());
    }
    command
        .arg("--regexp")
        .arg(options.pattern)
        .arg("--")
        .arg("-");
    command
        .stdin(Stdio::from(file.try_clone().map_err(engine_error)?))
        .stdout(Stdio::from(stdout.try_clone().map_err(engine_error)?))
        .stderr(Stdio::from(stderr.try_clone().map_err(engine_error)?));
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    let mut child = OwnedChild(Some(command.spawn().map_err(engine_error)?));
    let mut timed_out = false;
    loop {
        match control.check() {
            Err(SearchStop::Cancelled) => return Err(SearchWalkError::Cancelled),
            Err(SearchStop::TimedOut) => {
                timed_out = true;
                break;
            }
            Ok(()) => {}
        }
        if child.exited().map_err(engine_error)? {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = child.finish()?;
    control
        .check_cancellation()
        .map_err(|_| SearchWalkError::Cancelled)?;
    if !timed_out && !matches!(status.code(), Some(0 | 1)) {
        stderr.seek(SeekFrom::Start(0)).map_err(engine_error)?;
        let mut diagnostic = String::new();
        stderr
            .take(64 * 1024)
            .read_to_string(&mut diagnostic)
            .map_err(engine_error)?;
        return Err(engine_error(format!(
            "{} exited with {status}: {}",
            options.command.display(),
            diagnostic.trim_end()
        )));
    }
    stdout.seek(SeekFrom::Start(0)).map_err(engine_error)?;
    let snapshot_len = stdout.metadata().map_err(engine_error)?.len();
    if options.files_mode {
        // rg emits exactly one stdin name for -l; an empty captured stream is
        // no match, including a deadline before the first result was written.
        return Ok((usize::from(snapshot_len > 0), timed_out));
    }
    if options.count_mode {
        let mut result = String::new();
        stdout
            .take(snapshot_len.min(4096))
            .read_to_string(&mut result)
            .map_err(engine_error)?;
        let count = result.rsplit('\0').next().unwrap_or("").trim();
        if count.is_empty() {
            return Ok((0, timed_out));
        }
        let count = count.parse::<usize>().map_err(engine_error)?;
        return Ok((count, timed_out));
    }
    let mut reader = BufReader::new(stdout.take(snapshot_len));
    let mut row = String::new();
    let mut matches = 0;
    let mut recorded_any = false;
    if let Some(content) = &mut content {
        content.timed_out = timed_out;
    }
    loop {
        control
            .check_cancellation()
            .map_err(|_| SearchWalkError::Cancelled)?;
        row.clear();
        if reader.read_line(&mut row).map_err(engine_error)? == 0 {
            break;
        }
        // Native's JSON parser deliberately ignores partial/malformed events.
        let Ok(event) = serde_json::from_str::<Value>(&row) else {
            continue;
        };
        let data = &event["data"];
        match event["type"].as_str() {
            Some("match" | "context") => {
                let is_context = event["type"] == "context";
                if !is_context {
                    matches += 1;
                }
                if let Some(content) = &mut content {
                    let text = json_event_text(&data["lines"]);
                    content
                        .record(
                            data["line_number"].as_u64(),
                            text.as_bytes(),
                            is_context,
                            (!is_context).then(|| {
                                data["submatches"]
                                    .as_array()
                                    .map(Vec::as_slice)
                                    .unwrap_or(&[])
                            }),
                        )
                        .map_err(engine_error)?;
                    recorded_any = true;
                }
            }
            Some("end") => {
                if recorded_any {
                    if let Some(content) = &mut content {
                        content.finish(data["binary_offset"].as_u64());
                    }
                }
            }
            _ => {}
        }
    }
    Ok((matches, timed_out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grep::{SearchPage, SearchPagination};
    use std::path::PathBuf;
    use std::time::Instant;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn system_rg() -> Option<PathBuf> {
        let found = crate::ripgrep_mode::find_rg_on_path();
        if found.is_none() && std::env::var_os("LINGXI_REQUIRE_SYSTEM_RG_TESTS").is_some() {
            panic!("system-rg validation requested but no executable rg is installed");
        }
        if found.is_none() {
            eprintln!("system-rg runtime test skipped: rg not installed");
        }
        found
    }

    fn options<'a>(command: &'a Path, pattern: &'a str) -> Options<'a> {
        Options {
            command,
            pattern,
            case_insensitive: false,
            multiline: false,
            single_file: true,
            files_mode: false,
            count_mode: false,
            only_matching: false,
            context_before: None,
            context_after: None,
        }
    }

    fn executable(root: &Path, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join("owned-rg.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\"'\"'"))
    }

    #[test]
    fn system_rg_native_content_receipts() {
        let Some(command) = system_rg() else {
            return;
        };
        let fixture: Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../scripts/tests/fixtures/file_search_322.json"
        )))
        .unwrap();
        for case in fixture["searches"].as_array().unwrap() {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("a.txt");
            std::fs::write(&path, case["contents"].as_str().unwrap()).unwrap();
            let mut file = File::open(&path).unwrap();
            let input = &case["options"];
            let context = input["context"].as_u64().or_else(|| input["-C"].as_u64());
            let before = context.or_else(|| input["-B"].as_u64()).map(|n| n as usize);
            let after = context.or_else(|| input["-A"].as_u64()).map(|n| n as usize);
            let raw_context = ["context", "-C", "-B", "-A"]
                .iter()
                .any(|key| input[key].as_u64().is_some_and(|n| n > 0));
            let (control, _owner) =
                SearchControl::new(Instant::now() + Duration::from_secs(30), None);
            let mut page = SearchPage::new(SearchPagination::new(
                input["head_limit"].as_u64().map(|n| n as usize),
                input["offset"].as_u64().unwrap_or(0) as usize,
            ));
            let mut previous = None;
            let mut content = GrepContent {
                matcher: None,
                path: &path,
                relative: "a.txt".into(),
                page: &mut page,
                previous_line: &mut previous,
                only_matching: input["-o"] == true,
                show_line_numbers: input["-n"] != false,
                single_file: case["singleFile"] == true,
                context_breaks: raw_context,
                recorded_any: false,
                timed_out: false,
                control: &control,
            };
            let mut config = options(&command, input["pattern"].as_str().unwrap());
            config.single_file = case["singleFile"] == true;
            config.multiline = input["multiline"] == true;
            config.only_matching = input["-o"] == true;
            config.case_insensitive = input["-i"] == true;
            config.context_before = before;
            config.context_after = after;
            let (_, expired) = search(&mut file, config, &control, Some(&mut content)).unwrap();
            drop(content);
            assert!(!expired);
            assert_eq!(
                page.values.join("\n"),
                case["expected"]["content"].as_str().unwrap_or(""),
                "{}",
                case["name"]
            );
            assert_eq!(
                page.total as u64,
                case["expected"]["totalLines"].as_u64().unwrap_or(0),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn system_rg_count_and_files_modes_use_real_binary() {
        let Some(command) = system_rg() else {
            return;
        };
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("a.txt");
        std::fs::write(&path, "first\nhit hit\nhit\nlast\n").unwrap();
        for (pattern, multiline, files, expected) in [
            ("hit", false, false, 2),
            ("first\\nhit", true, false, 1),
            ("hit", false, true, 1),
            ("absent", false, true, 0),
        ] {
            let (control, _owner) =
                SearchControl::new(Instant::now() + Duration::from_secs(30), None);
            let mut config = options(&command, pattern);
            config.multiline = multiline;
            config.files_mode = files;
            config.count_mode = !files;
            let (count, expired) =
                search(&mut File::open(&path).unwrap(), config, &control, None).unwrap();
            assert_eq!(count, expected, "{pattern}");
            assert!(!expired);
        }
    }

    #[test]
    fn system_rg_uses_pinned_stdin_without_candidate_path_arguments() {
        use std::os::unix::fs::symlink;
        let Some(command) = system_rg() else {
            return;
        };
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("candidate.txt");
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&path, "safe\n").unwrap();
        std::fs::write(&outside, "EXTERNAL_SECRET\n").unwrap();
        let mut pinned = File::open(&path).unwrap();
        std::fs::rename(&path, tmp.path().join("original.txt")).unwrap();
        symlink(&outside, &path).unwrap();
        let argv = tmp.path().join("argv.txt");
        let wrapper = executable(
            tmp.path(),
            &format!(
                "printf '%s\\n' \"$@\" > {}\nexec {} \"$@\"",
                shell_quote(&argv),
                shell_quote(&command)
            ),
        );
        let (control, _owner) = SearchControl::new(Instant::now() + Duration::from_secs(30), None);
        let mut config = options(&wrapper, "safe");
        config.count_mode = true;
        let (count, expired) = search(&mut pinned, config, &control, None).unwrap();
        assert_eq!(count, 1);
        assert!(!expired);
        let argv = std::fs::read_to_string(argv).unwrap();
        assert!(argv.lines().any(|arg| arg == "-"));
        assert!(!argv.contains(path.to_str().unwrap()));
        assert!(!argv.contains(outside.to_str().unwrap()));
    }

    #[test]
    fn system_rg_deadline_reaps_group_and_returns_complete_partial_events() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        std::fs::write(&input, "unused").unwrap();
        let leaked = tmp.path().join("descendant-ran");
        let release = tmp.path().join("release-descendant");
        let ready = tmp.path().join("complete-event-written");
        let event = r#"{"type":"match","data":{"lines":{"text":"hit\n"},"line_number":1,"submatches":[{"match":{"text":"hit"},"start":0,"end":3}]}}"#;
        let wrapper = executable(tmp.path(), &format!(
            "(while [ ! -e {} ]; do sleep 0.01; done; printf leaked > {}) &\nprintf '%s\\n' '{}'\nprintf ready > {}\nsleep 30",
            shell_quote(&release), shell_quote(&leaked), event, shell_quote(&ready),
        ));
        let (control, _owner) = SearchControl::new(Instant::now() + Duration::from_secs(10), None);
        let deadline_ready = ready.clone();
        // Do not race shell startup against150ms of wall-clock time. The child
        // publishes readiness strictly after its complete JSON event is in the
        // capture file; exercise the same production timeout path only then.
        let control = control
            .with_test_stop(move || deadline_ready.is_file().then_some(SearchStop::TimedOut));
        let mut page = SearchPage::new(SearchPagination::new(None, 0));
        let mut previous = None;
        let mut content = GrepContent {
            matcher: None,
            path: &input,
            relative: "input".into(),
            page: &mut page,
            previous_line: &mut previous,
            only_matching: false,
            show_line_numbers: true,
            single_file: true,
            context_breaks: false,
            recorded_any: false,
            timed_out: false,
            control: &control,
        };
        let start = Instant::now();
        let (count, expired) = search(
            &mut File::open(&input).unwrap(),
            options(&wrapper, "hit"),
            &control,
            Some(&mut content),
        )
        .unwrap();
        drop(content);
        assert!(
            ready.is_file(),
            "fixture did not publish its complete event before the wall-clock guard"
        );
        assert!(expired);
        assert_eq!(count, 1);
        assert_eq!(page.values, ["1:hit"]);
        assert!(start.elapsed() < Duration::from_secs(10));
        // A surviving descendant can write only after search has returned.
        // Scheduling before timeout can never create a false leaked marker.
        std::fs::write(&release, "release").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!leaked.exists());
    }

    #[test]
    fn system_rg_deadline_before_results_returns_valid_empty_snapshot() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        std::fs::write(&input, "unused").unwrap();
        let wrapper = executable(tmp.path(), "sleep 30");
        let (control, _owner) = SearchControl::new(Instant::now(), None);
        let mut page = SearchPage::new(SearchPagination::new(None, 0));
        let mut previous = None;
        let mut content = GrepContent {
            matcher: None,
            path: &input,
            relative: "input".into(),
            page: &mut page,
            previous_line: &mut previous,
            only_matching: false,
            show_line_numbers: true,
            single_file: true,
            context_breaks: false,
            recorded_any: false,
            timed_out: false,
            control: &control,
        };
        let (count, expired) = search(
            &mut File::open(&input).unwrap(),
            options(&wrapper, "hit"),
            &control,
            Some(&mut content),
        )
        .unwrap();
        drop(content);
        assert!(expired);
        assert_eq!(count, 0);
        assert_eq!(page.total, 0);
        assert!(page.values.is_empty());
    }

    #[test]
    fn system_rg_cancellation_kills_descendants_and_reaps() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("input");
        std::fs::write(&input, "unused").unwrap();
        let ready = tmp.path().join("ready");
        let leaked = tmp.path().join("descendant-ran");
        let release = tmp.path().join("release-descendant");
        let wrapper = executable(
            tmp.path(),
            &format!(
                "(while [ ! -e {} ]; do sleep 0.01; done; printf leaked > {}) &\nprintf ready > {}\nwait",
                shell_quote(&release),
                shell_quote(&leaked),
                shell_quote(&ready)
            ),
        );
        let token = CancellationToken::new();
        let cancel = token.clone();
        let (control, _owner) =
            SearchControl::new(Instant::now() + Duration::from_secs(10), Some(token));
        let cancel_ready = ready.clone();
        // Trigger the real token only after the child has started. A separate
        // observer's short startup deadline can fail before cancellation is
        // exercised, and a timed descendant can run before a delayed observer.
        let control = control.with_test_stop(move || {
            if cancel_ready.is_file() {
                cancel.cancel();
            }
            None
        });
        let result = search(
            &mut File::open(&input).unwrap(),
            options(&wrapper, "unused"),
            &control,
            None,
        );
        assert!(ready.is_file(), "fixture did not start: {result:?}");
        assert_eq!(result.unwrap_err(), SearchWalkError::Cancelled);
        // Only a descendant surviving search's kill-and-reap can observe this.
        std::fs::write(&release, "release").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert!(!leaked.exists());
    }
}
