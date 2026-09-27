//! `apply-seccomp` — install a seccomp BPF filter blocking `socket(AF_UNIX)`
//! (and `socketpair(AF_UNIX)`), then `execvp` the workload command.
//!
//! This is the faithful Rust equivalent of the pre-built C `apply-seccomp`
//! binary shipped by `@anthropic-ai/sandbox-runtime` (whose source is not
//! vendored). The package's documented behavioral contract is:
//!
//! > block `socket(AF_UNIX, …)` inside the sandbox so the workload cannot
//! > create its own unix sockets to reach the bridge sockets directly,
//! > forcing all egress through the TCP proxy listeners (defense-in-depth on
//! > top of `--unshare-net`).
//!
//! # Documented divergence from the original C binary
//!
//! The original `apply-seccomp` ALSO sets up a nested user + PID + mount
//! namespace, remounts `/proc`, and becomes a PID-1 reaper. In the `LingXi`
//! assembly (`wrap_command_with_sandbox_linux`), **bwrap already provides**
//! `--unshare-pid` + `--proc /proc` (the parent PID namespace and a fresh
//! `/proc`), so the security-essential part — the `socket(AF_UNIX)` seccomp
//! block — is what this binary must reproduce faithfully. The nested-ns /
//! reaper aspects are supplied by the surrounding bwrap arguments. (The C
//! source is unavailable to copy; we port the documented behavioral contract.)
//!
//! # Architecture support (TS parity)
//!
//! Only `x86_64` and `aarch64` are supported. 32-bit x86 (`ia32`) is rejected
//! for the same reason the TS rejects it: on 32-bit x86 all socket operations
//! are multiplexed through the `socketcall()` syscall rather than a direct
//! `socket()` syscall, so a filter that only blocks `socket()` would be a
//! security bypass. Any other architecture is also rejected.

