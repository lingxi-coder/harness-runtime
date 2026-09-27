//! Net-deny seccomp BPF (spec r3 §Policy mapping: `network = Disabled` ⇒ seccomp
//! deny of socket-family syscalls). A hand-built classic-BPF program: allow by
//! default, return EPERM for socket-family syscalls. Pure data here; converted
//! to `sock_fprog` and injected via `minijail_set_seccomp_filters` on-device
//! (`run.rs`). Minijail policy *files* are allowlist/default-KILL and cannot
//! express allow-by-default — hence the raw filter (see plan Task 2 CORRECTION).

use sha2::{Digest, Sha256};

/// One classic-BPF instruction — the four fields of `struct sock_filter`.
/// Plain data so the program is built+hashed on the host; `run.rs` maps it to
/// `libc::sock_filter` on-device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BpfInsn {
    /// Opcode.
    pub code: u16,
    /// Jump-true offset.
    pub jt: u8,
    /// Jump-false offset.
    pub jf: u8,
    /// Generic field (immediate / return value).
    pub k: u32,
}

// Classic-BPF / seccomp constants (linux/bpf_common.h, linux/seccomp.h).
/// `BPF_LD | BPF_W | BPF_ABS` — load a 32-bit word from `seccomp_data` at `k`.
pub const BPF_LD_W_ABS: u16 = 0x20;
/// `BPF_JMP | BPF_JEQ | BPF_K` — jump if A == k.
pub const BPF_JEQ_K: u16 = 0x15;
/// `BPF_RET | BPF_K` — return constant k.
pub const BPF_RET_K: u16 = 0x06;
/// `seccomp_data` offsets: nr at 0, arch at 4.
const SECCOMP_DATA_NR_OFF: u32 = 0;
const SECCOMP_DATA_ARCH_OFF: u32 = 4;
/// `SECCOMP_RET_ALLOW`.
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
/// `SECCOMP_RET_KILL_THREAD` (`0x0000_0000`, the legacy bare `SECCOMP_RET_KILL`).
pub const SECCOMP_RET_KILL: u32 = 0x0000_0000;
/// `SECCOMP_RET_KILL_PROCESS` (`0x8000_0000`). Kills the WHOLE process on a
/// disallowed syscall, not just the offending thread — the correct fatal action
/// for the arch-mismatch guard. Available since Linux 4.14 (all Android 10+);
/// on older kernels an unrecognized RET action is masked down to KILL_THREAD, so
/// this degrades gracefully to the legacy behavior with no regression.
pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_ERRNO_BASE: u32 = 0x0005_0000;
const SECCOMP_RET_DATA: u32 = 0x0000_ffff;

/// `SECCOMP_RET_ERRNO | (errno & DATA)`.
#[must_use]
pub fn seccomp_ret_errno(errno: u32) -> u32 {
    SECCOMP_RET_ERRNO_BASE | (errno & SECCOMP_RET_DATA)
}

/// The policy name recorded in the receipt's `SeccompRef`.
#[must_use]
pub fn net_deny_policy_name() -> &'static str {
    "net-deny-v1"
}

/// The arch-independent socket-family syscall *names* the net-deny policy
/// denies. This is the POLICY IDENTITY: it is the same on every architecture
/// (only the per-arch syscall *numbers* differ, and those are resolved on
/// device). Used by [`net_deny_policy_identity_hash`].
///
/// Kept in sorted order so the identity hash is stable regardless of source
/// ordering.
const NET_DENY_SYSCALL_NAMES: &[&str] = &[
    "accept",
    "accept4",
    "bind",
    "connect",
    "getpeername",
    "getsockname",
    "getsockopt",
    "listen",
    "recvfrom",
    "recvmsg",
    "recvmmsg",
    "sendmmsg",
    "sendmsg",
    "sendto",
    "setsockopt",
    "shutdown",
    "socket",
    "socketpair",
];

/// Arch-INDEPENDENT identity hash of the net-deny policy, for the prepare-time
/// `SeccompRef.hash`.
///
/// # Why this and not the concrete BPF hash
///
/// `prepare()` is host-buildable and arch-agnostic — it runs before the runner
/// knows the device's architecture, so it cannot compute the per-arch BPF hash
/// (which depends on the concrete socket syscall *numbers* and the `AUDIT_ARCH`
/// tag; see [`net_deny_bpf_hash`]). To still give the receipt a stable,
/// verifiable policy identity at prepare time, we hash the POLICY DEFINITION:
/// the version name plus the sorted socket syscall *name* list. This is
/// identical on every arch (the policy is the same; only the numbers differ),
/// so two devices preparing the same plan record the same `SeccompRef`. The
/// runner can additionally note the concrete per-arch BPF hash in the receipt
/// when it has the device arch in hand.
#[must_use]
pub fn net_deny_policy_identity_hash() -> String {
    let mut h = Sha256::new();
    h.update(net_deny_policy_name().as_bytes());
    // Sort defensively so the identity is independent of source order.
    let mut names: Vec<&str> = NET_DENY_SYSCALL_NAMES.to_vec();
    names.sort_unstable();
    for name in names {
        h.update(b"\0");
        h.update(name.as_bytes());
    }
    hex::encode(h.finalize())
}

