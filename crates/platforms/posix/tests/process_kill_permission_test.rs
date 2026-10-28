//! Exercises real signal permission failures against fixture-owned process groups.
//!
//! The ignored test requires a Linux runner with passwordless sudo. The root
//! helper retains the exact child it creates and always waits for it; the test
//! never sends a signal to a process that the helper did not create.
#![cfg(target_os = "linux")]

use nix::errno::Errno;
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use platform_posix::process::kill_tree::{kill_tree_force, kill_tree_with_grace};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

// The root helper creates one session leader with the requested identity. It
// is the exact child's parent, polls its status, and kills and waits on that
// child in its finally block if the test fails or the control pipe closes.
const HELPER: &str = r#"
import os, subprocess, sys
uid, gid = map(int, sys.argv[1:3])
child = None
try:
    code = ('import os,signal; '
            'print(f"READY {os.getpid()} {os.getpgrp()} {os.getuid()}",flush=True); '
            'signal.pause()')
    child = subprocess.Popen([sys.executable, '-u', '-c', code],
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, text=True,
        user=uid, group=gid, extra_groups=[], start_new_session=True, cwd='/tmp')
    print(child.stdout.readline().strip(), flush=True)
    for line in sys.stdin:
        if line.strip() == 'POLL':
            status = child.poll()
            print('ALIVE' if status is None else f'EXIT {status}', flush=True)
        elif line.strip() == 'WAIT':
            print(f'EXIT {child.wait(timeout=10)}', flush=True)
        elif line.strip() == 'STOP':
            break
        else:
            raise RuntimeError('invalid fixture command')
finally:
    if child is not None:
        if child.poll() is None:
            child.kill()
        child.wait(timeout=10)
"#;

struct Fixture {
    helper: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    pid: u32,
}

impl Fixture {
    fn new(uid: u32, gid: u32) -> Self {
        let mut helper = Command::new("sudo")
            .args([
                "-n",
                "python3",
                "-u",
                "-c",
                HELPER,
                &uid.to_string(),
                &gid.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("start fixture helper with passwordless sudo");
        let input = helper.stdin.take().expect("fixture stdin");
        let mut output = BufReader::new(helper.stdout.take().expect("fixture stdout"));
        let mut ready = String::new();
        output.read_line(&mut ready).expect("fixture ready line");
        let fields: Vec<_> = ready.split_whitespace().collect();
        assert_eq!(fields.len(), 4, "fixture did not start: {ready:?}");
        assert_eq!(fields[0], "READY");
        let pid: u32 = fields[1].parse().unwrap();
        assert_eq!(fields[2], fields[1], "fixture must own its session/group");
        assert_eq!(fields[3].parse::<u32>().unwrap(), uid);
        Self {
            helper,
            input,
            output,
            pid,
        }
    }

    fn request(&mut self, command: &str) -> String {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        let mut reply = String::new();
        self.output.read_line(&mut reply).unwrap();
        reply.trim().to_owned()
    }

    fn finish(mut self) {
        writeln!(self.input, "STOP").unwrap();
        self.input.flush().unwrap();
        assert!(
            self.helper.wait().unwrap().success(),
            "fixture must reap its exact child"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = writeln!(self.input, "STOP");
        let _ = self.input.flush();
        let _ = self.helper.wait();
    }
}

fn user_id(flag: &str) -> u32 {
    let output = Command::new("id").arg(flag).output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// This is ignored by default because it creates an unprivileged fixture via
/// sudo. The hosted workflow opts in and treats timeout or missing sudo as a
/// failure, rather than interpreting an unrun fixture as a pass.
#[tokio::test]
#[ignore = "requires passwordless sudo and an unprivileged Linux runner"]
async fn eperm_is_not_a_successful_tree_kill() {
    let uid = user_id("-u");
    let gid = user_id("-g");
    assert_ne!(uid, 0, "the caller must not have signal privilege");
    assert_ne!(uid, 65534, "the denied fixture must have a different UID");

    let mut protected = Fixture::new(65534, 65534);
    let group = Pid::from_raw(i32::try_from(protected.pid).unwrap());
    assert_eq!(protected.request("POLL"), "ALIVE");
    assert_eq!(killpg(group, None), Err(Errno::EPERM));
    let denied = kill_tree_force(protected.pid);
    assert_eq!(protected.request("POLL"), "ALIVE");
    assert!(
        denied.is_err(),
        "signal denied but kill_tree_force returned Ok while protected leader is alive"
    );
    assert!(format!("{:?}", denied.unwrap_err()).contains("EPERM"));
    let graceful = kill_tree_with_grace(protected.pid, Duration::ZERO).await;
    assert_eq!(protected.request("POLL"), "ALIVE");
    assert!(format!("{:?}", graceful.unwrap_err()).contains("EPERM"));
    println!("EPERM propagated while protected leader remained alive");
    protected.finish(); // root helper kills and waits for its own exact child

    // Positive control: the same caller may kill a same-UID group, and the
    // helper independently waits for its exact child to confirm actual exit.
    let mut permitted = Fixture::new(uid, gid);
    assert_eq!(permitted.request("POLL"), "ALIVE");
    kill_tree_force(permitted.pid).expect("same-UID signal succeeds");
    assert_eq!(permitted.request("WAIT"), "EXIT -9");
    println!("same-UID leader signaled and reaped by its parent");
    permitted.finish();

    // Existing no-such-group behavior remains an accepted signal operation,
    // but this result is not a descendant reaping receipt.
    let absent = i32::MAX as u32;
    assert_eq!(killpg(Pid::from_raw(i32::MAX), None), Err(Errno::ESRCH));
    assert!(kill_tree_force(absent).is_ok());
    println!("absent group signaled as ESRCH; no descendant receipt claimed");
}
