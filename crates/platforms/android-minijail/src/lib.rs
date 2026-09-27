//! Minijail FFI wrapper (P0a): the smoke probe proving libminijail links and
//! a jailed `sh -c true` survives on-device. The P2 plan adds the full
//! `minijail_run_pid_pipes` spawn path here.
//!
//! This is the only workspace crate allowed to touch minijail FFI. The
//! Android-only module below binds the minimal set of `libminijail.h` entry
//! points directly (the upstream `minijail`/`minijail-sys` crates are
//! bypassed — their build script cannot cross-compile from a macOS host; see
//! `build.rs`). Host builds compile no unsafe code at all and report the
//! smoke as structurally unavailable.

pub mod seccomp;
pub use seccomp::{
    build_net_deny_bpf, net_deny_bpf_for_target, net_deny_bpf_hash, net_deny_policy_identity_hash,
    net_deny_policy_name, BpfInsn,
};

mod run;
pub use run::run_jailed;

mod probe;
pub use probe::{probe_extras, ProbeExtras};

use serde::Serialize;

/// An rlimit to apply in the jailed child (resource = a raw `RLIMIT_*` int).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JailRlimit {
    /// Raw `RLIMIT_*` constant (resolved by the caller — `platform-android`
    /// has no libc, so the resource is passed as the integer).
    pub resource: i32,
    /// Soft limit.
    pub soft: u64,
    /// Hard limit.
    pub hard: u64,
}

/// Everything `run_jailed` needs to fork+jail+exec one command. Built by the
/// safe `platform-android` side from an `AndroidSandboxPlan`.
#[derive(Debug, Clone)]
pub struct JailSpec {
    /// Absolute exec target (`/system/bin/sh` for v1).
    pub filename: String,
    /// Full argv (argv[0] included).
    pub argv: Vec<String>,
    /// The complete child environment (scrubbed allowlist — the ONLY env).
    pub envp: Vec<(String, String)>,
    /// Working directory applied via a `PRE_EXECVE` hook (minijail has no cwd API).
    pub cwd: String,
    /// Rlimits to apply before exec.
    pub rlimits: Vec<JailRlimit>,
    /// Whether to install the net-deny seccomp filter.
    pub net_deny: bool,
    /// The net-deny classic-BPF program (from `build_net_deny_bpf`, empty when
    /// `!net_deny`); `run_jailed` maps it to `sock_fprog` and injects it via
    /// `minijail_set_seccomp_filters`.
    pub bpf: Vec<BpfInsn>,
    /// Wall-clock budget; the watchdog kills the process group past this.
    pub timeout_ms: u64,
}

/// Result of a completed (or timed-out / enforcement-failed) jailed run.
#[derive(Debug, Clone)]
pub struct JailedOutput {
    /// Captured stdout (UTF-8 lossy).
    pub stdout: String,
    /// Captured stderr (UTF-8 lossy).
    pub stderr: String,
    /// The child's exit code (`status & 0xFF` of `minijail_wait`), or `-1` for an
    /// enforcement failure, a watchdog timeout, or a minijail-internal error.
    ///
    /// NOTE: this is NOT a reliable signal-kill marker. `minijail_wait` folds a
    /// signal-killed child into the same `128 + n` band as a voluntary `exit 137`,
    /// so a high exit code here may be either; callers MUST consult `timed_out`
    /// (not `exit_code == -1`) to detect the watchdog kill. See `decode_exit`.
    pub exit_code: i32,
    /// True when the watchdog killed the group for exceeding `timeout_ms`.
    pub timed_out: bool,
    /// `Some(reason)` when jail SETUP failed (filter load, rlimit, fork) —
    /// the runner maps this to `SandboxEnforcementFailed`, never a silent run.
    pub enforcement_failed: Option<String>,
}

/// `RLIMIT_CPU` as an `i32` for [`JailRlimit`] — a tiny helper so the host test
/// (and `platform-android`) need not depend on libc just to name the constant.
#[must_use]
pub fn libc_rlimit_cpu() -> i32 {
    0 // RLIMIT_CPU == 0 on Linux/Android.
}

/// Result of the on-device minijail smoke (serialized to the instrumentation
/// test through the `android_sandbox_smoke()` `UniFFI` export).
#[derive(Debug, Clone, Serialize)]
pub struct SmokeResult {
    /// Overall pass.
    pub ok: bool,
    /// `no_new_privs` was applied.
    pub no_new_privs: bool,
    /// The jailed child ran and exited 0.
    pub child_exit_zero: bool,
    /// Failure detail when `ok == false`.
    pub reason: Option<String>,
}

impl SmokeResult {
    /// Failure result captured before/after `no_new_privs` was applied.
    fn fail(no_new_privs: bool, reason: String) -> Self {
        Self {
            ok: false,
            no_new_privs,
            child_exit_zero: false,
            reason: Some(reason),
        }
    }
}

