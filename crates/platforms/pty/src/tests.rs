use std::collections::HashMap;
#[cfg(unix)]
use std::path::Path;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::spawn_pty_process;
use crate::ProcessSignal;
use crate::SpawnedProcess;
use crate::TerminalSize;

fn environment() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(unix)]
async fn spawn_shell(script: &str, size: TerminalSize) -> anyhow::Result<SpawnedProcess> {
    spawn_pty_process(
        "/bin/sh",
        &["-c".to_owned(), script.to_owned()],
        Path::new("/"),
        &environment(),
        &None,
        size,
        &[],
    )
    .await
}

async fn read_until(
    output: &mut mpsc::Receiver<Vec<u8>>,
    marker: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let mut collected = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(chunk) = output.recv().await {
            collected.extend_from_slice(&chunk);
            if collected
                .windows(marker.len())
                .any(|window| window == marker)
            {
                return Ok(());
            }
        }
        anyhow::bail!(
            "PTY output closed before marker {:?}",
            String::from_utf8_lossy(marker)
        )
    })
    .await??;
    Ok(collected)
}

#[cfg(windows)]
async fn spawn_powershell(script: &str, size: TerminalSize) -> anyhow::Result<SpawnedProcess> {
    let cwd = std::env::current_dir()?;
    spawn_pty_process(
        "powershell.exe",
        &[
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ],
        &cwd,
        &environment(),
        &None,
        size,
        &[],
    )
    .await
}

