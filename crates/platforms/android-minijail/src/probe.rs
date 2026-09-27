//! Capability probe extras (spec r3 §Capability probing items 2-9). Each probe
//! is best-effort and INDEPENDENTLY fallible: a probe that fails sets its field
//! `false`/`None` and never panics or poisons the others. The android body forks
//! disposable children so a kill/seccomp/socket experiment can never disturb the
//! engine process. Host builds compile no unsafe code and return all-false/empty
//! — the safe `platform-android` side asserts that conservatism.
//!
//! This runs BEFORE the capability cache is populated, so the probe must NOT use
//! the sandboxed `run_jailed` path (which itself depends on the caps). The
//! `/system/bin/sh` and `toybox` inventory probes use a plain `std::process::Command`.

/// The remaining probe results that complete `AndroidSandboxCapabilities`
/// beyond the P0a minijail smoke. Built by [`probe_extras`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // mirrors the spec's probe list verbatim
pub struct ProbeExtras {
    /// A harmless seccomp filter installed in a disposable child (`PR_SET_SECCOMP`).
    pub seccomp_filter: bool,
    /// The filter could be installed with `SECCOMP_FILTER_FLAG_TSYNC`.
    pub seccomp_tsync: bool,
    /// A child under the net-deny filter observed `socket()` ⇒ `EPERM` — the
    /// strong proof the net-deny filter actually blocks socket creation.
    pub net_deny_verified: bool,
    /// `kill(-pgid)` tore down a disposable, sessioned child (confirmed SIGKILL).
    pub pgid_kill: bool,
    /// Landlock ABI version when the kernel exposes it (expected `None` on GKI
    /// devices, which do not enable Landlock).
    pub landlock_abi: Option<u32>,
    /// `$KSH_VERSION` from `/system/bin/sh` when readable.
    pub system_sh_version: Option<String>,
    /// Probed toybox applet inventory (feeds the Shell tool prompt).
    pub toybox_applets: Vec<String>,
}

/// Run the capability probe extras. Host builds report conservative defaults
/// (all false / `None` / empty); the real probes are android-only.
#[must_use]
pub fn probe_extras() -> ProbeExtras {
    #[cfg(not(target_os = "android"))]
    {
        ProbeExtras::default()
    }
    #[cfg(target_os = "android")]
    {
        android_impl::probe_extras()
    }
}

#[cfg(target_os = "android")]
mod android_impl {
    use super::ProbeExtras;
    use crate::seccomp::{net_deny_bpf_for_target, BpfInsn};
    use std::process::Command;

    // ── Seccomp / Landlock syscall + flag constants ────────────────────────
    // libc (0.2) does NOT export `SECCOMP_*`, `SECCOMP_FILTER_FLAG_TSYNC`, or
    // the `landlock_*` syscall numbers on android. We reproduce them from the
    // stable kernel ABI headers and cite each source. They are identical on
    // every arch (the `landlock` syscall numbers are the only arch-sensitive
    // ones — both arm64 and x86_64 happen to share them on modern kernels;
    // resolved via `libc::SYS_landlock_create_ruleset` below, which libc DOES
    // expose, so we never hard-code that number).

    /// `PR_SET_SECCOMP` — `<linux/prctl.h>` (== 22). libc exposes this one.
    const PR_SET_SECCOMP: libc::c_int = libc::PR_SET_SECCOMP;
    /// `SECCOMP_MODE_FILTER` — `<linux/seccomp.h>` (== 2). Typed as the
    /// `prctl(2)` argument type (`c_ulong`).
    const SECCOMP_MODE_FILTER: libc::c_ulong = 2;
    /// `SECCOMP_SET_MODE_FILTER` — `seccomp(2)` operation (== 1). Typed as the
    /// `syscall(2)` variadic argument type (`c_long`).
    const SECCOMP_SET_MODE_FILTER: libc::c_long = 1;
    /// `SECCOMP_FILTER_FLAG_TSYNC` — `<linux/seccomp.h>` (== 1). Typed as the
    /// `syscall(2)` argument type (`c_long`).
    const SECCOMP_FILTER_FLAG_TSYNC: libc::c_long = 1;
    /// `LANDLOCK_CREATE_RULESET_VERSION` — `<linux/landlock.h>` flag asking the
    /// kernel to report the supported ABI version instead of creating a ruleset
    /// (== 1 << 0). Typed as the `syscall(2)` argument type (`c_long`).
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_long = 1;