/// Run the minijail smoke. Host builds report a structural "not android".
#[must_use]
pub fn minijail_smoke() -> SmokeResult {
    #[cfg(not(target_os = "android"))]
    {
        SmokeResult::fail(false, "minijail smoke requires an Android device".into())
    }
    #[cfg(target_os = "android")]
    {
        android_impl::smoke()
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    // LINK ANCHOR — do not remove. `platform-android-libcap` is a build-only
    // crate (empty lib.rs) whose rlib BUNDLES the static libcap objects that
    // resolve libminijail's `cap_*` references. rustc only links crates that
    // are actually referenced, and a `-shared` (cdylib) link does not error on
    // the resulting undefined symbols — without this reference the `.so`
    // builds fine but `dlopen` fails on-device with
    // `cannot locate symbol "cap_get_proc"` (caught by the P0a smoke gate).
    use platform_android_libcap as _;

    use super::SmokeResult;
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};
    use std::ptr;

    /// Opaque `struct minijail` handle from `libminijail.h`.
    #[repr(C)]
    struct RawMinijail {
        _opaque: [u8; 0],
    }

    // Hand-written bindings for the exact `libminijail.h` prototypes the
    // smoke needs (libminijail.a is built and linked by build.rs):
    //   struct minijail *minijail_new(void);
    //   void minijail_no_new_privs(struct minijail *j);
    //   int minijail_preserve_fd(struct minijail *j, int parent_fd,
    //                            int child_fd);
    //   int minijail_run_pid_pipes_no_preload(struct minijail *j,
    //       const char *filename, char *const argv[], pid_t *pchild_pid,
    //       int *pstdin_fd, int *pstdout_fd, int *pstderr_fd);
    //   int minijail_wait(struct minijail *j);
    //   void minijail_destroy(struct minijail *j);
    #[allow(unsafe_code)]
    extern "C" {
        fn minijail_new() -> *mut RawMinijail;
        fn minijail_no_new_privs(j: *mut RawMinijail);
        fn minijail_preserve_fd(j: *mut RawMinijail, parent_fd: c_int, child_fd: c_int) -> c_int;
        fn minijail_run_pid_pipes_no_preload(
            j: *mut RawMinijail,
            filename: *const c_char,
            argv: *const *mut c_char,
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
            // SAFETY: `self.0` is a non-null handle returned by
            // `minijail_new` (checked at construction) and destroyed once.
            #[allow(unsafe_code)]
            unsafe {
                minijail_destroy(self.0);
            }
        }
    }

    #[allow(unsafe_code)]
    pub(super) fn smoke() -> SmokeResult {
        // (a) Construct a jail and set no_new_privs.
        // SAFETY: plain constructor; the NULL result is checked.
        let jail = unsafe { minijail_new() };
        if jail.is_null() {
            return SmokeResult::fail(false, "minijail_new returned NULL".into());
        }
        let jail = Jail(jail);
        // SAFETY: `jail.0` is valid; setter has no other preconditions.
        unsafe { minijail_no_new_privs(jail.0) };

        // (b) Inherit stdio: preserve fds 0/1/2 into the child as-is.
        for fd in 0..3 {
            // SAFETY: `jail.0` is valid; fds are plain integers.
            let rc = unsafe { minijail_preserve_fd(jail.0, fd, fd) };
            if rc != 0 {
                return SmokeResult::fail(true, format!("minijail_preserve_fd({fd}) -> {rc}"));
            }
        }

        // (c) Fork + exec `/system/bin/sh -c true` inside the jail.
        let filename = CString::new("/system/bin/sh").expect("static path");
        let arg_cstrings = ["sh", "-c", "true"].map(|a| CString::new(a).expect("static arg"));
        let mut argv: Vec<*mut c_char> = arg_cstrings
            .iter()
            .map(|a| a.as_ptr().cast_mut())
            .chain(std::iter::once(ptr::null_mut()))
            .collect();
        let mut pid: libc::pid_t = 0;
        // SAFETY: `filename`/`arg_cstrings` CStrings outlive the call, `argv` is
        // NULL-terminated, `pid` is a valid out-pointer, and the NULL pipe
        // pointers mean "no pipes" per libminijail.h.
        let rc = unsafe {
            minijail_run_pid_pipes_no_preload(
                jail.0,
                filename.as_ptr(),
                argv.as_mut_ptr(),
                &raw mut pid,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != 0 {
            return SmokeResult::fail(true, format!("minijail_run -> {rc}"));
        }
        if pid <= 0 {
            return SmokeResult::fail(true, format!("minijail_run returned pid {pid}"));
        }

        // (d) Wait: minijail_wait returns the child's nonnegative exit
        // status, or a negative error.
        // SAFETY: `jail.0` is valid and its child has not been waited yet.
        let status = unsafe { minijail_wait(jail.0) };
        let exited_zero = status == 0;
        SmokeResult {
            ok: exited_zero,
            no_new_privs: true,
            child_exit_zero: exited_zero,
            reason: if exited_zero {
                None
            } else {
                Some(format!("minijail_wait -> {status} (pid {pid})"))
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_smoke_reports_structurally_unavailable() {
        let r = minijail_smoke();
        assert!(!r.ok);
        assert!(!r.no_new_privs);
        assert!(!r.child_exit_zero);
        assert!(r.reason.unwrap().contains("Android"));
    }

    #[test]
    fn jail_spec_and_output_construct() {
        let spec = JailSpec {
            filename: "/system/bin/sh".into(),
            argv: vec!["sh".into(), "-c".into(), "true".into()],
            envp: vec![("HOME".into(), "/data/x".into())],
            cwd: "/data/x".into(),
            rlimits: vec![JailRlimit {
                resource: libc_rlimit_cpu(),
                soft: 30,
                hard: 30,
            }],
            net_deny: true,
            bpf: build_net_deny_bpf(&[198, 199], 0xC000_00B7),
            timeout_ms: 120_000,
        };
        assert_eq!(spec.argv.len(), 3);
        let out = JailedOutput {
            stdout: "ok\n".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            enforcement_failed: None,
        };
        assert!(out.enforcement_failed.is_none());
    }
}