#![cfg_attr(target_os = "linux", allow(unsafe_code))]

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::convert::TryInto;
    use std::ffi::CString;
    use std::process::ExitCode;

    use seccompiler::{
        BackendError, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
        SeccompFilter, SeccompRule, TargetArch,
    };

    /// `AF_UNIX` address-family constant (the `domain` arg0 value blocked).
    const AF_UNIX: u64 = libc::AF_UNIX as u64;

    /// The seccomp arch this binary was compiled for. `None` on an unsupported
    /// architecture (TS `ia32`/other → unsupported parity).
    #[must_use]
    fn target_arch() -> Option<TargetArch> {
        if cfg!(target_arch = "x86_64") {
            Some(TargetArch::x86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(TargetArch::aarch64)
        } else {
            None
        }
    }

    /// Build the seccomp filter for `arch`: default action `Allow`; the matched
    /// action is `Errno(EPERM)`; rules fire on `socket` and `socketpair` when
    /// their `domain` argument (arg0) equals `AF_UNIX`. Everything else —
    /// crucially `socket(AF_INET, …)` so egress through the proxy still works —
    /// is allowed.
    ///
    /// Factored out (pure, arch-parameterized) so it can be unit-tested for
    /// both `x86_64` and `aarch64` without installing anything.
    ///
    /// # Errors
    /// Returns the `seccompiler` backend error if the conditions/rules/filter
    /// are rejected by the compiler (cannot happen for these constants).
    pub fn build_seccomp_filter(arch: TargetArch) -> Result<SeccompFilter, BackendError> {
        // arg0 (domain) == AF_UNIX. The domain argument is an `int`, so a Dword
        // comparison is the correct width.
        // On x86_64/aarch64 `libc::SYS_*` is a `c_long` (== i64), which is the
        // `SeccompFilter` rule-map key type — no cast/truncation.
        let unix_domain = |syscall: libc::c_long| -> Result<(i64, Vec<SeccompRule>), BackendError> {
            let cond =
                SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, AF_UNIX)?;
            let rule = SeccompRule::new(vec![cond])?;
            Ok((syscall, vec![rule]))
        };

        let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        let (sock_nr, sock_rules) = unix_domain(libc::SYS_socket)?;
        rules.insert(sock_nr, sock_rules);
        let (sp_nr, sp_rules) = unix_domain(libc::SYS_socketpair)?;
        rules.insert(sp_nr, sp_rules);

        SeccompFilter::new(
            rules,
            // Default: allow every syscall not listed.
            SeccompAction::Allow,
            // When a listed rule matches (domain == AF_UNIX) → EPERM.
            SeccompAction::Errno(libc::EPERM as u32),
            arch,
        )
    }

    /// Compile [`build_seccomp_filter`] to an installable BPF program for the
    /// current (compile-time) architecture. `Ok(None)` signals an unsupported
    /// architecture (the caller treats it as a hard error, TS parity).
    ///
    /// # Errors
    /// `BackendError` if filter construction or BPF compilation fails.
    fn compiled_filter() -> Result<Option<BpfProgram>, BackendError> {
        let Some(arch) = target_arch() else {
            return Ok(None);
        };
        let filter = build_seccomp_filter(arch)?;
        Ok(Some(filter.try_into()?))
    }

    /// Entry point: set `NO_NEW_PRIVS`, install the seccomp filter, then
    /// `execvp` `argv[1..]`. Returns a non-zero `ExitCode` on any failure
    /// (the `execvp` only returns on error — success replaces the process).
    pub fn run() -> ExitCode {
        let cli_args: Vec<String> = std::env::args().collect();
        if cli_args.len() < 2 {
            eprintln!("apply-seccomp: usage: apply-seccomp <command> [args...]");
            return ExitCode::from(2);
        }

        // 1. NO_NEW_PRIVS — required so an unprivileged process may install a
        //    seccomp filter, and so the filter survives across the exec.
        if let Err(e) = nix::sys::prctl::set_no_new_privs() {
            eprintln!("apply-seccomp: failed to set NO_NEW_PRIVS: {e}");
            return ExitCode::FAILURE;
        }

        // 2. Build + install the AF_UNIX block. An unsupported architecture is a
        //    hard error (TS ia32-unsupported parity) — never silently run
        //    without the filter, which would be a security bypass.
        let prog = match compiled_filter() {
            Ok(Some(p)) => p,
            Ok(None) => {
                eprintln!(
                    "apply-seccomp: unsupported architecture (only x86_64 and aarch64 are \
                     supported; 32-bit x86 would allow a socketcall() bypass)"
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!("apply-seccomp: failed to build seccomp filter: {e}");
                return ExitCode::FAILURE;
            }
        };
        if let Err(e) = seccompiler::apply_filter(&prog) {
            eprintln!("apply-seccomp: failed to install seccomp filter: {e}");
            return ExitCode::FAILURE;
        }

        // 3. execvp the workload. On success this never returns.
        let Ok(prog_name) = CString::new(cli_args[1].as_str()) else {
            eprintln!("apply-seccomp: command contains a NUL byte");
            return ExitCode::FAILURE;
        };
        let exec_argv: Result<Vec<CString>, _> = cli_args[1..]
            .iter()
            .map(|s| CString::new(s.as_str()))
            .collect();
        let Ok(exec_argv) = exec_argv else {
            eprintln!("apply-seccomp: an argument contains a NUL byte");
            return ExitCode::FAILURE;
        };

        match nix::unistd::execvp(&prog_name, &exec_argv) {
            Ok(_never) => ExitCode::SUCCESS, // unreachable: execvp replaced us
            Err(e) => {
                eprintln!("apply-seccomp: failed to exec {}: {e}", cli_args[1]);
                ExitCode::FAILURE
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // The filter must compile for BOTH supported arches (the BPF bytecode
        // is arch-specific). If `SeccompFilter::new` accepted the rules and the
        // `TryInto<BpfProgram>` succeeds, the socket/socketpair AF_UNIX rules
        // are well-formed for that arch.
        fn assert_compiles(arch: TargetArch) {
            let filter = build_seccomp_filter(arch).expect("filter should build");
            let prog: BpfProgram = filter.try_into().expect("filter should compile to BPF");
            assert!(!prog.is_empty(), "compiled BPF program must be non-empty");
        }

        #[test]
        fn builds_for_x86_64() {
            assert_compiles(TargetArch::x86_64);
        }

        #[test]
        fn builds_for_aarch64() {
            assert_compiles(TargetArch::aarch64);
        }

        #[test]
        fn af_unix_is_one() {
            // The blocked domain constant is AF_UNIX == 1 on Linux.
            assert_eq!(AF_UNIX, 1);
        }

        #[test]
        fn current_arch_is_supported_here() {
            // CI runs on x86_64/aarch64 — the compile-time arch must resolve.
            assert!(target_arch().is_some());
        }
    }
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::run()
}

/// Non-Linux stub: the binary only has meaning on Linux (it installs a Linux
/// seccomp BPF filter). It is kept compilable so the workspace builds on
/// macOS/Windows, but errors at runtime.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("apply-seccomp is Linux-only");
    std::process::ExitCode::FAILURE
}