/// The net-deny classic-BPF program for the CURRENT build target.
///
/// On Android it resolves the per-arch socket syscall numbers (`libc::SYS_*`)
/// and the matching `AUDIT_ARCH_*` tag, then builds the program via
/// [`build_net_deny_bpf`]. On the host (where the runner cannot jail anyway)
/// it returns an empty program — the host `run_jailed` reports an enforcement
/// failure before the BPF would ever be consulted.
#[must_use]
pub fn net_deny_bpf_for_target() -> Vec<BpfInsn> {
    #[cfg(not(target_os = "android"))]
    {
        Vec::new()
    }
    #[cfg(target_os = "android")]
    {
        build_net_deny_bpf(&target_socket_nrs(), target_audit_arch())
    }
}

/// The per-arch socket-family syscall numbers for the current Android target.
/// Resolved from `libc::SYS_*` so they track the device ABI exactly.
#[cfg(target_os = "android")]
fn target_socket_nrs() -> Vec<u32> {
    // The socket-family syscalls. Cast each `libc::SYS_*` (c_long) to u32 — all
    // syscall numbers fit in u32. Names mirror NET_DENY_SYSCALL_NAMES.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let nrs: Vec<u32> = vec![
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_bind,
        libc::SYS_connect,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_shutdown,
    ]
    .into_iter()
    .map(|n| n as u32)
    .collect();
    nrs
}

/// The `AUDIT_ARCH_*` tag for the current Android target — used by the BPF
/// arch guard. `libc` does not export `AUDIT_ARCH_*`, so the constants are
/// reproduced from `<linux/audit.h>` (stable kernel ABI).
#[cfg(target_os = "android")]
fn target_audit_arch() -> u32 {
    // AUDIT_ARCH = arch number | __AUDIT_ARCH_64BIT(0x8000_0000) |
    // __AUDIT_ARCH_LE(0x4000_0000). EM_AARCH64=183(0xB7), EM_X86_64=62(0x3E).
    #[cfg(target_arch = "aarch64")]
    {
        0xC000_00B7
    }
    #[cfg(target_arch = "x86_64")]
    {
        0xC000_003E
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        // 32-bit Android targets are out of scope for v1; the arch guard would
        // KILL anyway. Return 0 so a wrong-arch invocation is denied.
        0
    }
}