    /// Map the host-built `BpfInsn`s to `libc::sock_filter` (identical 4-field
    /// layout) — shared by the seccomp-filter and net-deny probes.
    fn to_sock_filters(bpf: &[BpfInsn]) -> Vec<libc::sock_filter> {
        bpf.iter()
            .map(|i| libc::sock_filter {
                code: i.code,
                jt: i.jt,
                jf: i.jf,
                k: i.k,
            })
            .collect()
    }

    /// A 1-insn allow-everything classic-BPF program: unconditionally
    /// `RET ALLOW`. Used by the harmless `seccomp_filter`/`seccomp_tsync` install
    /// probes — it imposes no policy, it only proves the kernel accepts a seccomp
    /// filter from this process.
    fn allow_all_bpf() -> Vec<libc::sock_filter> {
        use crate::seccomp::{BPF_RET_K, SECCOMP_RET_ALLOW};
        vec![libc::sock_filter {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        }]
    }

    /// Wrap a `sock_filter` slice in a `sock_fprog`. The probe programs are tiny
    /// (1 insn allow-all, < 20 insns net-deny), so the length always fits `u16`;
    /// `try_from` keeps the conversion infallible-by-construction without a cast.
    /// Returns `None` if a program were ever too long (then the probe stays
    /// `false`). The returned `sock_fprog` borrows `filters` — keep it alive.
    fn fprog(filters: &[libc::sock_filter]) -> Option<libc::sock_fprog> {
        let len = u16::try_from(filters.len()).ok()?;
        Some(libc::sock_fprog {
            len,
            filter: filters.as_ptr().cast_mut(),
        })
    }

