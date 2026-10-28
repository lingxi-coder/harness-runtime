//! Isolated diagnostic only: probe Windows end-of-job-time signaling.
#![cfg(windows)]
use std::ffi::{c_void, OsStr};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Handle = *mut c_void;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const ERROR_NOT_ENOUGH_QUOTA: u32 = 1816;
const STATUS_QUOTA_EXCEEDED: u32 = 0xc000_0044;
const CREATE_SUSPENDED: u32 = 0x4;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    process_time: i64,
    job_time: i64,
    flags: u32,
    min_working_set: usize,
    max_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}
#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}
#[allow(dead_code)] // Fields are consumed by the Win32 ABI.
#[repr(C)]
struct StartupInfo {
    cb: u32,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_chars: u32,
    y_chars: u32,
    fill: u32,
    flags: u32,
    show: u16,
    reserved_len: u16,
    reserved_ptr: *mut u8,
    stdin: Handle,
    stdout: Handle,
    stderr: Handle,
}
#[allow(dead_code)] // Fields are filled by the Win32 ABI.
#[repr(C)]
struct ProcessInfo {
    process: Handle,
    thread: Handle,
    pid: u32,
    thread_id: u32,
}
#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: u32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, code: u32) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn CreateProcessW(
        application: *const u16,
        command: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit: i32,
        flags: u32,
        environment: *const c_void,
        current_directory: *const u16,
        startup: *const StartupInfo,
        information: *mut ProcessInfo,
    ) -> i32;
    fn ResumeThread(thread: Handle) -> u32;
    fn TerminateProcess(process: Handle, code: u32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn WaitForSingleObject(handle: Handle, millis: u32) -> u32;
    fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
}
fn fail(context: &str) -> ! {
    panic!("{context}: {}", std::io::Error::last_os_error());
}
fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}
fn job() -> OwnedHandle {
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        fail("CreateJobObjectW");
    }
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = ExtendedLimits::default();
    limits.basic.flags = 0x2000; // KILL_ON_JOB_CLOSE, no breakaway
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            9,
            (&limits as *const ExtendedLimits).cast(),
            std::mem::size_of::<ExtendedLimits>() as u32,
        )
    } == 0
    {
        fail("set kill-on-close");
    }
    job
}
fn set_job_time(job: &OwnedHandle) {
    let action: u32 = 0; // JOB_OBJECT_TERMINATE_AT_END_OF_JOB
    if unsafe { SetInformationJobObject(job.as_raw_handle(), 6, (&action as *const u32).cast(), 4) }
        == 0
    {
        fail("set end-of-job action");
    }
    let mut limits = ExtendedLimits::default();
    limits.basic.flags = 0x2000 | 0x4; // kill on close + JOB_OBJECT_LIMIT_JOB_TIME
    limits.basic.job_time = 1; // 100 ns of user time after setting the limit
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            9,
            (&limits as *const ExtendedLimits).cast(),
            std::mem::size_of::<ExtendedLimits>() as u32,
        )
    } == 0
    {
        fail("set end-of-job time limit");
    }
}
fn create_suspended(role: &str, directory: &Path, mode: &str) -> (OwnedHandle, OwnedHandle, u32) {
    let exe = std::env::current_exe().unwrap();
    let application = wide(exe.as_os_str());
    let command_line = format!(
        "\"{}\" {} \"{}\" {}",
        exe.display(),
        role,
        directory.display(),
        mode
    );
    let mut command = wide(OsStr::new(&command_line));
    let mut startup: StartupInfo = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<StartupInfo>() as u32;
    let mut info: ProcessInfo = unsafe { std::mem::zeroed() };
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_SUSPENDED | CREATE_NO_WINDOW,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut info,
        )
    } == 0
    {
        fail("CreateProcessW");
    }
    (
        unsafe { OwnedHandle::from_raw_handle(info.process) },
        unsafe { OwnedHandle::from_raw_handle(info.thread) },
        info.pid,
    )
}
fn spawn_member(job: &OwnedHandle, role: &str, directory: &Path, mode: &str) -> (OwnedHandle, u32) {
    let (process, thread, pid) = create_suspended(role, directory, mode);
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            TerminateProcess(process.as_raw_handle(), 0xdead);
            WaitForSingleObject(process.as_raw_handle(), 5000);
        }
        panic!("assign suspended {role}: {error}");
    }
    let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
    if previous != 1 {
        panic!(
            "resume {role}: previous={previous}, error={}",
            std::io::Error::last_os_error()
        );
    }
    drop(thread);
    (process, pid)
}
fn held_process(pid: u32) -> OwnedHandle {
    let raw = unsafe { OpenProcess(0x0010_0000 | 0x1000, 0, pid) };
    if raw.is_null() {
        fail("OpenProcess");
    }
    unsafe { OwnedHandle::from_raw_handle(raw) }
}
fn wait(handle: &OwnedHandle, millis: u32) -> u32 {
    unsafe { WaitForSingleObject(handle.as_raw_handle(), millis) }
}
fn member(job: &OwnedHandle, process: &OwnedHandle) -> bool {
    let mut result = 0;
    if unsafe { IsProcessInJob(process.as_raw_handle(), job.as_raw_handle(), &mut result) } == 0 {
        fail("IsProcessInJob");
    }
    result != 0
}
fn exit_code(process: &OwnedHandle) -> u32 {
    let mut result = 0;
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut result) } == 0 {
        fail("GetExitCodeProcess");
    }
    result
}
fn require_expected_exit(process: &OwnedHandle, description: &str, terminated_first: bool) {
    let code = exit_code(process);
    assert!(
        code == ERROR_NOT_ENOUGH_QUOTA
            || code == STATUS_QUOTA_EXCEEDED
            || (terminated_first && code == 143),
        "{description} did not receive quota termination: {code:#x}"
    );
}
fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "marker timeout: {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn child(directory: &Path, index: &str) {
    let pending = directory.join(format!("child-{index}.pending"));
    std::fs::write(&pending, std::process::id().to_string()).unwrap();
    std::fs::rename(pending, directory.join(format!("child-{index}"))).unwrap();
    std::thread::sleep(Duration::from_secs(60));
}
fn leader(directory: &Path, mode: &str) {
    for index in 0..3 {
        Command::new(std::env::current_exe().unwrap())
            .arg("child")
            .arg(directory)
            .arg(index.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
    }
    for index in 0..3 {
        wait_file(&directory.join(format!("child-{index}")));
    }
    std::fs::write(directory.join("leader-ready"), "ready").unwrap();
    if mode == "live" {
        std::thread::sleep(Duration::from_secs(60));
    }
}
fn pulse() -> ! {
    let mut value: u64 = 1;
    loop {
        value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
    }
}
fn probe(round: u32, root: &Path) {
    let directory = root.join(format!("round-{round}"));
    std::fs::create_dir(&directory).unwrap();
    let job = job();
    let mode = if round % 2 == 0 { "live" } else { "exit" };
    let no_time_limit = round < 32;
    let terminated_first = !no_time_limit && round % 4 >= 2;
    let (leader, leader_pid) = spawn_member(&job, "leader", &directory, mode);
    wait_file(&directory.join("leader-ready"));
    let mut children = Vec::new();
    for index in 0..3 {
        let marker = directory.join(format!("child-{index}"));
        wait_file(&marker);
        let pid = std::fs::read_to_string(&marker)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let held = held_process(pid);
        assert!(member(&job, &held), "child is outside tested Job: {pid}");
        assert_eq!(wait(&held, 0), WAIT_TIMEOUT, "child exited before stop");
        children.push((pid, held));
    }
    if mode == "exit" {
        assert_eq!(wait(&leader, 5000), WAIT_OBJECT_0, "leader did not exit");
    } else {
        assert_eq!(wait(&leader, 0), WAIT_TIMEOUT, "leader is not alive");
    }
    assert_eq!(
        wait(&job, 0),
        WAIT_TIMEOUT,
        "Job already signaled before limit"
    );
    if no_time_limit {
        if unsafe { TerminateJobObject(job.as_raw_handle(), 143) } == 0 {
            fail("TerminateJobObject without time limit");
        }
        let job_state = wait(&job, 0);
        assert!(
            matches!(job_state, WAIT_OBJECT_0 | WAIT_TIMEOUT),
            "invalid Job wait: {job_state}"
        );
        let mut unsignaled = 0usize;
        for (_, held) in &children {
            match wait(held, 0) {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => unsignaled += 1,
                state => panic!("invalid descendant wait: {state}"),
            }
        }
        println!("jobtime-negative round={round} mode={mode} job_wait={job_state} unsignaled_children={unsignaled}");
        assert!(
            job_state != WAIT_OBJECT_0 || unsignaled == 0,
            "ordinary termination signaled the Job while a retained child remained live"
        );
        for (_, held) in &children {
            assert_eq!(wait(held, 15000), WAIT_OBJECT_0, "held child did not exit");
        }
        assert_eq!(
            wait(&leader, 15000),
            WAIT_OBJECT_0,
            "held leader did not exit"
        );
        std::fs::remove_dir_all(&directory).unwrap();
        return;
    }
    let (burner, burner_thread, burner_pid) = create_suspended("pulse", &directory, "unused");
    if !terminated_first
        && unsafe { AssignProcessToJobObject(job.as_raw_handle(), burner.as_raw_handle()) } == 0
    {
        let error = std::io::Error::last_os_error();
        unsafe {
            TerminateProcess(burner.as_raw_handle(), 0xdead);
            WaitForSingleObject(burner.as_raw_handle(), 5000);
        }
        panic!("assign suspended pulse: {error}");
    }
    set_job_time(&job);
    let mut burner_is_member = true;
    if terminated_first {
        // Stop current payload members immediately, then start a trusted pulse
        // only to drive the documented Job time-limit signal. The pulse stays
        // suspended until it is safely assigned to this exact Job.
        if unsafe { TerminateJobObject(job.as_raw_handle(), 143) } == 0 {
            fail("TerminateJobObject");
        }
        let job_state = wait(&job, 0);
        assert!(
            matches!(job_state, WAIT_OBJECT_0 | WAIT_TIMEOUT),
            "invalid Job wait: {job_state}"
        );
        let unsignaled = children
            .iter()
            .filter(|(_, held)| wait(held, 0) == WAIT_TIMEOUT)
            .count();
        println!("jobtime-after-terminate round={round} job_wait={job_state} unsignaled_children={unsignaled}");
        assert!(
            job_state != WAIT_OBJECT_0 || unsignaled == 0,
            "Job signaled immediately after TerminateJobObject while a retained child was live"
        );
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), burner.as_raw_handle()) } == 0 {
            let error = std::io::Error::last_os_error();
            assert_eq!(
                wait(&job, 15000),
                WAIT_OBJECT_0,
                "pulse assignment failed without a signaled Job: {error}"
            );
            unsafe {
                TerminateProcess(burner.as_raw_handle(), 0xdead);
            }
            assert_eq!(wait(&burner, 5000), WAIT_OBJECT_0);
            burner_is_member = false;
        }
    }
    if burner_is_member {
        let previous = unsafe { ResumeThread(burner_thread.as_raw_handle()) };
        if previous != 1 {
            let error = std::io::Error::last_os_error();
            assert!(
                previous == u32::MAX
                    && wait(&job, 15000) == WAIT_OBJECT_0
                    && wait(&burner, 0) == WAIT_OBJECT_0,
                "resume suspended pulse: previous={previous}, error={error}"
            );
        }
    }
    drop(burner_thread);
    assert_eq!(
        wait(&job, 15000),
        WAIT_OBJECT_0,
        "job time barrier never signaled"
    );
    assert_eq!(
        wait(&leader, 0),
        WAIT_OBJECT_0,
        "Job signaled while retained leader handle unsignaled"
    );
    for (pid, held) in &children {
        assert_eq!(
            wait(held, 0),
            WAIT_OBJECT_0,
            "Job signaled while retained descendant {pid} is unsignaled"
        );
        require_expected_exit(held, &format!("descendant {pid}"), terminated_first);
    }
    assert_eq!(
        wait(&burner, 0),
        WAIT_OBJECT_0,
        "Job signaled while pulse handle unsignaled"
    );
    if burner_is_member {
        require_expected_exit(&burner, "pulse", terminated_first);
    }
    let (extra, extra_thread, _) = create_suspended("pulse", &directory, "unused");
    let assigned = unsafe { AssignProcessToJobObject(job.as_raw_handle(), extra.as_raw_handle()) };
    if assigned == 0 {
        unsafe {
            TerminateProcess(extra.as_raw_handle(), 0xdead);
        }
        assert_eq!(
            wait(&extra, 5000),
            WAIT_OBJECT_0,
            "unassigned suspended probe did not terminate"
        );
    }
    drop(extra_thread);
    assert_eq!(assigned, 0, "job accepted another process after signaling");
    println!("jobtime-probe round={round} mode={mode} terminated_first={terminated_first} pulse_member={burner_is_member} leader={leader_pid} burner={burner_pid} signal=job+all-held-handles sealed=true");
    std::fs::remove_dir_all(&directory).unwrap();
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => child(Path::new(&args[2]), &args[3]),
        Some("leader") => leader(Path::new(&args[2]), &args[3]),
        Some("pulse") => pulse(),
        None => {
            let root: PathBuf =
                std::env::temp_dir().join(format!("lingxi-jobtime-probe-{}", std::process::id()));
            std::fs::create_dir(&root).unwrap();
            for round in 0..48 {
                probe(round, &root);
            }
            std::fs::remove_dir(&root).unwrap();
            println!("jobtime-probe all-rounds-passed=48");
        }
        other => panic!("unexpected role: {other:?}"),
    }
}