#[cfg(windows)]
fn windows_process_is_running(process_id: u32) -> bool {
    use std::os::windows::io::RawHandle;

    use winapi::shared::minwindef::DWORD;
    use winapi::um::handleapi::CloseHandle;
    use winapi::um::minwinbase::STILL_ACTIVE;
    use winapi::um::processthreadsapi::GetExitCodeProcess;
    use winapi::um::processthreadsapi::OpenProcess;
    use winapi::um::winnt::PROCESS_QUERY_LIMITED_INFORMATION;

    let handle: RawHandle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) as RawHandle };
    if handle.is_null() {
        return false;
    }

    let mut exit_code: DWORD = 0;
    let queried = unsafe { GetExitCodeProcess(handle.cast(), &mut exit_code) };
    unsafe {
        CloseHandle(handle.cast());
    }
    queried != 0 && exit_code == STILL_ACTIVE
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_interaction_and_wait_are_lossless() -> anyhow::Result<()> {
    let mut spawned = spawn_shell(
        "stty -echo; printf '__READY__\\n'; IFS= read -r line; printf '__GOT__%s\\n' \"$line\"",
        TerminalSize::default(),
    )
    .await?;
    read_until(&mut spawned.stdout_rx, b"__READY__").await?;

    spawned
        .session
        .write("你好, PTY\n".as_bytes().to_vec())
        .await?;
    let output = read_until(&mut spawned.stdout_rx, "__GOT__你好, PTY".as_bytes()).await?;
    assert!(String::from_utf8_lossy(&output).contains("__GOT__你好, PTY"));

    let (wait_code, exit_code) = tokio::join!(spawned.session.wait(), spawned.exit_rx);
    assert_eq!(wait_code, 0);
    assert_eq!(exit_code?, 0);
    assert!(spawned.session.has_exited());
    assert_eq!(spawned.session.exit_code(), Some(0));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_tail_remains_readable_after_exit_notification() -> anyhow::Result<()> {
    let mut spawned = spawn_shell("printf '__TAIL_AFTER_WAIT__'", TerminalSize::default()).await?;

    // The child waiter and PTY reader are intentionally independent. Even
    // when wait wins the race, the reader must remain alive long enough to
    // publish bytes written immediately before exit.
    assert_eq!(spawned.exit_rx.await?, 0);
    let output = read_until(&mut spawned.stdout_rx, b"__TAIL_AFTER_WAIT__").await?;
    assert!(String::from_utf8_lossy(&output).contains("__TAIL_AFTER_WAIT__"));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_reaches_the_child_terminal() -> anyhow::Result<()> {
    let mut spawned = spawn_shell(
        "stty -echo; stty size; printf '__READY__\\n'; IFS= read -r line; stty size",
        TerminalSize { rows: 24, cols: 80 },
    )
    .await?;
    let initial = read_until(&mut spawned.stdout_rx, b"__READY__").await?;
    assert!(String::from_utf8_lossy(&initial).contains("24 80"));

    spawned.session.resize(TerminalSize {
        rows: 47,
        cols: 133,
    })?;
    spawned.session.write(b"continue\n".to_vec()).await?;
    let resized = read_until(&mut spawned.stdout_rx, b"47 133").await?;
    assert!(String::from_utf8_lossy(&resized).contains("47 133"));
    assert_eq!(spawned.exit_rx.await?, 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_targets_the_foreground_process_group() -> anyhow::Result<()> {
    let mut spawned = spawn_shell(
        "trap 'printf __INTERRUPTED__; exit 42' INT; printf '__READY__\\n'; while :; do sleep 1; done",
        TerminalSize::default(),
    )
    .await?;
    read_until(&mut spawned.stdout_rx, b"__READY__").await?;
    spawned.session.signal(ProcessSignal::Interrupt)?;
    let output = read_until(&mut spawned.stdout_rx, b"__INTERRUPTED__").await?;
    assert!(String::from_utf8_lossy(&output).contains("__INTERRUPTED__"));
    assert_eq!(spawned.exit_rx.await?, 42);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminate_kills_descendants_in_the_same_process_group() -> anyhow::Result<()> {
    let mut spawned = spawn_shell(
        "sleep 1000 & child=$!; printf '__CHILD__%s\\n' \"$child\"; wait",
        TerminalSize::default(),
    )
    .await?;
    let output = read_until(&mut spawned.stdout_rx, b"\n").await?;
    let text = String::from_utf8_lossy(&output).replace('\r', "");
    let child_pid: i32 = text
        .lines()
        .find_map(|line| line.strip_prefix("__CHILD__"))
        .ok_or_else(|| anyhow::anyhow!("missing child PID in {text:?}"))?
        .parse()?;

    spawned.session.request_terminate();
    let exit_code = tokio::time::timeout(Duration::from_secs(5), spawned.exit_rx).await??;
    assert_ne!(exit_code, 0);

    let child_gone = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = unsafe { libc::kill(child_pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        child_gone.is_ok(),
        "descendant {child_pid} survived PTY termination"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_stdin_delivers_eof() -> anyhow::Result<()> {
    let mut spawned = spawn_shell(
        "stty -echo; printf '__READY__\\n'; cat >/dev/null; printf '__EOF__'",
        TerminalSize::default(),
    )
    .await?;
    read_until(&mut spawned.stdout_rx, b"__READY__").await?;
    spawned.session.close_stdin();
    let output = read_until(&mut spawned.stdout_rx, b"__EOF__").await?;
    assert!(String::from_utf8_lossy(&output).contains("__EOF__"));
    assert_eq!(spawned.exit_rx.await?, 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_zero_terminal_dimensions() -> anyhow::Result<()> {
    let error = spawn_shell("true", TerminalSize { rows: 0, cols: 80 })
        .await
        .expect_err("zero dimensions must fail");
    assert!(error.to_string().contains("greater than zero"));
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_raw_interaction_and_wait_are_lossless() -> anyhow::Result<()> {
    assert!(crate::conpty_supported(), "CI host must provide ConPTY");
    let mut spawned = spawn_powershell(
        "$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); \
         Write-Output '__READY__'; \
         $line = [Console]::In.ReadLine(); \
         Write-Output ('__GOT__' + $line)",
        TerminalSize::default(),
    )
    .await?;
    read_until(&mut spawned.stdout_rx, b"__READY__").await?;

    spawned
        .session
        .write("你好, Windows ConPTY\n".as_bytes().to_vec())
        .await?;
    let marker = "__GOT__你好, Windows ConPTY";
    let output = read_until(&mut spawned.stdout_rx, marker.as_bytes()).await?;
    assert!(String::from_utf8_lossy(&output).contains(marker));
    assert_eq!(spawned.exit_rx.await?, 0);
    assert_eq!(spawned.session.wait().await, 0);
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_output_tail_remains_readable_after_exit() -> anyhow::Result<()> {
    let mut spawned = spawn_powershell(
        "[Console]::Out.Write('__TAIL_AFTER_WAIT__')",
        TerminalSize::default(),
    )
    .await?;

    assert_eq!(spawned.exit_rx.await?, 0);
    let output = read_until(&mut spawned.stdout_rx, b"__TAIL_AFTER_WAIT__").await?;
    assert!(String::from_utf8_lossy(&output).contains("__TAIL_AFTER_WAIT__"));
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_resize_reaches_the_child_terminal() -> anyhow::Result<()> {
    let mut spawned = spawn_powershell(
        "Write-Output '__READY__'; \
         [void][Console]::In.ReadLine(); \
         Start-Sleep -Milliseconds 100; \
         Write-Output ('__SIZE__{0} {1}' -f [Console]::WindowHeight, [Console]::WindowWidth)",
        TerminalSize { rows: 24, cols: 80 },
    )
    .await?;
    read_until(&mut spawned.stdout_rx, b"__READY__").await?;

    spawned.session.resize(TerminalSize {
        rows: 47,
        cols: 133,
    })?;
    spawned.session.write(b"continue\n".to_vec()).await?;
    let output = read_until(&mut spawned.stdout_rx, b"__SIZE__47 133").await?;
    assert!(String::from_utf8_lossy(&output).contains("__SIZE__47 133"));
    assert_eq!(spawned.exit_rx.await?, 0);
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_job_object_termination_kills_descendants() -> anyhow::Result<()> {
    let mut spawned = spawn_powershell(
        "$child = Start-Process powershell.exe \
             -ArgumentList '-NoLogo','-NoProfile','-Command','Start-Sleep -Seconds 1000' \
             -PassThru; \
         Write-Output ('__CHILD__' + $child.Id); \
         Wait-Process -Id $child.Id",
        TerminalSize::default(),
    )
    .await?;
    let output = read_until(&mut spawned.stdout_rx, b"\n").await?;
    let text = String::from_utf8_lossy(&output).replace('\r', "");
    let child_pid: u32 = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("__CHILD__"))
        .ok_or_else(|| anyhow::anyhow!("missing child PID in {text:?}"))?
        .parse()?;
    assert!(windows_process_is_running(child_pid));

    spawned.session.request_terminate();
    let exit_code = tokio::time::timeout(Duration::from_secs(15), spawned.exit_rx).await??;
    assert_ne!(exit_code, 0);

    tokio::time::timeout(Duration::from_secs(5), async {
        while windows_process_is_running(child_pid) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("descendant {child_pid} survived Job Object termination"))?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_close_stdin_delivers_eof() -> anyhow::Result<()> {
    let mut spawned = spawn_powershell(
        "Write-Output '__READY__'; \
         [void][Console]::In.ReadToEnd(); \
         Write-Output '__EOF__'",
        TerminalSize::default(),
    )
    .await?;
    read_until(&mut spawned.stdout_rx, b"__READY__").await?;

    spawned.session.close_stdin();
    let output = read_until(&mut spawned.stdout_rx, b"__EOF__").await?;
    assert!(String::from_utf8_lossy(&output).contains("__EOF__"));
    assert_eq!(spawned.exit_rx.await?, 0);
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_reports_unsupported_interactive_interrupt() -> anyhow::Result<()> {
    let spawned = spawn_powershell("Start-Sleep -Seconds 1000", TerminalSize::default()).await?;
    let error = spawned
        .session
        .signal(ProcessSignal::Interrupt)
        .expect_err("Windows ConPTY cannot synthesize a Unix SIGINT");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    spawned.session.terminate();
    assert_ne!(spawned.exit_rx.await?, 0);
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conpty_rejects_zero_terminal_dimensions() -> anyhow::Result<()> {
    let error = spawn_powershell("exit 0", TerminalSize { rows: 24, cols: 0 })
        .await
        .expect_err("zero dimensions must fail");
    assert!(error.to_string().contains("greater than zero"));
    Ok(())
}