    /// Fork a disposable child, run `child` in it, and report whether the child
    /// exited with status 0. The child closure MUST end by calling `_exit`
    /// (never return to the forked Rust stack — that would run destructors
    /// twice); as a belt-and-braces guard we `_exit(127)` if it ever returns.
    /// Any fork or wait failure yields `false`. The parent reaps with `waitpid`.
    ///
    /// SAFETY of the model: between `fork()` and the child's `_exit`, the child
    /// calls only async-signal-safe operations (the supplied `child` closure must
    /// honour that). We never touch the heap allocator across the fork in a way
    /// that could deadlock because the closures below only do raw syscalls.
    #[allow(unsafe_code)]
    fn fork_child_exits_zero(child: impl FnOnce()) -> bool {
        // SAFETY: `fork` duplicates the process; we handle all three outcomes.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return false; // fork failed — best-effort probe stays false.
        }
        if pid == 0 {
            // Child: run the experiment, which ends in `_exit`. If it ever
            // returns (it must not), exit non-zero rather than unwind.
            child();
            // SAFETY: async-signal-safe terminator on the unreachable path.
            unsafe { libc::_exit(127) };
        }
        // Parent: reap the child and inspect its exit status.
        let mut status: libc::c_int = 0;
        // SAFETY: `pid` is our direct child; `&mut status` is a valid out-param.
        let rc = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if rc != pid {
            return false;
        }
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
    }

    /// Install a harmless allow-all seccomp filter via
    /// `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &fprog)` in a throwaway child.
    /// Child exits 0 on success. Requires `no_new_privs` first (unprivileged
    /// seccomp precondition).
    #[allow(unsafe_code)]
    fn probe_seccomp_filter() -> bool {
        let filters = allow_all_bpf();
        fork_child_exits_zero(move || {
            // SAFETY (child, async-signal-safe path): set no_new_privs, then load
            // the allow-all filter via prctl, then _exit with the result. No heap
            // growth, no destructors run.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(1);
                }
                let Some(prog) = fprog(&filters) else {
                    libc::_exit(1);
                };
                let rc = libc::prctl(
                    PR_SET_SECCOMP,
                    SECCOMP_MODE_FILTER,
                    std::ptr::addr_of!(prog) as libc::c_ulong,
                    0,
                    0,
                );
                libc::_exit(i32::from(rc != 0));
            }
        })
    }

    /// Attempt the allow-all filter with `SECCOMP_FILTER_FLAG_TSYNC` via the
    /// `seccomp(2)` syscall in a throwaway child. Child exits 0 on success.
    #[allow(unsafe_code)]
    fn probe_seccomp_tsync() -> bool {
        let filters = allow_all_bpf();
        fork_child_exits_zero(move || {
            // SAFETY (child): no_new_privs, then seccomp(SET_MODE_FILTER, TSYNC,
            // &prog). `SYS_seccomp` is exported by libc on android.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(1);
                }
                let Some(prog) = fprog(&filters) else {
                    libc::_exit(1);
                };
                let rc = libc::syscall(
                    libc::SYS_seccomp,
                    SECCOMP_SET_MODE_FILTER,
                    SECCOMP_FILTER_FLAG_TSYNC,
                    std::ptr::addr_of!(prog) as libc::c_long,
                );
                libc::_exit(i32::from(rc != 0));
            }
        })
    }

    /// Fork a child, install the REAL net-deny BPF (allow-default, socket-family
    /// ⇒ EPERM), then have the child call `socket(AF_INET, SOCK_STREAM, 0)` and
    /// report (via exit code) whether it got `EPERM`. THIS is the strong proof
    /// the net-deny filter actually blocks socket creation. Exit 0 ⇒ `socket()`
    /// returned -1 with errno `EPERM`.
    #[allow(unsafe_code)]
    fn probe_net_deny_verified() -> bool {
        let bpf = net_deny_bpf_for_target();
        if bpf.is_empty() {
            return false; // no per-target program (non-android arch) — unproven.
        }
        let filters = to_sock_filters(&bpf);
        fork_child_exits_zero(move || {
            // SAFETY (child): no_new_privs, load the net-deny filter, then try to
            // create a socket. We expect EPERM (the filter's ERRNO action), which
            // the kernel delivers as socket() == -1 && errno == EPERM.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(2);
                }
                let Some(prog) = fprog(&filters) else {
                    libc::_exit(2);
                };
                if libc::prctl(
                    PR_SET_SECCOMP,
                    SECCOMP_MODE_FILTER,
                    std::ptr::addr_of!(prog) as libc::c_ulong,
                    0,
                    0,
                ) != 0
                {
                    libc::_exit(3);
                }
                let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
                if fd >= 0 {
                    // The filter did NOT block — close and report failure.
                    libc::close(fd);
                    libc::_exit(4);
                }
                let err = *libc::__errno();
                libc::_exit(if err == libc::EPERM { 0 } else { 5 });
            }
        })
    }

    /// Fork a child that `setsid()`s (becoming its own session/pgid leader) then
    /// sleeps; `kill(-childpgid, SIGKILL)`; confirm via `waitpid` that it died
    /// from SIGKILL. Proves the watchdog's process-group teardown works.
    #[allow(unsafe_code)]
    fn probe_pgid_kill() -> bool {
        // SAFETY: `fork`; all three outcomes handled. The child only calls
        // async-signal-safe libc (setsid/pause/_exit).
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return false;
        }
        if pid == 0 {
            // Child: new session (pgid == pid), then block until killed.
            // SAFETY: async-signal-safe syscalls only.
            unsafe {
                libc::setsid();
                // pause() returns only when a handler-caught signal arrives;
                // SIGKILL is uncatchable so this never returns — the child is
                // terminated by the kill below. Loop defensively in case pause
                // is interrupted by some other signal.
                loop {
                    libc::pause();
                }
            }
        }
        // Parent: the child becomes leader of pgid == its pid after setsid().
        // There is a small window before setsid() runs; retry the group kill a
        // few times so we don't race the child's setsid. A direct kill(pid)
        // fallback is intentionally avoided — we are specifically proving the
        // GROUP kill path.
        let child_pgid = pid;
        let mut status: libc::c_int = 0;
        for _ in 0..50 {
            // SAFETY: negating the pgid targets the child's own group only.
            unsafe { libc::kill(-child_pgid, libc::SIGKILL) };
            // Non-blocking reap attempt; if not yet dead, give setsid/kill time.
            // SAFETY: `pid` is our child; `&mut status` valid; WNOHANG returns 0
            // while the child is still alive.
            let rc = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            if rc == pid {
                break;
            }
            // SAFETY: portable short sleep; `timespec` is a plain struct.
            unsafe {
                let ts = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 2_000_000, // 2ms
                };
                libc::nanosleep(std::ptr::addr_of!(ts), std::ptr::null_mut());
            }
        }
        // If WNOHANG never reaped it above, do one final blocking wait so we
        // never leak a zombie, then evaluate.
        if !(libc::WIFSIGNALED(status) || libc::WIFEXITED(status)) {
            // SAFETY: blocking reap of our own child.
            unsafe { libc::waitpid(pid, &raw mut status, 0) };
        }
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL
    }

    /// Query the Landlock ABI version:
    /// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`.
    /// Returns `Some(version)` when the kernel reports a positive ABI version,
    /// else `None` (expected on GKI devices, which return -1/ENOSYS).
    ///
    /// CRITICAL: this MUST run in a forked child, not in-process. Android
    /// installs an app-domain seccomp policy on every app process that does NOT
    /// allow `landlock_create_ruleset` (arm64 nr 444) — a bare in-process
    /// `syscall(444, …)` raises SIGSYS and aborts the WHOLE app (observed on the
    /// API-34 emulator: "seccomp prevented call to disallowed arm64 system call
    /// 444"). Forking contains that SIGSYS to a throwaway child: the parent reaps
    /// it and reports `None` for any non-clean exit (signalled, or non-zero) just
    /// as it would for ENOSYS — best-effort, never crashes the host process.
    ///
    /// The ABI version travels back through the child's exit code: a positive
    /// version `v` (always small — current Landlock ABI is single digits) exits
    /// `v`; anything else exits 0 meaning "no Landlock". The `0` sentinel is
    /// safe because Landlock ABI versions are `>= 1`.
    #[allow(unsafe_code)]
    fn probe_landlock_abi() -> Option<u32> {
        // SAFETY: `fork` duplicates the process; we handle all three outcomes.
        // The child runs only async-signal-safe syscalls and ends in `_exit`.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return None; // fork failed — best-effort probe stays None.
        }
        if pid == 0 {
            // Child: ask the kernel for the Landlock ABI. If Android's seccomp
            // policy blocks the syscall it SIGSYS-kills THIS child (contained);
            // otherwise we map a positive ABI onto the exit code.
            // SAFETY (child, async-signal-safe path): a single syscall + _exit;
            // no heap growth, no destructors. `SYS_landlock_create_ruleset` is
            // exported by libc on android; NULL attr + VERSION flag creates
            // nothing and touches no memory.
            unsafe {
                let rc = libc::syscall(
                    libc::SYS_landlock_create_ruleset,
                    std::ptr::null::<libc::c_void>(),
                    0usize,
                    LANDLOCK_CREATE_RULESET_VERSION as libc::c_long,
                );
                // Clamp into the 1..=125 exit-code range; 0 (and anything out
                // of range) means "no usable Landlock". `try_from` keeps the
                // conversion lossless and `(1..=125).contains` keeps it in range.
                let code = i32::try_from(rc)
                    .ok()
                    .filter(|v| (1..=125).contains(v))
                    .unwrap_or(0);
                libc::_exit(code);
            }
        }
        // Parent: reap and decode. A signalled child (e.g. Android's SIGSYS) or
        // a 0 exit both mean "no Landlock" → None.
        let mut status: libc::c_int = 0;
        // SAFETY: `pid` is our direct child; `&mut status` is a valid out-param.
        let rc = unsafe { libc::waitpid(pid, &raw mut status, 0) };
        if rc != pid || !libc::WIFEXITED(status) {
            return None;
        }
        let code = libc::WEXITSTATUS(status);
        if code >= 1 {
            u32::try_from(code).ok()
        } else {
            None
        }
    }

    /// Run `/system/bin/sh -c 'echo $KSH_VERSION'` (a plain, NON-jailed
    /// `std::process::Command` — the probe runs before caps are known) and trim
    /// the captured stdout. `None` if sh is absent or printed nothing.
    fn probe_system_sh_version() -> Option<String> {
        let out = Command::new("/system/bin/sh")
            .args(["-c", "echo $KSH_VERSION"])
            .output()
            .ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    /// Inventory toybox applets by running `toybox` with no args (it prints the
    /// applet list, whitespace-separated, possibly across multiple lines). Falls
    /// back to an empty vec on any failure.
    fn probe_toybox_applets() -> Vec<String> {
        let Ok(out) = Command::new("/system/bin/toybox").output() else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let mut applets: Vec<String> = text
            .split_whitespace()
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect();
        applets.sort_unstable();
        applets.dedup();
        applets
    }

    pub(super) fn probe_extras() -> ProbeExtras {
        // Each probe is independent and best-effort: a panic-free, fail-to-false
        // contract. They run sequentially; a failure in one does not affect the
        // next (no shared mutable state, no early return).
        ProbeExtras {
            seccomp_filter: probe_seccomp_filter(),
            seccomp_tsync: probe_seccomp_tsync(),
            net_deny_verified: probe_net_deny_verified(),
            pgid_kill: probe_pgid_kill(),
            landlock_abi: probe_landlock_abi(),
            system_sh_version: probe_system_sh_version(),
            toybox_applets: probe_toybox_applets(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_probe_extras_is_conservative() {
        // On the host (non-Android) the probe cannot run, so every field must be
        // the conservative default: false / None / empty. This is the merge gate
        // for the host build; the real probes are exercised on-device.
        let extras = probe_extras();
        assert_eq!(extras, ProbeExtras::default());
        assert!(!extras.seccomp_filter);
        assert!(!extras.seccomp_tsync);
        assert!(!extras.net_deny_verified);
        assert!(!extras.pgid_kill);
        assert!(extras.landlock_abi.is_none());
        assert!(extras.system_sh_version.is_none());
        assert!(extras.toybox_applets.is_empty());
    }
}
