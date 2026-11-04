from pathlib import Path


def replace_once(path, old, new):
    text = path.read_text(encoding="utf-8")
    if text.count(old) != 1:
        raise ValueError(f"expected exactly one occurrence in {path}: {old!r}")
    path.write_text(text.replace(old, new, 1), encoding="utf-8")


gate = Path("crates/platforms/windows/src/process/supervisor_gate.rs")
anchor = "    #[tokio::test]\n    async fn armed_payload_job_drop_still_terminates_its_child() {"
probe = '''    #[tokio::test]
    async fn diagnose_job_close_stdin_lifetimes() {
        for hold_stdin in [false, true] {
            for iteration in 0..4 {
                let mut command = tokio::process::Command::new("cmd.exe");
                command.args(["/D", "/S", "/C", "more"])
                    .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
                    .creation_flags(0x4).kill_on_drop(true);
                let mut child = command.spawn().unwrap();
                let job = CommandJob::assign_suspended(&child).unwrap();
                release(suspended_thread(&child).unwrap()).unwrap();
                let held_stdin = if hold_stdin { child.stdin.take() } else { None };
                tokio::time::sleep(Duration::from_millis(100)).await;
                let before = child.try_wait().unwrap();
                drop(job);
                let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                eprintln!("DIAG job_close hold_stdin={hold_stdin} iteration={iteration} before={before:?} after={status:?}");
                drop(held_stdin);
            }
        }
    }

'''
replace_once(gate, anchor, probe + anchor)
replace_once(gate, "        assert!(!status.success());", '        eprintln!("DIAG original job close status={status:?} code={:?}", status.code());\n        assert!(!status.success());')

background = Path("crates/platforms/windows/src/process/background.rs")
anchor = '''        {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("fixture.cmd"),'''
replace_once(background, anchor, '''        {
            let case = format!("{background},{blocked_stdin}");
            if std::env::var("WINDOWS_PROCESS_DIAG_CASE").ok().as_deref() != Some(case.as_str()) {
                continue;
            }
            eprintln!("DIAG starting stop case={case}");
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(
                directory.path().join("fixture.cmd"),''')
anchor = '''                assert!(
                    matches!(result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled")
                );
            }
            assert!(matches!(
                sink.settlements.lock().unwrap().as_slice(),
                [Ok(Some(143))]
            ));'''
replace_once(background, anchor, '''                eprintln!("DIAG stop case={case} result={result:?} settlements={:?}", sink.settlements.lock().unwrap());
                assert!(
                    matches!(&result, Err(ProcessError::Io(message)) if message == "supervisor command cancelled"),
                    "case={case} result={result:?}"
                );
            }
            eprintln!("DIAG final case={case} settlements={:?}", sink.settlements.lock().unwrap());
            assert!(matches!(
                sink.settlements.lock().unwrap().as_slice(),
                [Ok(Some(143))]
            ));''')
