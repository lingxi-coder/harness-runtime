#![allow(non_snake_case)]
use std::{
    collections::HashSet,
    ffi::c_void,
    fs,
    io::{self, Write},
    mem,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr, thread,
    time::{Duration, Instant},
};
type Handle = *mut c_void;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const CREATE_SUSPENDED: u32 = 4;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const JOB_OBJECT_LIMIT_JOB_TIME: u32 = 4;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const DUPLICATE_SAME_ACCESS: u32 = 2;
const PROCESS_DUP_HANDLE: u32 = 0x0040;

#[repr(C)]
#[derive(Default)]
struct Accounting {
    total_user: i64,
    total_kernel: i64,
    this_user: i64,
    this_kernel: i64,
    faults: u32,
    total: u32,
    active: u32,
    terminated: u32,
}
#[repr(C)]
#[derive(Default)]
struct BasicLimit {
    process_time: i64,
    job_time: i64,
    flags: u32,
    min_ws: usize,
    max_ws: usize,
    active_limit: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_ops: u64,
    write_ops: u64,
    other_ops: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
}
#[repr(C)]
#[derive(Default)]
struct ExtendedLimit {
    basic: BasicLimit,
    io: IoCounters,
    process_memory: usize,
    job_memory: usize,
    peak_process: usize,
    peak_job: usize,
}
#[repr(C)]
#[derive(Default)]
struct Startup {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    xsize: u32,
    ysize: u32,
    xchars: u32,
    ychars: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved_size: u16,
    reserved_ptr: *mut u8,
    stdin: Handle,
    stdout: Handle,
    stderr: Handle,
}
#[repr(C)]
#[derive(Default)]
struct ProcessInfo {
    process: Handle,
    thread: Handle,
    pid: u32,
    tid: u32,
}
// These are the documented 64-bit Windows ABI structure sizes.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(mem::size_of::<Accounting>() == 48);
    assert!(mem::size_of::<BasicLimit>() == 64);
    assert!(mem::size_of::<IoCounters>() == 48);
    assert!(mem::size_of::<ExtendedLimit>() == 144);
    assert!(mem::size_of::<Startup>() == 104);
    assert!(mem::size_of::<ProcessInfo>() == 24);
};
#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(security: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: i32, data: *const c_void, len: u32) -> i32;
    fn QueryInformationJobObject(
        job: Handle,
        class: i32,
        data: *mut c_void,
        len: u32,
        returned: *mut u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn TerminateJobObject(job: Handle, code: u32) -> i32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn ResumeThread(thread: Handle) -> u32;
    fn CreateProcessW(
        app: *const u16,
        cmd: *mut u16,
        proc_attr: *const c_void,
        thread_attr: *const c_void,
        inherit: i32,
        flags: u32,
        env: *const c_void,
        cwd: *const u16,
        startup: *const Startup,
        info: *mut ProcessInfo,
    ) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn DuplicateHandle(
        source_proc: Handle,
        source: Handle,
        target_proc: Handle,
        target: *mut Handle,
        access: u32,
        inherit: i32,
        options: u32,
    ) -> i32;
}
fn raw(h: &OwnedHandle) -> Handle {
    h.as_raw_handle()
}
fn require(ok: i32, op: &str) {
    assert_ne!(ok, 0, "{op}: {}", io::Error::last_os_error());
}
fn wait(h: &OwnedHandle, ms: u32) -> u32 {
    unsafe { WaitForSingleObject(raw(h), ms) }
}
fn assert_live(h: &OwnedHandle, label: &str) {
    assert_eq!(wait(h, 0), WAIT_TIMEOUT, "{label} unexpectedly not live");
}
fn assert_ended(h: &OwnedHandle, label: &str) {
    let w = wait(h, 0);
    println!("handle_check name={label} wait={w}");
    assert_eq!(
        w, WAIT_OBJECT_0,
        "{label} remains unsignaled after job signal"
    );
}
struct Job {
    h: OwnedHandle,
}
impl Job {
    fn new() -> Self {
        let h = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        assert!(
            !h.is_null(),
            "CreateJobObjectW: {}",
            io::Error::last_os_error()
        );
        let h = unsafe { OwnedHandle::from_raw_handle(h) };
        let mut limits = ExtendedLimit::default();
        limits.basic.flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        require(
            unsafe {
                SetInformationJobObject(
                    raw(&h),
                    9,
                    (&limits as *const ExtendedLimit).cast(),
                    mem::size_of::<ExtendedLimit>() as u32,
                )
            },
            "set kill on close",
        );
        Self { h }
    }
    fn assign(&self, p: &Proc) -> Result<(), io::Error> {
        if unsafe { AssignProcessToJobObject(raw(&self.h), raw(&p.h)) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn member(&self, h: &OwnedHandle) -> bool {
        let mut value = 0;
        require(
            unsafe { IsProcessInJob(raw(h), raw(&self.h), &mut value) },
            "IsProcessInJob",
        );
        value != 0
    }
    fn account(&self) -> Accounting {
        let mut a = Accounting::default();
        require(
            unsafe {
                QueryInformationJobObject(
                    raw(&self.h),
                    1,
                    (&mut a as *mut Accounting).cast(),
                    mem::size_of::<Accounting>() as u32,
                    ptr::null_mut(),
                )
            },
            "query accounting",
        );
        a
    }
    fn arm(&self, additional_100ns: i64) {
        // Explicit terminate action; the signal must never be reset afterward.
        let action = 0u32;
        require(
            unsafe {
                SetInformationJobObject(
                    raw(&self.h),
                    6,
                    (&action as *const u32).cast(),
                    mem::size_of::<u32>() as u32,
                )
            },
            "set terminate action",
        );
        let mut limits = ExtendedLimit::default();
        require(
            unsafe {
                QueryInformationJobObject(
                    raw(&self.h),
                    9,
                    (&mut limits as *mut ExtendedLimit).cast(),
                    mem::size_of::<ExtendedLimit>() as u32,
                    ptr::null_mut(),
                )
            },
            "query limits",
        );
        assert_eq!(
            limits.basic.flags & (0x800 | 0x1000),
            0,
            "breakaway is enabled"
        );
        limits.basic.flags |= JOB_OBJECT_LIMIT_JOB_TIME;
        limits.basic.job_time = additional_100ns;
        require(
            unsafe {
                SetInformationJobObject(
                    raw(&self.h),
                    9,
                    (&limits as *const ExtendedLimit).cast(),
                    mem::size_of::<ExtendedLimit>() as u32,
                )
            },
            "set job user time",
        );
    }
    fn signaled(&self) {
        let w = wait(&self.h, 10_000);
        assert_eq!(w, WAIT_OBJECT_0, "UNVERIFIED: job wait={w}");
    }
}
struct Proc {
    h: OwnedHandle,
    t: OwnedHandle,
    pid: u32,
}
impl Proc {
    fn resume(&self) {
        assert_ne!(
            unsafe { ResumeThread(raw(&self.t)) },
            u32::MAX,
            "ResumeThread: {}",
            io::Error::last_os_error()
        );
    }
}
impl Drop for Proc {
    fn drop(&mut self) {
        if wait(&self.h, 0) == WAIT_TIMEOUT {
            unsafe {
                TerminateProcess(raw(&self.h), 199);
            }
            let _ = wait(&self.h, 5000);
        }
    }
}
fn suspended(args: &[&str]) -> Proc {
    let exe = std::env::current_exe().unwrap();
    let app: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let mut cmd = format!("\"{}\"", exe.display());
    for arg in args {
        assert!(!arg.contains('"'));
        cmd.push_str(&format!(" \"{arg}\""));
    }
    let mut command: Vec<u16> = cmd.encode_utf16().chain([0]).collect();
    let mut start = Startup::default();
    start.cb = mem::size_of::<Startup>() as u32;
    let mut pi = ProcessInfo::default();
    require(
        unsafe {
            CreateProcessW(
                app.as_ptr(),
                command.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                CREATE_SUSPENDED | CREATE_NO_WINDOW,
                ptr::null(),
                ptr::null(),
                &start,
                &mut pi,
            )
        },
        "CreateProcessW suspended",
    );
    Proc {
        h: unsafe { OwnedHandle::from_raw_handle(pi.process) },
        t: unsafe { OwnedHandle::from_raw_handle(pi.thread) },
        pid: pi.pid,
    }
}
fn assigned(job: &Job, args: &[&str]) -> Proc {
    let p = suspended(args);
    job.assign(&p).expect("assign suspended process");
    assert!(job.member(&p.h));
    p.resume();
    p
}
fn sentinel(job: &Job) -> Proc {
    assigned(job, &["--child", "cpu"])
}
fn read_handles(path: &Path, seen: &mut HashSet<usize>, out: &mut Vec<OwnedHandle>) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for line in text.split_inclusive('\n').filter(|l| l.ends_with('\n')) {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("H:") {
            let value: usize = value.parse().expect("bad duplicated handle record");
            if seen.insert(value) {
                // DuplicateHandle targeted this process, and each record is written once.
                out.push(unsafe { OwnedHandle::from_raw_handle(value as Handle) });
            }
        } else if line.starts_with("ERROR:") {
            panic!("spawner: {line}");
        }
    }
}
fn wait_handles(
    path: &Path,
    expected: usize,
    seen: &mut HashSet<usize>,
    out: &mut Vec<OwnedHandle>,
) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        read_handles(path, seen, out);
        if out.len() >= expected {
            return;
        }
        assert!(
            Instant::now() < until,
            "timed out waiting for duplicated child handles"
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn file(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lingxi-job-probe-{}-{name}", std::process::id()))
}
fn as_arg(p: &Path) -> String {
    p.to_str().expect("unicode temporary path").to_owned()
}
fn child(mode: &str, args: &[String]) -> ! {
    match mode {
        "cpu" => {
            let mut n = 0u64;
            loop {
                n = std::hint::black_box(n.wrapping_add(1));
            }
        }
        "idle" => loop {
            thread::sleep(Duration::from_secs(1));
        },
        "natural" => {
            thread::sleep(Duration::from_millis(20));
            std::process::exit(0);
        }
        "spawner" => {
            let path = &args[0];
            let owner_pid: u32 = args[1].parse().unwrap();
            let count: usize = args[2].parse().unwrap();
            let child_mode = &args[3];
            let owner_raw = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, owner_pid) };
            if owner_raw.is_null() {
                fs::write(
                    path,
                    format!("ERROR: OpenProcess {}\n", io::Error::last_os_error()),
                )
                .unwrap();
                std::process::exit(2);
            }
            let owner = unsafe { OwnedHandle::from_raw_handle(owner_raw) };
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            for _ in 0..count {
                let spawned = Command::new(std::env::current_exe().unwrap())
                    .args(["--child", child_mode])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                let p = match spawned {
                    Ok(p) => p,
                    Err(e) => {
                        writeln!(f, "SPAWN_END:{e}").unwrap();
                        f.flush().unwrap();
                        break;
                    }
                };
                let mut duplicated = ptr::null_mut();
                if unsafe {
                    DuplicateHandle(
                        GetCurrentProcess(),
                        p.as_raw_handle(),
                        raw(&owner),
                        &mut duplicated,
                        0,
                        0,
                        DUPLICATE_SAME_ACCESS,
                    )
                } == 0
                {
                    writeln!(f, "ERROR: DuplicateHandle {}", io::Error::last_os_error()).unwrap();
                    f.flush().unwrap();
                    std::process::exit(2);
                }
                writeln!(f, "H:{}", duplicated as usize).unwrap();
                f.flush().unwrap();
                drop(p);
                thread::sleep(Duration::from_millis(1));
            }
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        "breakaway" => {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--child", "idle"])
                .creation_flags(CREATE_BREAKAWAY_FROM_JOB)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let result = match command.spawn() {
                Ok(mut p) => {
                    let _ = p.kill();
                    let _ = p.wait();
                    "UNEXPECTED_SUCCESS".to_owned()
                }
                Err(e) => format!("DENIED:{}:{e}", e.raw_os_error().unwrap_or_default()),
            };
            fs::write(&args[0], result).unwrap();
            loop {
                thread::sleep(Duration::from_secs(1));
            }
        }
        _ => panic!("unknown child mode"),
    }
}
fn read_file_wait(path: &Path) -> String {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = fs::read_to_string(path) {
            if !s.is_empty() {
                return s;
            }
        }
        assert!(
            Instant::now() < until,
            "file ready timeout: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(2));
    }
}
fn negative_controls() {
    println!("CASE negative_controls");
    let j = Job::new();
    let p = assigned(&j, &["--child", "idle"]);
    assert_live(&p.h, "idle");
    let action = 0u32;
    assert_eq!(
        unsafe { SetInformationJobObject(raw(&j.h), 6, (&action as *const u32).cast(), 0) },
        0,
        "invalid info length unexpectedly accepted"
    );
    println!("invalid_set_error={}", io::Error::last_os_error());
    j.arm(100_000_000); // 10 seconds of extra user CPU, without a busy sentinel.
    assert_eq!(
        wait(&j.h, 250),
        WAIT_TIMEOUT,
        "untriggered time limit must be unverified"
    );
    assert_live(&p.h, "idle under untriggered limit");
    require(
        unsafe { TerminateJobObject(raw(&j.h), 143) },
        "TerminateJobObject",
    );
    assert_eq!(
        wait(&p.h, 5000),
        WAIT_OBJECT_0,
        "TerminateJobObject process did not exit"
    );
    assert_eq!(
        wait(&j.h, 250),
        WAIT_TIMEOUT,
        "ordinary TerminateJobObject unexpectedly signaled the job on this host"
    );
    println!("RESULT negative_controls PASS untriggered=unverified ordinary_terminate_job_signal=timeout");
}
fn direct_and_descendant_race() {
    println!("CASE direct_and_descendant_race");
    let path = file("race");
    let _ = fs::remove_file(&path);
    let path_arg = as_arg(&path);
    let owner = std::process::id().to_string();
    let j = Job::new();
    let cpu = sentinel(&j);
    let spawner = assigned(
        &j,
        &["--child", "spawner", &path_arg, &owner, "200", "idle"],
    );
    let mut seen = HashSet::new();
    let mut children = Vec::new();
    wait_handles(&path, 4, &mut seen, &mut children);
    for h in &children {
        assert!(j.member(h));
        assert_live(h, "prearmed grandchild");
    }
    let before = j.account();
    println!(
        "before_arm active={} total={} known_grandchildren={} user_time={}",
        before.active,
        before.total,
        children.len(),
        before.total_user
    );
    j.arm(500_000); // 50 ms additional user CPU; let the grandchild creator race the limit.
    let armed = j.account();
    j.signaled();
    read_handles(&path, &mut seen, &mut children);
    assert_ended(&cpu.h, "sentinel");
    assert_ended(&spawner.h, "spawner");
    for (n, h) in children.iter().enumerate() {
        assert!(j.member(h), "observed grandchild escaped the job");
        assert_ended(h, &format!("grandchild-{n}"));
    }
    let after = j.account();
    println!("after_signal active={} total={} known_grandchildren={} coverage={} new_assignments_after_arm={}", after.active, after.total, children.len(), if after.total as usize == children.len()+2 { "all-observed" } else { "incomplete-possible-killed-before-handle-record" }, after.total.saturating_sub(armed.total));
    assert_eq!(wait(&j.h, 0), WAIT_OBJECT_0, "job signal did not persist");
    let late = suspended(&["--child", "idle"]);
    let late_pid = late.pid;
    let result = j.assign(&late);
    assert!(
        result.is_err(),
        "post-signal assignment unexpectedly succeeded for pid={late_pid}"
    );
    println!("late_assignment=denied reason={}", result.unwrap_err());
    let _ = fs::remove_file(path);
    println!("RESULT direct_and_descendant_race PASS observed_handles={} all_observed={} actual_creation_overlap={}", children.len()+2, after.total as usize == children.len()+2, if after.total > armed.total { "observed" } else { "inconclusive" });
}
fn breakaway_denied() {
    println!("CASE breakaway_denied");
    let path = file("breakaway");
    let _ = fs::remove_file(&path);
    let path_arg = as_arg(&path);
    let j = Job::new();
    let cpu = sentinel(&j);
    let worker = assigned(&j, &["--child", "breakaway", &path_arg]);
    let result = read_file_wait(&path);
    println!("breakaway_result={result}");
    assert!(
        result.starts_with("DENIED:5:"),
        "unexpected breakaway result"
    );
    j.arm(10_000);
    j.signaled();
    assert_ended(&cpu.h, "sentinel");
    assert_ended(&worker.h, "breakaway-worker");
    let _ = fs::remove_file(path);
    println!("RESULT breakaway_denied PASS scope=CreateProcess-only");
}
fn earlier_terminations() {
    println!("CASE earlier_terminations");
    let mut gaps = 0;
    let mut gap_still_unsignaled_after_arm = 0;
    for mode in ["natural", "terminate"] {
        for iteration in 0..20 {
            let j = Job::new();
            let cpu = sentinel(&j);
            let p = assigned(
                &j,
                &[
                    "--child",
                    if mode == "natural" { "natural" } else { "idle" },
                ],
            );
            if mode == "terminate" {
                require(
                    unsafe { TerminateProcess(raw(&p.h), 77) },
                    "terminate prior member",
                );
            }
            let until = Instant::now() + Duration::from_secs(2);
            let mut gap = false;
            loop {
                assert_live(&cpu.h, "sentinel before historical-gap observation");
                let a = j.account();
                let w = wait(&p.h, 0);
                if a.active == 1 && w == WAIT_TIMEOUT {
                    gap = true;
                    break;
                }
                if w == WAIT_OBJECT_0 {
                    break;
                }
                assert!(Instant::now() < until, "prior member did not start exiting");
                thread::yield_now();
            }
            if gap {
                gaps += 1;
            }
            j.arm(10_000);
            let prior_after_arm = wait(&p.h, 0);
            if gap && prior_after_arm == WAIT_TIMEOUT {
                gap_still_unsignaled_after_arm += 1;
            }
            j.signaled();
            let after = wait(&p.h, 0);
            if after != WAIT_OBJECT_0 {
                panic!("COUNTEREXAMPLE mode={mode} iteration={iteration} prior_gap={gap} prior_after_arm={prior_after_arm} after_job_signal={after}");
            }
            assert_ended(&cpu.h, "sentinel");
            println!("prior mode={mode} iteration={iteration} gap={gap} after_arm={prior_after_arm} after_job_signal={after}");
        }
    }
    println!("RESULT earlier_terminations PASS iterations=40 gap_observed={gaps} gap_still_unsignaled_after_arm={gap_still_unsignaled_after_arm} history_proof={}", if gaps == 0 { "inconclusive-no-active-count-gap-observed" } else { "empirical-only" });
}
fn earlier_descendant() {
    println!("CASE earlier_descendant");
    let mut unsignaled_after_arm = 0;
    for iteration in 0..12 {
        let path = file(&format!("earlier-grandchild-{iteration}"));
        let _ = fs::remove_file(&path);
        let path_arg = as_arg(&path);
        let owner = std::process::id().to_string();
        let j = Job::new();
        let cpu = sentinel(&j);
        let parent = assigned(&j, &["--child", "spawner", &path_arg, &owner, "1", "idle"]);
        let mut seen = HashSet::new();
        let mut descendants = Vec::new();
        wait_handles(&path, 1, &mut seen, &mut descendants);
        let target = &descendants[0];
        assert!(j.member(target));
        assert_live(target, "grandchild before termination");
        require(
            unsafe { TerminateProcess(raw(target), 78) },
            "terminate prior grandchild",
        );
        j.arm(10_000);
        let pre = wait(target, 0);
        if pre == WAIT_TIMEOUT {
            unsignaled_after_arm += 1;
        }
        j.signaled();
        let after = wait(target, 0);
        if after != WAIT_OBJECT_0 {
            panic!("COUNTEREXAMPLE prior grandchild iteration={iteration} after_arm={pre} after_job_signal={after}");
        }
        assert_ended(&cpu.h, "sentinel");
        assert_ended(&parent.h, "parent");
        println!("prior_grandchild iteration={iteration} after_arm={pre} after_job_signal={after}");
        let _ = fs::remove_file(path);
    }
    println!("RESULT earlier_descendant PASS iterations=12 prior_unsignaled_after_arm={unsignaled_after_arm} historical_scope=empirical-only");
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--child") {
        child(&args[1], &args[2..]);
    }
    println!(
        "windows-job-time-probe version=1 pid={}",
        std::process::id()
    );
    let cases: [(&str, fn()); 5] = [
        ("negative_controls", negative_controls),
        ("direct_and_descendant_race", direct_and_descendant_race),
        ("breakaway_denied", breakaway_denied),
        ("earlier_terminations", earlier_terminations),
        ("earlier_descendant", earlier_descendant),
    ];
    let mut failures = 0;
    for (name, run) in cases {
        if std::panic::catch_unwind(run).is_err() {
            failures += 1;
            println!("RESULT {name} FAIL");
        }
    }
    if failures != 0 {
        println!("RESULT OVERALL FAIL failures={failures}");
        std::process::exit(1);
    }
    println!("RESULT OVERALL PASS; signal historical-scope and unobserved descendants require documented contract; timeout is unverified");
}