/// Build the net-deny classic-BPF program for one architecture.
///
/// Shape (one JEQ per syscall, jumping to a shared EPERM ret):
/// ```text
///   [0]  LD arch
///   [1]  JEQ audit_arch, jt=1, jf=0   → match: skip KILL; mismatch: fall to KILL
///   [2]  RET KILL                      ← arch-mismatch guard
///   [3]  LD nr
///   [4]  JEQ socket_nrs[0], jt=(n-1), jf=0
///   [5]  JEQ socket_nrs[1], jt=(n-2), jf=0
///   ...
///   [4+n-1] JEQ socket_nrs[n-1], jt=0, jf=0
///   [4+n]   RET ERRNO(EPERM)           ← any matched nr lands here
///   [4+n+1] RET ALLOW                  ← default
/// ```
///
/// Jump arithmetic (one-JEQ-per-syscall shape, jumping forward to ERRNO ret):
/// At insn index `4 + i` (0-based among the JEQ chain), the EPERM ret sits at
/// index `4 + n`. Distance from insn `4+i` to the EPERM insn is `n - i`
/// instructions ahead. Because `jt` is the offset AFTER the JEQ executes (pc
/// advances past the JEQ first), `jt = (n - i) - 1 = n - 1 - i`.
/// A non-matching JEQ takes `jf=0` and falls to the very next comparison.
///
/// `socket_nrs` and `audit_arch` are arch-specific and passed in by the
/// on-device caller (`libc::SYS_*`, `AUDIT_ARCH_*`) so this stays
/// host-testable.
#[must_use]
pub fn build_net_deny_bpf(socket_nrs: &[u32], audit_arch: u32) -> Vec<BpfInsn> {
    // Fixed preamble: 4 insns.
    // [0] Load arch from seccomp_data (offset 4).
    // [1] If arch == audit_arch, jump over the KILL (jt=1 skips insn [2]);
    //     otherwise fall to [2] (jf=0 → next insn).
    // [2] Arch mismatch guard — KILL_PROCESS (not EPERM: a wrong-arch
    //     invocation is not a graceful deny, it indicates a kernel/process
    //     mismatch; kill the whole process — not just the thread — so the
    //     AArch32 compat ABI can never be used to slip past net-deny).
    // [3] Load syscall nr from seccomp_data (offset 0).
    let mut prog = vec![
        BpfInsn {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_ARCH_OFF,
        },
        BpfInsn {
            code: BPF_JEQ_K,
            jt: 1,
            jf: 0,
            k: audit_arch,
        },
        BpfInsn {
            code: BPF_RET_K,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_KILL_PROCESS,
        },
        BpfInsn {
            code: BPF_LD_W_ABS,
            jt: 0,
            jf: 0,
            k: SECCOMP_DATA_NR_OFF,
        },
    ];

    // [4 .. 4+n-1] One JEQ per socket syscall nr.
    // Layout after the LD-nr insn at [3]:
    //   [4+i]   JEQ k=socket_nrs[i], jt=<to EPERM>, jf=<to next or ALLOW>
    //   [4+n]   RET EPERM
    //   [4+n+1] RET ALLOW
    //
    // jt (match): distance to EPERM from the insn AFTER this JEQ.
    //   insn absolute index = 4 + i
    //   EPERM absolute index = 4 + n
    //   jt = (4 + n) - (4 + i) - 1 = n - i - 1
    //
    // jf (no match): for all but the last, fall through to the next JEQ (jf=0).
    // For the last JEQ (i == n-1): jf=0 would land on EPERM (wrong); we need
    // jf=1 to skip over EPERM and reach ALLOW.
    let n = socket_nrs.len();
    for (i, &nr) in socket_nrs.iter().enumerate() {
        // Safety: n <= 255 is a caller precondition (BPF jump offsets are u8);
        // syscall lists are short (< 20 nrs in practice).
        #[allow(clippy::cast_possible_truncation)]
        let jt = (n - i - 1) as u8;
        let jf = u8::from(i == n - 1);
        prog.push(BpfInsn {
            code: BPF_JEQ_K,
            jt,
            jf,
            k: nr,
        });
    }

    // [4+n] EPERM return — reached by any matched socket syscall.
    prog.push(BpfInsn {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: seccomp_ret_errno(1),
    });
    // [4+n+1] Default ALLOW — reached by all unmatched syscalls.
    prog.push(BpfInsn {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });

    prog
}

