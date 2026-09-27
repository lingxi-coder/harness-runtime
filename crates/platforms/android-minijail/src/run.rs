//! `run_jailed`: fork + jail + exec one command, capture output, enforce the
//! timeout by killing the process group, reap. The only fork+exec path in the
//! engine (spec r3 D6: in-engine `minijail_run_*`). All unsafe FFI is here.
//!
//! Host builds cannot jail, so `run_jailed` reports an enforcement failure and
//! callers stay fail-closed. The Android body is `#[cfg(target_os = "android")]`
//! and is verified by the `arm64`/`x86_64` cross-build plus the on-device
//! instrumentation gate — it cannot execute on the macOS host.

use crate::{JailSpec, JailedOutput};

/// Run `spec` to completion under Minijail. Host builds cannot jail — they
/// report enforcement failure so callers stay fail-closed.
#[must_use]
pub fn run_jailed(spec: &JailSpec) -> JailedOutput {
    #[cfg(not(target_os = "android"))]
    {
        let _ = spec;
        JailedOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            timed_out: false,
            enforcement_failed: Some("jailed execution requires an Android device".into()),
        }
    }
    #[cfg(target_os = "android")]
    {
        android_impl::run(spec)
    }
}

/// Decode the int returned by `minijail_wait` into a `JailedOutput.exit_code`.
///
/// `minijail_wait` does NOT hand back a raw `waitpid(2)` status — it pre-collapses
/// the wait status into a single int (see `libminijail.c::minijail_wait_internal`,
/// vendored at `third_party/minijail/libminijail.c`):
///   * a normally-exited child (`WIFEXITED`) → `WEXITSTATUS` (the true 0..=255 exit
///     code), so `exit 0`→0, `exit 1`→1, `exit 137`→137, `exit 200`→200;
///   * a signal-killed child (`WIFSIGNALED`) → `MINIJAIL_ERR_SIG_BASE + signum`
///     = `128 + n` (bash's `$? = 128 + signum` convention), with `SIGSYS`/seccomp
///     mapped to `MINIJAIL_ERR_SECCOMP_VIOLATION` (253);
///   * an internal failure (e.g. `waitpid` error / `-ECHILD`) → a NEGATIVE value.
///
/// We therefore return the low byte (`status & 0xFF`) for any non-negative status
/// and reserve `-1` strictly for the negative internal-error case — mirroring the
/// desktop `PosixProcess` semantics (`output.status.code().unwrap_or(-1)`) for
/// normally-exited children, which never collapse a real exit code into -1.
///
/// AMBIGUITY — exit codes `>= 128` are NOT perfectly distinguishable from signals.
/// Because minijail folds the signal case into the SAME `128 + n` band, a
/// voluntary `exit 137` and a `SIGKILL`-killed child both surface as `137`; there
/// is no information left in `minijail_wait`'s return value to tell them apart.
/// The old behaviour treated the whole `>= 128` band as "signalled → -1", which
/// silently corrupted legitimate high exit codes (`exit 137`/`exit 200` → -1).
/// We now preserve the value: `is_error` (`exit_code != 0`) is intact, and the
/// dominant kill cause — the watchdog timeout — is already carried INDEPENDENTLY
/// by the `timed_out` flag (set by the call site BEFORE calling `decode_exit`).
/// Callers MUST consult `timed_out`, NOT a `-1` exit code, to detect a kill.
///
/// CAVEAT — 126/127 are NOT guaranteed child exit codes. `minijail_wait` returns
/// `MINIJAIL_ERR_NO_ACCESS` (126) / `MINIJAIL_ERR_NO_COMMAND` (127) when minijail
/// itself could not `exec` the target (e.g. not executable / not found), so a
/// 126/127 here may be a minijail exec-failure code rather than something the
/// child returned. We surface them as-is: they coincide with bash's own 126
/// ("cannot execute") / 127 ("command not found") conventions, so the value is
/// meaningful to callers either way and needs no special-casing.
///
/// This is a pure mapping over an int (the `c_int` from the FFI is `i32`), so it
/// lives at file scope — outside the android-only FFI module — and is unit-tested
/// on the host. On non-android hosts the only caller is the FFI runner (cfg'd
/// out), so the lib build legitimately sees it as unused; the host TESTS exercise
/// it directly.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn decode_exit(status: i32) -> i32 {
    if status < 0 {
        // minijail internal error (e.g. wait failed / -ECHILD) — never a child
        // exit code.
        -1
    } else {
        // Normally-exited children carry their true exit code (incl. >= 128 like
        // `exit 137`); signal-killed children share the 128+n band but the
        // watchdog kill is reported via `timed_out`, not here.
        status & 0xFF
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    // LINK ANCHOR — do not remove. `platform-android-libcap` is a build-only
    // crate whose rlib BUNDLES the static libcap objects that resolve
    // libminijail's `cap_*` references. rustc only links crates that are
    // actually referenced, and a `-shared` (cdylib) link does not error on the
    // resulting undefined symbols — without this reference the `.so` builds
    // fine but `dlopen` fails on-device with `cannot locate symbol
    // "cap_get_proc"`. The smoke path in lib.rs has its own anchor; keep this
    // one too so the run module stays self-sufficient if linking changes.
    use platform_android_libcap as _;

    use crate::{JailSpec, JailedOutput};
    use std::ffi::CString;
    use std::io::Read;
    use std::os::raw::{c_char, c_int, c_void};
    use std::os::unix::io::{FromRawFd, RawFd};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Opaque `struct minijail` handle from `libminijail.h`.
    #[repr(C)]
    struct RawMinijail {
        _opaque: [u8; 0],
    }

    /// `MINIJAIL_HOOK_EVENT_PRE_EXECVE` — the hook runs just before `execve(2)`.
    /// `minijail_hook_event_t` is a plain (unannotated) C enum in
    /// `libminijail.h`, so its members number from 0 in declaration order:
    /// `PRE_DROP_CAPS=0`, `PRE_EXECVE=1`, `PRE_CHROOT=2`, `MAX=3`. Verified
    /// against the vendored header (`third_party/minijail/libminijail.h:89-101`).
    const MINIJAIL_HOOK_EVENT_PRE_EXECVE: c_int = 1;

    // Hand-written bindings for the exact `libminijail.h` prototypes the runner
    // needs (libminijail.a is built and linked by build.rs). Each prototype is
    // reproduced from the vendored header so reviewers can diff it 1:1:
    //   struct minijail *minijail_new(void);
    //   void minijail_no_new_privs(struct minijail *j);
    //   int  minijail_rlimit(struct minijail *j, int type,
    //                        rlim_t cur, rlim_t max);            // :277
    //   void minijail_use_seccomp_filter(struct minijail *j);    // :129
    //   void minijail_set_seccomp_filter_tsync(struct minijail *j); // :131
    //   void minijail_set_seccomp_filters(struct minijail *j,
    //            const struct sock_fprog *filter);               // :168
    //   int  minijail_create_session(struct minijail *j);        // :343
    //   int  minijail_add_hook(struct minijail *j, minijail_hook_t hook,
    //            void *payload, minijail_hook_event_t event);    // :467
    //       where minijail_hook_t = int (*)(void *context)       // :83
    //   int  minijail_run_env_pid_pipes_no_preload(struct minijail *j,
    //            const char *filename, char *const argv[], char *const envp[],
    //            pid_t *pchild_pid, int *pstdin_fd, int *pstdout_fd,
    //            int *pstderr_fd);                                // :623
    //   int  minijail_wait(struct minijail *j);
    //   void minijail_destroy(struct minijail *j);
    // NOTE: `minijail_log_seccomp_filter_failures` is deliberately NOT bound —
    // libminijail.c die()s if it was called together with set_seccomp_filters.
    #[allow(unsafe_code)]
    extern "C" {
        fn minijail_new() -> *mut RawMinijail;
        fn minijail_no_new_privs(j: *mut RawMinijail);
        //   void minijail_close_open_fds(struct minijail *j);       // :235
        fn minijail_close_open_fds(j: *mut RawMinijail);
        fn minijail_rlimit(
            j: *mut RawMinijail,
            r#type: c_int,
            cur: libc::rlim_t,
            max: libc::rlim_t,
        ) -> c_int;
        fn minijail_use_seccomp_filter(j: *mut RawMinijail);
        fn minijail_set_seccomp_filter_tsync(j: *mut RawMinijail);
        fn minijail_set_seccomp_filters(j: *mut RawMinijail, filter: *const libc::sock_fprog);
        fn minijail_create_session(j: *mut RawMinijail) -> c_int;
        fn minijail_add_hook(
            j: *mut RawMinijail,
            hook: extern "C" fn(*mut c_void) -> c_int,
            payload: *mut c_void,
            event: c_int,
        ) -> c_int;
        fn minijail_run_env_pid_pipes_no_preload(
            j: *mut RawMinijail,
            filename: *const c_char,
            argv: *const *mut c_char,
            envp: *const *mut c_char,
            pchild_pid: *mut libc::pid_t,
            pstdin_fd: *mut c_int,
            pstdout_fd: *mut c_int,
            pstderr_fd: *mut c_int,
        ) -> c_int;
        fn minijail_wait(j: *mut RawMinijail) -> c_int;
        fn minijail_destroy(j: *mut RawMinijail);
    }

    /// Owned jail handle: destroys the `struct minijail` on drop so every
    /// early-return path below stays leak-free.
    struct Jail(*mut RawMinijail);

    impl Drop for Jail {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a non-null handle returned by `minijail_new`
            // (checked at construction) and destroyed exactly once (this Drop).
            #[allow(unsafe_code)]
            unsafe {
                minijail_destroy(self.0);
            }
        }
    }

    /// `PRE_EXECVE` hook: `chdir` into the requested cwd inside the jailed child,
    /// just before `execve`. `minijail` has no cwd API, so this is how
    /// `JailSpec::cwd` is applied. `payload` is a `*const c_char` to a NUL-terminated path owned
    /// by the caller (a `CString` kept alive across the whole run — see `run`).
    /// Returns 0 on success or `-errno` so minijail aborts the child if the
    /// directory is gone (fail-closed: never exec in the wrong directory).
    extern "C" fn chdir_hook(payload: *mut c_void) -> c_int {
        if payload.is_null() {
            return -libc::EINVAL;
        }
        // SAFETY: minijail invokes this hook in the forked child just before
        // execve. `payload` is the `cwd` CString pointer we passed to
        // `minijail_add_hook`; that CString is owned by `run`'s stack and
        // outlives the entire jail setup + run, so it is a valid NUL-terminated
        // C string here. `chdir` only reads it.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::chdir(payload.cast::<c_char>()) };
        if rc == 0 {
            0
        } else {
            // errno is positive; report it negated so minijail treats it as a
            // hook failure and aborts the child rather than exec'ing.
            #[allow(unsafe_code)]
            let e = unsafe { *libc::__errno() };
            -e
        }
    }

    /// Drain a captured pipe fd to EOF on its own thread, taking ownership of the
    /// fd (closed when the returned `File` drops inside the thread). Spawned for
    /// BOTH stdout and stderr so a child that fills one pipe buffer while we read
    /// the other cannot deadlock (the classic sequential-read hazard).
    fn spawn_reader(fd: RawFd) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            // SAFETY: `fd` is a freshly returned, owned pipe read-end from
            // `minijail_run_env_pid_pipes_no_preload`. We transfer sole
            // ownership into this `File`; it is closed exactly once when the
            // `File` drops at the end of this thread. No other code touches this
            // fd (the parent never reads/closes it directly).
            #[allow(unsafe_code)]
            let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
            let mut buf = Vec::new();
            // Errors here mean a truncated read (e.g. the child was SIGKILLed by
            // the watchdog); keep whatever we captured.
            let _ = file.read_to_end(&mut buf);
            buf
        })
    }

    fn fail(reason: String) -> JailedOutput {
        JailedOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
            timed_out: false,
            enforcement_failed: Some(reason),
        }
    }

    /// Block until the jailed LEADER `pid` terminates, WITHOUT reaping it.
    ///
    /// Uses `waitid(P_PID, …, WEXITED | WNOWAIT)`: it returns when the leader has
    /// exited but leaves it a reapable zombie for `minijail_wait` to reap last.
    /// Two properties matter:
    /// - It waits on the LEADER, not on pipe EOF, so a backgrounded grandchild
    ///   that inherits and holds the stdout/stderr write end (`sleep 300 & echo
    ///   hi`) cannot pin the call to the full timeout.
    /// - Leaving the leader an un-reaped zombie keeps its pid/pgid RESERVED, so
    ///   the subsequent group-cleanup `kill(-pgid)` can never race a recycled
    ///   pgid (the documented kill/reap hazard is sidestepped entirely).
    ///
    /// Retries on `EINTR`; returns on any other error (e.g. `ECHILD`) so the
    /// caller proceeds to the cleanup kill + `minijail_wait`.
    fn wait_leader_exit(pid: libc::pid_t) {
        // SAFETY: `siginfo_t` is a plain C struct; zeroing it is a valid initial
        // state for `waitid`, which fills it on success.
        #[allow(unsafe_code)]
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        loop {
            // SAFETY: `waitid` only writes `info`; `pid` is the live/zombie child.
            #[allow(unsafe_code)]
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                return;
            }
            // SAFETY: reading the thread-local errno after a failed libc call.
            #[allow(unsafe_code)]
            let e = unsafe { *libc::__errno() };
            if e == libc::EINTR {
                continue;
            }
            return;
        }
    }

    /// Build the jail, fork+exec the command, capture stdio, enforce the timeout
    /// by killing the child's own process group, reap, and return the outcome.
    /// Any setup failure returns `enforcement_failed` — we NEVER run unconfined.
    ///
    /// This is one linear FFI sequence (build → filter → session → hook →
    /// marshal → run → capture → reap) whose steps share many keep-alive
    /// lifetimes (`CString`s, the filter `Vec`) that must all outlive the single run
    /// call; splitting it into helpers would only scatter those lifetime ties and
    /// obscure the ordering that makes the unsafe calls sound — so it stays one
    /// function with a documented length allowance.
    #[allow(unsafe_code, clippy::too_many_lines)]
    pub(crate) fn run(spec: &JailSpec) -> JailedOutput {
        // (1) Construct the jail. Drop guard destroys it on every return path.
        // SAFETY: plain constructor; NULL is checked immediately.
        let raw = unsafe { minijail_new() };
        if raw.is_null() {
            return fail("minijail_new returned NULL".into());
        }
        let jail = Jail(raw);

        // (2) no_new_privs — mandatory for an unprivileged seccomp filter.
        // SAFETY: `jail.0` is valid; the setter has no other preconditions.
        unsafe { minijail_no_new_privs(jail.0) };

        // (2b) Close every inherited fd in the jailed child except stdin/stdout/
        // stderr (the pipe fds minijail itself manages). Without this, libminijail
        // leaves ALL inherited fds open in the child (it only dup2's 0/1/2), so the
        // engine's non-CLOEXEC native fds (JNI/ART, libgit2, audio, C libs) would
        // be reachable from attacker-supplied shell commands — and the net-deny
        // seccomp filter blocks socket()/connect() but NOT read()/write() on an
        // already-open fd, so a leaked live connection or secret file fd would
        // bypass confinement. Requesting close_open_fds sets the libminijail flag
        // that closes them just before execve.
        // SAFETY: `jail.0` is valid; the setter only toggles an internal flag.
        unsafe { minijail_close_open_fds(jail.0) };

        // (3) rlimits. Each resource int comes from the safe side (RLIMIT_*).
        for rl in &spec.rlimits {
            // SAFETY: `jail.0` is valid; the args are plain integers.
            let rc = unsafe {
                minijail_rlimit(
                    jail.0,
                    rl.resource as c_int,
                    rl.soft as libc::rlim_t,
                    rl.hard as libc::rlim_t,
                )
            };
            if rc != 0 {
                return fail(format!("minijail_rlimit(resource={}) -> {rc}", rl.resource));
            }
        }

        // (4) net-deny seccomp filter (raw classic-BPF, allow-by-default,
        // socket-family -> EPERM). We map the host-built `BpfInsn`s to
        // `libc::sock_filter` (identical 4-field layout) and inject the program
        // verbatim via `minijail_set_seccomp_filters` (the header notes it does
        // NOT take ownership of the filter — hence we keep the Vec alive across
        // the run call below). We intentionally do NOT call
        // `minijail_log_seccomp_filter_failures` (libminijail die()s if combined
        // with set_seccomp_filters).
        //
        // `filter_insns`/`filter_prog` MUST outlive the run call (the filter is
        // non-owning — the kernel reads it during the run). They are bound at
        // function scope so they live until `run` returns; both stay empty/NULL
        // when `!net_deny`. `filter_insns` holds the program alive for the whole
        // jailed lifetime even though minijail keeps only the raw pointer.
        let filter_insns: Vec<libc::sock_filter> = if spec.net_deny {
            spec.bpf
                .iter()
                .map(|i| libc::sock_filter {
                    code: i.code,
                    jt: i.jt,
                    jf: i.jf,
                    k: i.k,
                })
                .collect()
        } else {
            Vec::new()
        };
        // `filter_prog` lives at function scope (kept alive across the run call);
        // it points at `filter_insns` only on the net_deny path, NULL otherwise.
        let filter_prog = if spec.net_deny {
            if filter_insns.is_empty() {
                return fail("net_deny set but BPF program is empty".into());
            }
            let Ok(len) = u16::try_from(filter_insns.len()) else {
                return fail(format!("BPF program too long: {}", filter_insns.len()));
            };
            let prog = libc::sock_fprog {
                len,
                // `filter` is a non-owning pointer; `filter_insns` outlives the run.
                filter: filter_insns.as_ptr().cast_mut(),
            };
            // SAFETY: `jail.0` is valid. `set_seccomp_filters` does NOT take
            // ownership of the filter (header) — the kernel reads `prog` and the
            // `filter_insns` it points at during the run, and both remain alive
            // until after the run call returns (bound in this function's scope).
            // We pair it with use_seccomp_filter (selects seccomp-bpf mode) +
            // tsync (apply to all threads — not the log fn, compatible with
            // set_filters).
            unsafe {
                minijail_use_seccomp_filter(jail.0);
                minijail_set_seccomp_filter_tsync(jail.0);
                minijail_set_seccomp_filters(jail.0, &raw const prog);
            }
            prog
        } else {
            libc::sock_fprog {
                len: 0,
                filter: ptr::null_mut(),
            }
        };
        // Hold the program (and thus its backing `filter_insns`) alive across the
        // run call below even though minijail retains only the raw pointer.
        let _ = &filter_prog;

        // (5) Put the child in its own session/process-group so the watchdog can
        // kill(-pgid) WITHOUT touching the engine's own group. The child becomes
        // the leader of a new pgid == its pid.
        // SAFETY: `jail.0` is valid; create_session has no other preconditions.
        let rc = unsafe { minijail_create_session(jail.0) };
        if rc != 0 {
            return fail(format!("minijail_create_session -> {rc}"));
        }

        // (6) PRE_EXECVE chdir hook. `cwd_c` MUST outlive the run call (the hook
        // runs in the child during minijail_run, reading this pointer). Skip the
        // hook entirely for an EMPTY cwd: chdir("") returns ENOENT, which would
        // make the hook abort the child pre-execve — an empty cwd means "run in
        // the inherited working directory", not "fail". (The live Shell path
        // always sets a canonical cwd; this guards the SystemShell/None-cwd path.)
        let Ok(cwd_c) = CString::new(spec.cwd.as_str()) else {
            return fail("cwd contains an interior NUL byte".into());
        };
        if !spec.cwd.is_empty() {
            // SAFETY: `jail.0` is valid; `chdir_hook` is an `extern "C"` fn
            // matching `minijail_hook_t = int (*)(void *)`. `payload` is the
            // `cwd_c` pointer, which lives on this stack frame past the run call,
            // so it is valid when the hook fires inside the child. PRE_EXECVE ==
            // 1 (verified above).
            let rc = unsafe {
                minijail_add_hook(
                    jail.0,
                    chdir_hook,
                    cwd_c.as_ptr().cast::<c_void>().cast_mut(),
                    MINIJAIL_HOOK_EVENT_PRE_EXECVE,
                )
            };
            if rc != 0 {
                return fail(format!("minijail_add_hook(chdir) -> {rc}"));
            }
        }

        // (7) Marshal filename + argv + envp as NUL-terminated C arrays. The
        // CStrings and the pointer Vecs MUST all outlive the run call.
        let Ok(filename) = CString::new(spec.filename.as_str()) else {
            return fail("filename contains an interior NUL byte".into());
        };
        let Ok(argv_c) = spec
            .argv
            .iter()
            .map(|a| CString::new(a.as_str()))
            .collect::<Result<Vec<CString>, _>>()
        else {
            return fail("argv entry contains an interior NUL byte".into());
        };
        let Ok(envp_c) = spec
            .envp
            .iter()
            .map(|(k, v)| CString::new(format!("{k}={v}")))
            .collect::<Result<Vec<CString>, _>>()
        else {
            return fail("env entry contains an interior NUL byte".into());
        };
        // NULL-terminated arrays of `*mut c_char`.
        let mut argv: Vec<*mut c_char> = argv_c
            .iter()
            .map(|c| c.as_ptr().cast_mut())
            .chain(std::iter::once(ptr::null_mut()))
            .collect();
        let mut envp: Vec<*mut c_char> = envp_c
            .iter()
            .map(|c| c.as_ptr().cast_mut())
            .chain(std::iter::once(ptr::null_mut()))
            .collect();

        // (8) Fork + exec inside the jail. stdin is NULL (no stdin in v1).
        // stdin deferred — ProcessCommand.stdin exists but the Shell tool never
        // sets it; wire a stdin pipe in a later task.
        let mut pid: libc::pid_t = 0;
        let mut stdout_fd: c_int = -1;
        let mut stderr_fd: c_int = -1;
        // SAFETY: `jail.0` is valid. `filename`/`argv_c`/`envp_c` CStrings and
        // the `argv`/`envp` pointer Vecs all outlive this call (declared above,
        // dropped only at end of scope). Both pointer arrays are NUL-terminated.
        // `pid`/`stdout_fd`/`stderr_fd` are valid out-params; stdin is NULL
        // ("no stdin pipe" per libminijail.h). The seccomp filter (`_keep_insns`)
        // is still alive here, as required by set_seccomp_filters' non-ownership.
        let rc = unsafe {
            minijail_run_env_pid_pipes_no_preload(
                jail.0,
                filename.as_ptr(),
                argv.as_mut_ptr(),
                envp.as_mut_ptr(),
                &raw mut pid,
                ptr::null_mut(), // stdin deferred
                &raw mut stdout_fd,
                &raw mut stderr_fd,
            )
        };
        if rc != 0 {
            return fail(format!("minijail_run_env_pid_pipes_no_preload -> {rc}"));
        }
        if pid <= 0 {
            return fail(format!("minijail_run returned pid {pid}"));
        }
        if stdout_fd < 0 || stderr_fd < 0 {
            // Should never happen: minijail_run succeeded (rc == 0, pid > 0) but
            // handed back a bad fd. Stay fail-closed AND leak-free: close any
            // valid fd we did get, and reap the live child so we leave no zombie.
            // SAFETY: each `close` targets a fd value minijail just returned to
            // us by value; we only close the ones that are >= 0 (valid), and we
            // own them (the reader threads that would otherwise own them have not
            // been spawned on this path). `minijail_wait` reaps `jail.0`'s child,
            // which has not been waited on.
            unsafe {
                if stdout_fd >= 0 {
                    libc::close(stdout_fd);
                }
                if stderr_fd >= 0 {
                    libc::close(stderr_fd);
                }
                minijail_wait(jail.0);
            }
            return fail(format!(
                "minijail_run gave bad pipe fds (out={stdout_fd}, err={stderr_fd})"
            ));
        }

        // The child is the leader of its own pgid (== its pid) thanks to
        // create_session, so kill(-child_pgid) targets ONLY the child group and
        // never the engine's process group.
        let child_pgid = pid;

        // (9) Concurrent capture: drain stdout AND stderr on their own threads so
        // a child filling one pipe buffer while we read the other cannot deadlock.
        // Each thread owns and closes its fd exactly once.
        let stdout_reader = spawn_reader(stdout_fd);
        let stderr_reader = spawn_reader(stderr_fd);

        // (10) Watchdog: after timeout_ms, set `timed_out` and kill the child's
        // process group. It is CANCELLABLE via `done`, and — crucially — it is
        // ALWAYS joined BEFORE the reaping `minijail_wait` (see the ordering
        // proof at step 11), so a `kill(-pgid)` can never fire on a pgid that
        // `minijail_wait` has already reaped and the OS may have recycled. The
        // watchdog polls `done` on a short interval rather than sleeping the full
        // timeout, so cancellation is prompt (returns within one tick of `done`).
        let timed_out = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let watchdog = {
            let timed_out = Arc::clone(&timed_out);
            let done = Arc::clone(&done);
            let timeout = Duration::from_millis(spec.timeout_ms);
            thread::spawn(move || {
                let start = Instant::now();
                let tick = Duration::from_millis(20);
                loop {
                    if done.load(Ordering::SeqCst) {
                        // Normal/background path: the leader exited,
                        // `wait_leader_exit` returned and set `done` — all before
                        // the reaping minijail_wait. Returning here means this
                        // watchdog fires no kill (and never sets `timed_out`), so
                        // a fast command that backgrounds a child is not reported
                        // as timed out and there is no recycled-pgid hazard.
                        return;
                    }
                    if start.elapsed() >= timeout {
                        // Timeout path: the child is genuinely hung (it ignored
                        // the deadline and has not closed its pipes), so it is
                        // still LIVE and UNREAPED — minijail_wait runs only after
                        // this watchdog is joined, and that join happens only
                        // after the readers hit EOF, which the kill below causes.
                        // So the kill always precedes the reap; the pgid cannot be
                        // recycled yet. Re-check `done` once more to lose the race
                        // to a child that exited in the last tick.
                        if done.load(Ordering::SeqCst) {
                            return;
                        }
                        // Mark BEFORE the kill so the result reports timed_out
                        // regardless of how wait() decodes the SIGKILL.
                        timed_out.store(true, Ordering::SeqCst);
                        // SAFETY: `child_pgid` is the child's own pgid (== its
                        // pid; it leads a fresh session from create_session).
                        // Negating it targets that group ONLY — never the
                        // engine's group (which has a different, positive pgid).
                        // The child is unreaped here (reap is the very last step,
                        // after this thread is joined), so the pgid is still ours
                        // and cannot have been recycled.
                        #[allow(unsafe_code)]
                        unsafe {
                            libc::kill(-child_pgid, libc::SIGKILL);
                        }
                        return;
                    }
                    thread::sleep(tick);
                }
            })
        };

        // (11) Tear down so the wall-clock tracks the LEADER's exit, not pipe
        // EOF, while keeping `minijail_wait` the sole, LAST reap and closing the
        // kill/reap recycled-pgid window.
        //
        // We block on the leader via `wait_leader_exit` (waitid WNOWAIT), NOT by
        // joining the pipe readers first: a backgrounded grandchild that holds a
        // stdout/stderr write end (`sleep 300 & echo hi`) would otherwise keep
        // the readers blocked and pin the call to the full timeout even though
        // the foreground command finished instantly.
        //
        // RECYCLE-PGID SAFETY: `wait_leader_exit` leaves the leader a reapable
        // ZOMBIE, which keeps its pid/pgid reserved until `minijail_wait` reaps
        // it last. So the group-cleanup `kill(-pgid)` below — and any late
        // watchdog kill — can never hit a recycled pgid. Walk both paths:
        //
        //  - Normal/background exit: the leader exits; `wait_leader_exit`
        //    returns; we cancel the watchdog (`done`) and `kill(-pgid)` to tear
        //    down any backgrounded grandchildren (a tool call is complete — its
        //    background procs must not outlive it). That closes every pipe write
        //    end, so the readers hit EOF; we join them, join the watchdog (which
        //    saw `done` and never killed / never set timed_out), then reap.
        //
        //  - Hang/timeout: the leader ignores the deadline; the watchdog's
        //    deadline elapses, it sets `timed_out` and SIGKILLs the (still-ours)
        //    pgid; the leader dies; `wait_leader_exit` returns; we kill the group
        //    again (harmless — pgid still reserved by the zombie), join readers
        //    (now EOF), join the watchdog (already returned), then reap.
        //
        // In every path the cleanup kill happens-before the reap, and the pgid is
        // reserved by the un-reaped zombie throughout — no recycle hazard.
        wait_leader_exit(pid);

        // Leader has exited (zombie, reserving the pgid). Cancel the watchdog and
        // kill the child's process group to terminate any lingering backgrounded
        // grandchildren still holding the pipe.
        // SAFETY: `child_pgid` is the child's own session pgid; negating targets
        // only that group. The zombie leader keeps the pgid reserved, so this can
        // never hit a recycled group; killing a group whose only member is the
        // zombie leader is a harmless no-op.
        done.store(true, Ordering::SeqCst);
        unsafe { libc::kill(-child_pgid, libc::SIGKILL) };

        // Readers now hit EOF promptly (every pipe write end is closed by the
        // group kill). Bound the final join: in the rare case a grandchild did
        // its OWN setsid — escaping the group kill — and still holds a pipe end,
        // wait a short grace and then DETACH that reader rather than block
        // forever. The common in-group `cmd & background` case EOFs within a few
        // ms, so this adds no latency there.
        let drain_start = Instant::now();
        let drain_grace = Duration::from_millis(200);
        while (!stdout_reader.is_finished() || !stderr_reader.is_finished())
            && drain_start.elapsed() < drain_grace
        {
            thread::sleep(Duration::from_millis(5));
        }
        let stdout_bytes = if stdout_reader.is_finished() {
            stdout_reader.join().unwrap_or_default()
        } else {
            Vec::new()
        };
        let stderr_bytes = if stderr_reader.is_finished() {
            stderr_reader.join().unwrap_or_default()
        } else {
            Vec::new()
        };

        // Join the watchdog (it returned on `done`, or after its own timeout
        // kill). After this, no kill can run again.
        let _ = watchdog.join();

        // Reap LAST. minijail_wait reaps the zombie leader; prompt because the
        // leader has already exited.
        // SAFETY: `jail.0` is valid and its child has not been waited on yet.
        let status = unsafe { minijail_wait(jail.0) };

        let was_timed_out = timed_out.load(Ordering::SeqCst);
        let exit_code = if was_timed_out {
            -1
        } else {
            super::decode_exit(status)
        };

        JailedOutput {
            stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
            stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
            exit_code,
            timed_out: was_timed_out,
            enforcement_failed: None,
        }
        // `jail` (Drop -> minijail_destroy), `_keep_insns`, `filter_prog`,
        // `cwd_c`, `filename`, `argv_c`/`envp_c`, `argv`/`envp` all drop here —
        // after the run + wait, so every pointer handed to minijail stayed valid
        // for the whole jailed lifetime.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_run_reports_enforcement_failure() {
        let spec = JailSpec {
            filename: "/system/bin/sh".into(),
            argv: vec!["sh".into()],
            envp: vec![],
            cwd: "/".into(),
            rlimits: vec![],
            net_deny: false,
            bpf: vec![],
            timeout_ms: 1000,
        };
        let out = run_jailed(&spec);
        assert!(out.enforcement_failed.is_some());
        assert_eq!(out.exit_code, -1);
    }

    // `decode_exit` is a pure mapping over `minijail_wait`'s return int, so it is
    // exercised on the host even though the FFI jail body is android-only.
    #[test]
    fn decode_exit_normal_codes() {
        assert_eq!(decode_exit(0), 0); // exit 0
        assert_eq!(decode_exit(1), 1); // exit 1
        assert_eq!(decode_exit(126), 126); // bash "cannot execute" / minijail NO_ACCESS
        assert_eq!(decode_exit(127), 127); // bash "not found" / minijail NO_COMMAND
    }

    #[test]
    fn decode_exit_high_codes_are_preserved() {
        // The bug case: a child that VOLUNTARILY `exit 137` is WIFEXITED with
        // WEXITSTATUS == 137, so minijail_wait returns 137 — it must NOT be
        // collapsed to -1 just because it crosses the 128 boundary. The
        // watchdog-kill case is carried independently by `timed_out`.
        assert_eq!(decode_exit(137), 137); // exit 137 (was -1 before the fix)
        assert_eq!(decode_exit(200), 200); // exit 200 (was -1 before the fix)
        assert_eq!(decode_exit(128), 128);
        assert_eq!(decode_exit(255), 255);
    }

    #[test]
    fn decode_exit_signalled_child_shares_the_high_band() {
        // A child killed by signal `n` is reported by minijail as 128 + n (bash
        // convention), so e.g. SIGKILL(9) -> 137 and SIGSEGV(11) -> 139. These
        // are INDISTINGUISHABLE from a voluntary `exit 137` / `exit 139`, so
        // decode_exit surfaces the low byte either way; callers must consult
        // `timed_out` for the watchdog-kill cause rather than treating the code
        // as a kill marker.
        assert_eq!(decode_exit(128 + 9), 137); // SIGKILL via minijail encoding
        assert_eq!(decode_exit(128 + 11), 139); // SIGSEGV via minijail encoding
                                                // SECCOMP violation (SIGSYS) is reported as MINIJAIL_ERR_SECCOMP_VIOLATION(253).
        assert_eq!(decode_exit(253), 253);
    }

    #[test]
    fn decode_exit_negative_is_internal_error() {
        // minijail_wait returns a negative value (-ECHILD / -errno) on its own
        // internal failure — never a child exit code, so it maps to -1.
        assert_eq!(decode_exit(-1), -1);
        assert_eq!(decode_exit(-10), -1);
    }
}