/// Hex SHA-256 over the serialized program (each insn as little-endian
/// `code|jt|jf|k`) — for the receipt's `SeccompRef.hash`.
#[must_use]
pub fn net_deny_bpf_hash(socket_nrs: &[u32], audit_arch: u32) -> String {
    let prog = build_net_deny_bpf(socket_nrs, audit_arch);
    let mut h = Sha256::new();
    for insn in &prog {
        h.update(insn.code.to_le_bytes());
        h.update([insn.jt, insn.jf]);
        h.update(insn.k.to_le_bytes());
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    // arm64 socket-family nrs (sample for the host test; on-device the caller
    // passes libc::SYS_* — see `socket_syscall_nrs()` in run.rs).
    const ARM64_SOCKET_NRS: &[u32] = &[198, 199, 200, 201, 202, 203, 206, 207, 212];
    const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;

    #[test]
    fn bpf_validates_arch_loads_nr_and_denies_then_allows() {
        let prog = build_net_deny_bpf(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        // First insns load+check arch (offset 4 in seccomp_data), then load nr
        // (offset 0). Last insn is the default RET ALLOW.
        assert!(
            prog.len() >= ARM64_SOCKET_NRS.len() + 4,
            "arch+nr+jumps+rets"
        );
        let last = prog.last().copied().unwrap();
        assert_eq!(last.code, BPF_RET_K, "default action is the final insn");
        assert_eq!(last.k, SECCOMP_RET_ALLOW, "default = ALLOW");
        // Exactly one ERRNO(EPERM) return present.
        assert!(
            prog.iter()
                .any(|i| i.code == BPF_RET_K && i.k == seccomp_ret_errno(1)),
            "denied syscalls return EPERM(1)"
        );
        // A RET KILL_PROCESS appears ONLY in the arch-mismatch guard (exactly
        // once), not as the net-deny action — net denial is graceful EPERM, not
        // fatal. The guard kills the whole process (not just the thread).
        let kill_count = prog
            .iter()
            .filter(|i| i.code == BPF_RET_K && i.k == SECCOMP_RET_KILL_PROCESS)
            .count();
        assert_eq!(
            kill_count, 1,
            "exactly one KILL_PROCESS insn (arch-mismatch guard)"
        );
        // And no legacy bare KILL_THREAD return is emitted.
        assert!(
            !prog
                .iter()
                .any(|i| i.code == BPF_RET_K && i.k == SECCOMP_RET_KILL),
            "arch guard must use KILL_PROCESS, not KILL_THREAD"
        );
    }

    #[test]
    fn bpf_hash_is_stable_hex_and_arch_sensitive() {
        let a = net_deny_bpf_hash(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        let b = net_deny_bpf_hash(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Different arch ⇒ different program ⇒ different hash.
        let x86 = net_deny_bpf_hash(&[41, 42, 43, 49, 50], 0xC000_003E);
        assert_ne!(a, x86);
    }

    #[test]
    fn policy_name_is_versioned() {
        assert_eq!(net_deny_policy_name(), "net-deny-v1");
    }

    #[test]
    fn policy_identity_hash_is_stable_arch_independent_hex() {
        let a = net_deny_policy_identity_hash();
        let b = net_deny_policy_identity_hash();
        // Stable across calls and a 64-char lowercase hex digest.
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Unlike net_deny_bpf_hash, it does NOT depend on per-arch numbers —
        // it is the policy DEFINITION identity, the same on every device.
        let arm = net_deny_bpf_hash(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        assert_ne!(a, arm, "identity hash is the policy def, not the BPF bytes");
    }

    #[test]
    fn bpf_for_target_is_empty_on_host() {
        // The host runner path never jails, so the per-target BPF is empty.
        assert!(net_deny_bpf_for_target().is_empty());
    }

    /// Simulate BPF execution for a given syscall number.
    ///
    /// Returns the `k` field of the first `BPF_RET_K` reached when executing
    /// the program as if `arch == audit_arch` and `nr == syscall_nr`. This
    /// lets us hand-trace the jump arithmetic without an actual BPF VM.
    ///
    /// Simplified model: handles only `BPF_LD_W_ABS`, `BPF_JEQ_K`,
    /// `BPF_RET_K` — exactly the opcodes `build_net_deny_bpf` emits.
    fn simulate(prog: &[BpfInsn], syscall_nr: u32, audit_arch: u32) -> u32 {
        let mut pc: usize = 0;
        // The accumulator register A — tracks the last value loaded by BPF_LD_W_ABS.
        let mut acc: u32 = 0;
        loop {
            assert!(pc < prog.len(), "BPF program ran off the end at pc={pc}");
            let insn = prog[pc];
            match insn.code {
                BPF_LD_W_ABS => {
                    // Load from seccomp_data at byte offset `k`.
                    acc = if insn.k == SECCOMP_DATA_ARCH_OFF {
                        audit_arch
                    } else if insn.k == SECCOMP_DATA_NR_OFF {
                        syscall_nr
                    } else {
                        panic!("unexpected LD offset {:#x}", insn.k);
                    };
                    pc += 1;
                }
                BPF_JEQ_K => {
                    // Jump-true/false are offsets relative to the NEXT insn.
                    if acc == insn.k {
                        pc += 1 + insn.jt as usize;
                    } else {
                        pc += 1 + insn.jf as usize;
                    }
                }
                BPF_RET_K => return insn.k,
                _ => panic!("unexpected opcode {:#x} at pc {}", insn.code, pc),
            }
        }
    }

    #[test]
    fn simulate_matched_nr_reaches_eperm() {
        let prog = build_net_deny_bpf(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        // Each socket nr should return EPERM.
        for nr in ARM64_SOCKET_NRS {
            let ret = simulate(&prog, *nr, AUDIT_ARCH_AARCH64);
            assert_eq!(
                ret,
                seccomp_ret_errno(1),
                "nr={nr} should reach EPERM, got {ret:#x}"
            );
        }
    }

    #[test]
    fn simulate_unmatched_nr_reaches_allow() {
        let prog = build_net_deny_bpf(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        // A clearly non-socket nr (read=63, write=64, openat=56 on arm64)
        // should fall through to ALLOW.
        for nr in [56_u32, 63, 64, 1000] {
            let ret = simulate(&prog, nr, AUDIT_ARCH_AARCH64);
            assert_eq!(
                ret, SECCOMP_RET_ALLOW,
                "nr={nr} should reach ALLOW, got {ret:#x}"
            );
        }
    }

    #[test]
    fn simulate_wrong_arch_reaches_kill() {
        let prog = build_net_deny_bpf(ARM64_SOCKET_NRS, AUDIT_ARCH_AARCH64);
        // If the arch tag is wrong, the arch guard fires.
        let ret = simulate(&prog, 63, 0xC000_003E /* x86_64 AUDIT_ARCH */);
        assert_eq!(
            ret, SECCOMP_RET_KILL_PROCESS,
            "wrong arch must reach KILL_PROCESS"
        );
    }
}
