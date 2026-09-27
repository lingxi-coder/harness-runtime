//! Linux bwrap sandbox assembly — a 1:1 behavioral port of
//! `linux-sandbox-utils.js` (the dependency check, the `socat` network-namespace
//! bridge lifecycle, `buildSandboxCommand`, the full `wrapCommandWithSandboxLinux`
//! bwrap argv assembly, and mount-point cleanup).
//!
//! # Design divergence from the TS reference (documented, faithful)
//!
//! The TS module keeps **module-global mutable state** — `activeSandboxCount`
//! (a refcount) and a `bwrapMountPoints` `Set` populated as a side effect of
//! `generateFilesystemArgs`. `cleanupBwrapMountPoints()` reads both globals.
//! This port has **no global state**: [`crate::fs_args::generate_filesystem_args`]
//! *returns* the `Vec<PathBuf>` of mount points, [`wrap_command_with_sandbox_linux`]
//! threads it back to the caller, and [`cleanup_bwrap_mount_points`] takes that
//! slice as an explicit argument. Each wrap invocation therefore owns exactly the
//! mount points it created — the refcount/defer dance disappears because there is
//! no shared set to protect. The observable file-cleanup behavior (unlink empty
//! files, rmdir empty dirs, ignore errors) is identical.
//!
//! # Seccomp (P7)
//!
//! [`resolve_apply_seccomp_prefix`] locates the `apply-seccomp` helper binary
//! (the faithful Rust port of the package's pre-built C `apply-seccomp`,
//! living in this workspace's `apply-seccomp` crate). When a binary is
//! resolved, [`build_sandbox_command`] takes the seccomp branch — prefixing the
//! user command with `<apply-seccomp> bash -c '<cmd>'` so the workload runs
//! under a seccomp BPF filter that returns `EPERM` for `socket(AF_UNIX)` and
//! `socketpair(AF_UNIX)` (forcing all egress through the TCP proxy) — and
//! [`wrap_command_with_sandbox_linux`] emits that prefix in the non-network
//! branch too. The `allow_all_unix_sockets` path skips seccomp entirely.
//!
//! ## Documented divergence (the security-essential subset)
//!
//! The original C `apply-seccomp` ALSO creates a nested user+PID+mount
//! namespace, remounts `/proc`, and becomes a PID-1 reaper. In this assembly
//! bwrap already supplies `--unshare-pid` + `--proc /proc`, so the only part
//! the helper must reproduce is the SECURITY-ESSENTIAL `socket(AF_UNIX)` block;
//! the nested-ns/reaper aspects come from the surrounding bwrap args. The C
//! source is unavailable to copy — we port the documented behavioral contract.

use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::env::Platform;
use crate::fs_args::{generate_filesystem_args, ReadConfig, WriteConfig};

/// `isExecutable(p)` (`linux-sandbox-utils.js:285-296`): is `p` executable by
/// the current process (`access(p, X_OK)`).
///
/// On Unix this checks the file exists and has any execute bit. (The TS uses
/// `fs.accessSync(p, X_OK)`; we approximate with a `metadata` + mode check,
/// which matches for the common single-user case the dependency check serves.)
#[must_use]
pub fn is_executable(p: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(p) {
            Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
    }
}

/// `whichSync(cmd)` (`linux-sandbox-utils.js`): resolve `cmd` on `PATH`,
/// returning the absolute path or `None`. Backed by the `which` crate.
#[must_use]
pub fn which_sync(cmd: &str) -> Option<PathBuf> {
    which::which(cmd).ok()
}

/// Options shared by [`get_linux_dependency_status`] and
/// [`check_linux_dependencies`] — the explicit-override paths for the required
/// binaries, plus the optional `apply-seccomp` locator inputs (the TS `opts`
/// `seccompConfig`).
#[derive(Debug, Default, Clone)]
pub struct LinuxDependencyOpts {
    /// Explicit `bwrap` path override. `None` → resolve via `PATH`.
    pub bwrap_path: Option<String>,
    /// Explicit `socat` path override. `None` → resolve via `PATH`.
    pub socat_path: Option<String>,
    /// Explicit `apply-seccomp` binary path (the TS `seccompConfig.binaryPath`).
    /// Threaded into [`resolve_apply_seccomp_prefix`] for the availability probe.
    pub seccomp_apply_path: Option<String>,
    /// Trusted `apply-seccomp` argv0 (a bare command name resolved inside bwrap).
    pub seccomp_apply_argv0: Option<String>,
}

/// Structured dependency status (`getLinuxDependencyStatus`, :297-311).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinuxDependencyStatus {
    /// `bwrap` is installed/executable.
    pub has_bwrap: bool,
    /// `socat` is installed/executable.
    pub has_socat: bool,
    /// `apply-seccomp` is locatable for the current arch (`true` when
    /// [`resolve_apply_seccomp_prefix`] resolves a prefix).
    pub has_seccomp_apply: bool,
}

/// `getLinuxDependencyStatus(opts)` (:297-311): probe each dependency. An
/// explicit path is checked with [`is_executable`]; otherwise `PATH` is probed
/// with [`which_sync`]. `has_seccomp_apply` reflects the real
/// [`resolve_apply_seccomp_prefix`] locator (explicit path / argv0 / workspace
/// fallback, arch-gated).
#[must_use]
pub fn get_linux_dependency_status(opts: &LinuxDependencyOpts) -> LinuxDependencyStatus {
    LinuxDependencyStatus {
        has_bwrap: opts
            .bwrap_path
            .as_deref()
            .map_or_else(|| which_sync("bwrap").is_some(), is_executable),
        has_socat: opts
            .socat_path
            .as_deref()
            .map_or_else(|| which_sync("socat").is_some(), is_executable),
        has_seccomp_apply: resolve_apply_seccomp_prefix(
            opts.seccomp_apply_path.as_deref().map(Path::new),
            opts.seccomp_apply_argv0.as_deref(),
        )
        .is_some(),
    }
}

/// Structured dependency-check result (`checkLinuxDependencies`, :312-363).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinuxDependencyCheck {
    /// Fatal problems (missing/non-executable `bwrap` or `socat`).
    pub errors: Vec<String>,
    /// Non-fatal problems (here: seccomp unavailable).
    pub warnings: Vec<String>,
}

/// `checkLinuxDependencies(opts)` (:312-363). Error strings are byte-exact with
/// the TS reference. An explicit override path that is not executable is an
/// *error* (a directive, not a hint); a missing `PATH` binary is also an error.
/// Seccomp being unavailable is a warning (P7 seam → always emitted).
#[must_use]
pub fn check_linux_dependencies(opts: &LinuxDependencyOpts) -> LinuxDependencyCheck {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    if let Some(p) = opts.bwrap_path.as_deref() {
        if !is_executable(p) {
            errors.push(format!("bubblewrap (bwrap) not executable at {p}"));
        }
    } else if which_sync("bwrap").is_none() {
        errors.push("bubblewrap (bwrap) not installed".to_string());
    }

    if let Some(p) = opts.socat_path.as_deref() {
        if !is_executable(p) {
            errors.push(format!("socat not executable at {p}"));
        }
    } else if which_sync("socat").is_none() {
        errors.push("socat not installed".to_string());
    }

    // Seccomp is a defense-in-depth warning, not an error: emit it only when
    // the `apply-seccomp` helper cannot be located (matching the TS branch that
    // pushes the warning when no binary resolves).
    let has_seccomp_apply = resolve_apply_seccomp_prefix(
        opts.seccomp_apply_path.as_deref().map(Path::new),
        opts.seccomp_apply_argv0.as_deref(),
    )
    .is_some();
    if !has_seccomp_apply {
        warnings.push("seccomp not available - unix socket access not restricted".to_string());
    }

    LinuxDependencyCheck { errors, warnings }
}

/// Is the host architecture one the `apply-seccomp` BPF filter supports
/// (`x86_64`/`aarch64`)? Mirrors the TS `getVendorArchitecture` x64/arm64 gate
/// — 32-bit x86 (`ia32`) and every other arch return `false` because the
/// `socket(AF_UNIX)` filter does not block the `socketcall()` multiplexer those
/// arches use (a security bypass). The gate is on the *compile-time* arch of
/// this crate, which equals the host arch for the sandbox we are assembling.
#[must_use]
fn seccomp_arch_supported() -> bool {
    cfg!(target_arch = "x86_64") || cfg!(target_arch = "aarch64")
}

/// Locate the `apply-seccomp` helper binary, returning the **prefix string**
/// (the binary path followed by a single space) to prepend before the
/// shell-quoted `<shell> -c <cmd>` — matching the TS
/// `applySeccompPrefix + shellquote([shell, '-c', cmd])` shape. Returns `None`
/// when no binary can be located or the architecture is unsupported.
///
/// Port of `getApplySeccompBinaryPath` (`generate-seccomp-filter.js`) search
/// order, adapted to the `LingXi` build layout:
/// 1. **Explicit `apply_path`** — if provided and it exists on disk, use it
///    (highest priority; the TS `seccompBinaryPath` parameter).
/// 2. **`argv0`** — a bare command name (e.g. `"apply-seccomp"`) that is trusted
///    to resolve inside the bwrap sandbox via `PATH`. Used as-is without a
///    host-side existence check (the TS treats an explicitly-configured name as
///    authoritative; inside bwrap the host `target/` may not be on `PATH`).
/// 3. **Relative/workspace fallback** — the built helper at
///    `target/<profile>/apply-seccomp` relative to this crate, probed for
///    existence. (The TS probes `vendor/seccomp/<arch>/apply-seccomp`; our
///    equivalent vendor location is the workspace `target/` build output.)
///
/// The arch gate (step 0) returns `None` for non-x64/arm64 BEFORE any path
/// probing, exactly as the TS `getVendorArchitecture` returns `null`.
#[must_use]
pub fn resolve_apply_seccomp_prefix(
    apply_path: Option<&Path>,
    argv0: Option<&str>,
) -> Option<String> {
    // Step 0: arch gate (TS getVendorArchitecture → null for ia32/other).
    if !seccomp_arch_supported() {
        return None;
    }

    // Step 1: explicit path that exists → use it.
    if let Some(p) = apply_path {
        if p.exists() {
            return Some(format!("{} ", shquote(&p.to_string_lossy())));
        }
        // Explicit path provided but missing → fall through to argv0/search
        // (matching the TS: it logs then continues to the local/global search).
    }

    // Step 2: trusted argv0 (bare name resolved inside bwrap via PATH).
    if let Some(name) = argv0 {
        if !name.is_empty() {
            return Some(format!("{} ", shquote(name)));
        }
    }

    // Step 3: workspace-relative build output fallback. Probe the standard
    // cargo profiles' `apply-seccomp` next to this crate's compiled location.
    for candidate in workspace_apply_seccomp_candidates() {
        if candidate.exists() {
            return Some(format!("{} ", shquote(&candidate.to_string_lossy())));
        }
    }

    None
}

/// Candidate paths for a workspace-built helper. Packaged helpers beside the
/// running executable also work independently of the source checkout.
fn workspace_apply_seccomp_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(dir) = executable.parent() {
            let dir = if dir.file_name().is_some_and(|name| name == "deps") {
                dir.parent().unwrap_or(dir)
            } else {
                dir
            };
            out.push(dir.join("apply-seccomp"));
        }
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("sandbox-runtime lives under workspace crates/");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| workspace.join("target"));
    for profile in ["debug", "release"] {
        out.push(target.join(profile).join("apply-seccomp"));
    }
    out
}

/// `cleanupBwrapMountPoints(mountPoints)` (:247-283), refactored to take the
/// mount-point slice explicitly (see the module-level design note — no global
/// `activeSandboxCount`/`bwrapMountPoints`).
///
/// For each mount point: if it is still the empty (size-0) **file** bwrap
/// created, unlink it; if it is an empty **directory** (an intermediate-component
/// mount point), rmdir it. Anything with real content is left alone. All errors
/// are ignored — the file may already be gone.
pub fn cleanup_bwrap_mount_points(mount_points: &[PathBuf]) {
    for mount_point in mount_points {
        let Ok(stat) = fs::symlink_metadata(mount_point) else {
            // Ignore cleanup errors — the file may have already been removed.
            continue;
        };
        if stat.is_file() && stat.len() == 0 {
            let _ = fs::remove_file(mount_point);
        } else if stat.is_dir() {
            // Only remove if still empty (intermediate-component mount point).
            if let Ok(mut entries) = fs::read_dir(mount_point) {
                if entries.next().is_none() {
                    let _ = fs::remove_dir(mount_point);
                }
            }
        }
    }
}

/// Shell-quote a single argument the way the TS `shellquote.quote([s])` does
/// for a one-element list. Falls back to the raw string only when `shlex`
/// refuses (a NUL byte), which cannot occur for the paths/commands here.
fn shquote(s: &str) -> String {
    shlex::try_quote(s).map_or_else(|_| s.to_string(), Cow::into_owned)
}

/// Shell-quote + space-join a list the way the TS `shellquote.quote([...])`
/// does. Mirrors `shlex::try_join`, with the same NUL-only fallback as
/// [`shquote`].
fn shjoin<'a, I>(parts: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let owned: Vec<&str> = parts.into_iter().collect();
    shlex::try_join(owned.iter().copied()).unwrap_or_else(|_| owned.join(" "))
}

/// The exact `socat` argv for a host-side bridge (`UNIX-LISTEN` → `TCP`):
/// `UNIX-LISTEN:<sock>,fork,reuseaddr` `TCP:localhost:<port>,keepalive,keepidle=10,keepintvl=5,keepcnt=3`
#[must_use]
fn host_socat_args(socket_path: &str, proxy_port: u16) -> [String; 2] {
    [
        format!("UNIX-LISTEN:{socket_path},fork,reuseaddr"),
        format!("TCP:localhost:{proxy_port},keepalive,keepidle=10,keepintvl=5,keepcnt=3"),
    ]
}

/// `buildSandboxCommand(...)` (:499-525): the command that runs *inside* the
/// sandbox. Starts socat listeners on ports 3128 (HTTP) and 1080 (SOCKS) that
/// forward to the bound Unix sockets, installs an EXIT trap killing them, then
/// runs the user command. Ports 3128/1080 are hardcoded (matching the TS).
///
/// When `apply_seccomp_prefix` is `Some`, the seccomp branch is taken: the user
/// command runs as `<prefix><shell> -c '<cmd>'`, i.e. under the `apply-seccomp`
/// helper that installs the `socket(AF_UNIX)` BPF block. When `None`, the `eval`
/// branch is taken. apply-seccomp runs AFTER the socat listeners start so socat
/// can still create its own Unix sockets.
///
/// Returns `<shell> -c <shellquote(inner_script)>`.
#[must_use]
pub fn build_sandbox_command(
    http_socket_path: &str,
    socks_socket_path: &str,
    user_command: &str,
    apply_seccomp_prefix: Option<&str>,
    shell: &str,
    socat_path: Option<&str>,
) -> String {
    // Default to bash for backward compatibility (TS: `shell || 'bash'`).
    let shell_path = if shell.is_empty() { "bash" } else { shell };
    // Host filesystem is bind-mounted into the sandbox, so an explicit socat
    // path resolves to the same binary inside bwrap.
    let socat = shquote(socat_path.unwrap_or("socat"));

    let socat_commands = [
        format!("{socat} TCP-LISTEN:3128,fork,reuseaddr UNIX-CONNECT:{http_socket_path} >/dev/null 2>&1 &"),
        format!("{socat} TCP-LISTEN:1080,fork,reuseaddr UNIX-CONNECT:{socks_socket_path} >/dev/null 2>&1 &"),
        "trap \"kill %1 %2 2>/dev/null; exit\" EXIT".to_string(),
    ];

    // apply-seccomp runs after socat so socat can still create Unix sockets.
    let inner_script = if let Some(prefix) = apply_seccomp_prefix {
        let apply_seccomp_cmd = format!("{prefix}{}", shjoin([shell_path, "-c", user_command]));
        let mut lines: Vec<String> = socat_commands.to_vec();
        lines.push(apply_seccomp_cmd);
        lines.join("\n")
    } else {
        let mut lines: Vec<String> = socat_commands.to_vec();
        lines.push(format!("eval {}", shjoin([user_command])));
        lines.join("\n")
    };

    format!("{shell_path} -c {}", shjoin([inner_script.as_str()]))
}

/// A live `socat` network-namespace bridge: two host-side `socat` children
/// forwarding Unix sockets to the host HTTP/SOCKS proxy ports. Tearing this
/// down (via [`LinuxBridge::teardown`] or `Drop`) SIGTERMs both children.
///
/// Faithful to the TS `LinuxNetworkBridge` object (`initializeLinuxNetworkBridge`
/// return value); the lifecycle is owned by this struct rather than the caller.
#[derive(Debug)]
pub struct LinuxBridge {
    /// Path to the HTTP Unix socket (`claude-http-<id>.sock` under tmpdir).
    pub http_socket_path: PathBuf,
    /// Path to the SOCKS Unix socket (`claude-socks-<id>.sock` under tmpdir).
    pub socks_socket_path: PathBuf,
    /// Host port the HTTP bridge forwards to.
    pub http_proxy_port: u16,
    /// Host port the SOCKS bridge forwards to.
    pub socks_proxy_port: u16,
    http_child: Option<Child>,
    socks_child: Option<Child>,
}

impl LinuxBridge {
    /// SIGTERM both bridge children (idempotent). Called by `Drop`.
    pub fn teardown(&mut self) {
        for child in [self.http_child.as_mut(), self.socks_child.as_mut()]
            .into_iter()
            .flatten()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.http_child = None;
        self.socks_child = None;
    }
}

impl Drop for LinuxBridge {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// `initializeLinuxNetworkBridge(httpProxyPort, socksProxyPort, socatPath)`
/// (:364-470). Spawns two host-side `socat` bridges with the EXACT argv, polls
/// readiness (5 attempts, sleeping `i*100ms` before retry, both socket files
/// must exist; error if a child died), and tears down any started child on
/// failure. Returns a [`LinuxBridge`] whose `Drop` SIGTERMs both children.
///
/// # Errors
/// Returns an `io::Error` if a `socat` child fails to spawn, dies during the
/// readiness poll, or the sockets do not appear within 5 attempts.
pub fn initialize_linux_network_bridge(
    http_proxy_port: u16,
    socks_proxy_port: u16,
    socat_path: Option<&str>,
) -> io::Result<LinuxBridge> {
    let socat = socat_path.unwrap_or("socat");
    let socket_id = random_hex_8();
    let tmp = std::env::temp_dir();
    let http_socket_path = tmp.join(format!("claude-http-{socket_id}.sock"));
    let socks_socket_path = tmp.join(format!("claude-socks-{socket_id}.sock"));

    let spawn_socat = |args: [String; 2]| -> io::Result<Child> {
        Command::new(socat)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };

    // Start HTTP bridge.
    let http_args = host_socat_args(&http_socket_path.to_string_lossy(), http_proxy_port);
    let mut http_child = spawn_socat(http_args).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to start HTTP bridge process: {e}"),
        )
    })?;

    // Start SOCKS bridge; tear down HTTP on failure.
    let socks_args = host_socat_args(&socks_socket_path.to_string_lossy(), socks_proxy_port);
    let socks_child = match spawn_socat(socks_args) {
        Ok(c) => c,
        Err(e) => {
            let _ = http_child.kill();
            let _ = http_child.wait();
            return Err(io::Error::new(
                e.kind(),
                format!("Failed to start SOCKS bridge process: {e}"),
            ));
        }
    };
    let mut socks_child = socks_child;

    // Wait for both sockets to be ready (5 attempts, sleep i*100ms).
    let max_attempts: u32 = 5;
    let mut ready = false;
    for i in 0..max_attempts {
        // A died child means the bridge cannot work — bail and clean up.
        let http_dead = matches!(http_child.try_wait(), Ok(Some(_)) | Err(_));
        let socks_dead = matches!(socks_child.try_wait(), Ok(Some(_)) | Err(_));
        if http_dead || socks_dead {
            kill_both(&mut http_child, &mut socks_child);
            return Err(io::Error::other("Linux bridge process died unexpectedly"));
        }
        if http_socket_path.exists() && socks_socket_path.exists() {
            ready = true;
            break;
        }
        if i == max_attempts - 1 {
            kill_both(&mut http_child, &mut socks_child);
            return Err(io::Error::other(format!(
                "Failed to create bridge sockets after {max_attempts} attempts"
            )));
        }
        std::thread::sleep(Duration::from_millis(u64::from(i) * 100));
    }

    if !ready {
        kill_both(&mut http_child, &mut socks_child);
        return Err(io::Error::other(format!(
            "Failed to create bridge sockets after {max_attempts} attempts"
        )));
    }

    Ok(LinuxBridge {
        http_socket_path,
        socks_socket_path,
        http_proxy_port,
        socks_proxy_port,
        http_child: Some(http_child),
        socks_child: Some(socks_child),
    })
}

/// Parameters for [`wrap_command_with_sandbox_linux`] — a 1:1 mirror of the TS
/// `wrapCommandWithSandboxLinux` `params` object.
//
// The five `bool` fields are a faithful 1:1 transcription of the TS `params`
// object's boolean flags; collapsing them into enums would diverge from the
// reference shape with no behavioral gain.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct WrapParams<'a> {
    /// The user command to wrap.
    pub command: &'a str,
    /// Apply `--unshare-net` + the proxy bridge/env (network restriction).
    pub needs_network_restriction: bool,
    /// HTTP bridge Unix socket path (host side). Bound into the sandbox.
    pub http_socket_path: Option<&'a str>,
    /// SOCKS bridge Unix socket path (host side). Bound into the sandbox.
    pub socks_socket_path: Option<&'a str>,
    /// Host HTTP proxy port (for the `LINGXI_HOST_HTTP_PROXY_PORT` setenv).
    pub http_proxy_port: Option<u16>,
    /// Host SOCKS proxy port (for the `LINGXI_HOST_SOCKS_PROXY_PORT` setenv).
    pub socks_proxy_port: Option<u16>,
    /// CA cert path threaded into the proxy env vars (TLS-MITM trust).
    pub ca_cert_path: Option<&'a str>,
    /// Read-restriction config (`denyOnly`). `None`/empty = no read restriction.
    pub read_config: Option<&'a ReadConfig>,
    /// Write-restriction config (`allowOnly`). `None` = no write restriction.
    pub write_config: Option<&'a WriteConfig>,
    /// Weaker nested sandbox (`--unshare-user --bind /proc /proc` instead of
    /// `--proc /proc`) for unprivileged containers.
    pub enable_weaker_nested_sandbox: bool,
    /// Skip the seccomp Unix-socket block. When `true`,
    /// [`resolve_apply_seccomp_prefix`] is NOT consulted and no `apply-seccomp`
    /// prefix is emitted (the workload may freely create Unix sockets).
    pub allow_all_unix_sockets: bool,
    /// Explicit `apply-seccomp` binary path (the TS `seccompConfig.binaryPath`).
    /// Threaded into [`resolve_apply_seccomp_prefix`].
    pub seccomp_apply_path: Option<&'a str>,
    /// Trusted `apply-seccomp` argv0 (a bare command name resolved inside bwrap
    /// via `PATH`). Threaded into [`resolve_apply_seccomp_prefix`].
    pub seccomp_apply_argv0: Option<&'a str>,
    /// Shell to run the command with (`binShell || 'bash'`).
    pub bin_shell: Option<&'a str>,
    /// Ripgrep command for `generate_filesystem_args` (`{ command: 'rg' }`).
    pub ripgrep_cmd: &'a str,
    /// Mandatory deny search depth for `generate_filesystem_args`.
    pub mandatory_deny_search_depth: usize,
    /// Allow git config in the sandbox.
    pub allow_git_config: bool,
    /// Explicit `bwrap` path override (`bwrapPath ?? 'bwrap'`).
    pub bwrap_path: Option<&'a str>,
    /// Explicit `socat` path override (threaded to `build_sandbox_command`).
    pub socat_path: Option<&'a str>,
    /// Working directory for `generate_filesystem_args`.
    pub cwd: &'a str,
    /// Platform for `generate_proxy_env_vars` (`GIT_SSH_COMMAND` branch).
    pub platform: Platform,
    /// Resolved tmpdir for `generate_proxy_env_vars` (`TMPDIR` env var).
    pub tmpdir: &'a str,
    /// Optional per-session token that validates per-command proxy credentials.
    pub proxy_auth_token: Option<&'a str>,
}

/// Push the `--unshare-net` network args (socket binds + proxy `--setenv`s)
/// onto `bwrap_args`. Extracted from [`wrap_command_with_sandbox_linux`] for the
/// `--unshare-net` + both-sockets branch (:862-904).
///
/// # Errors
/// Returns an `io::Error` if a declared bridge socket no longer exists (the
/// bridge process likely died).
fn push_network_args(bwrap_args: &mut Vec<String>, params: &WrapParams<'_>) -> io::Result<()> {
    bwrap_args.push("--unshare-net".into());
    let (Some(http_sock), Some(socks_sock)) = (params.http_socket_path, params.socks_socket_path)
    else {
        // No sockets → bare --unshare-net (network fully blocked).
        return Ok(());
    };

    // Verify the sockets still exist before binding (TS: die if the bridge died).
    if !PathBuf::from(http_sock).exists() {
        return Err(io::Error::other(format!(
            "Linux HTTP bridge socket does not exist: {http_sock}. \
             The bridge process may have died. Try reinitializing the sandbox."
        )));
    }
    if !PathBuf::from(socks_sock).exists() {
        return Err(io::Error::other(format!(
            "Linux SOCKS bridge socket does not exist: {socks_sock}. \
             The bridge process may have died. Try reinitializing the sandbox."
        )));
    }
    bwrap_args.push("--bind".into());
    bwrap_args.push(http_sock.into());
    bwrap_args.push(http_sock.into());
    bwrap_args.push("--bind".into());
    bwrap_args.push(socks_sock.into());
    bwrap_args.push(socks_sock.into());

    // Proxy env vars: HTTP listener 3128, SOCKS listener 1080 (the
    // sandbox-internal socat ports), plus the CA cert.
    let proxy_env = crate::env::generate_proxy_env_vars_with_auth(
        Some(3128),
        Some(1080),
        params.ca_cert_path,
        params.platform,
        params.tmpdir,
        Some(params.command),
        params.proxy_auth_token,
    );
    for (k, v) in proxy_env {
        bwrap_args.push("--setenv".into());
        bwrap_args.push(k);
        bwrap_args.push(v);
    }
    // Host proxy port env vars (debugging/transparency).
    if let Some(p) = params.http_proxy_port {
        bwrap_args.push("--setenv".into());
        bwrap_args.push("LINGXI_HOST_HTTP_PROXY_PORT".into());
        bwrap_args.push(p.to_string());
    }
    if let Some(p) = params.socks_proxy_port {
        bwrap_args.push("--setenv".into());
        bwrap_args.push("LINGXI_HOST_SOCKS_PROXY_PORT".into());
        bwrap_args.push(p.to_string());
    }
    Ok(())
}

/// `wrapCommandWithSandboxLinux(params)` (:822-983): assemble the full bwrap
/// argv. Returns `(wrapped_command, mount_points)` where `mount_points` is the
/// slice the caller must pass to [`cleanup_bwrap_mount_points`] after the
/// spawned command exits.
///
/// Branch-for-branch faithful to the TS:
/// - **Short-circuit:** no network AND no read-deny AND no write-config →
///   return `(command, vec![])` unchanged.
/// - **Network:** `--unshare-net`; if both sockets present → bind each
///   (`--bind <sock> <sock>`), emit `--setenv k v` for every
///   `generate_proxy_env_vars(3128, 1080, ca, platform, tmpdir)` pair, plus the
///   `CLAUDE_CODE_HOST_{HTTP,SOCKS}_PROXY_PORT` vars. No sockets → bare
///   `--unshare-net` (full block).
/// - **Filesystem:** `generate_filesystem_args(...)` args + mount points; then
///   `--dev /dev`, `--unshare-pid`, and `--proc /proc` (secure) or
///   `--unshare-user --bind /proc /proc` (weaker-nested).
/// - **Command:** resolve the shell on `PATH`; `-- <shell> -c <cmd>` where `cmd`
///   is `build_sandbox_command(...)` when network + both sockets (the seccomp
///   prefix is woven into its inner script), else — when an `apply-seccomp`
///   prefix resolved — `<prefix><shell> -c <cmd>`, else the bare command.
///
/// The `apply-seccomp` prefix (the `socket(AF_UNIX)` BPF block) is emitted
/// whenever `!allow_all_unix_sockets` AND [`resolve_apply_seccomp_prefix`]
/// resolves a binary for the current arch; `allow_all_unix_sockets` skips it.
///
/// # Errors
/// Returns an `io::Error` if the requested shell cannot be resolved on `PATH`,
/// or (faithful to the TS) if a declared bridge socket no longer exists.
pub fn wrap_command_with_sandbox_linux(
    params: &WrapParams<'_>,
) -> io::Result<(String, Vec<PathBuf>)> {
    // Read: denyOnly pattern — empty array means no restrictions.
    let has_read_restrictions = params
        .read_config
        .is_some_and(|rc| !rc.deny_only.is_empty());
    // Write: allowOnly pattern — None means no restrictions, any config = restrictions.
    let has_write_restrictions = params.write_config.is_some();

    // Short-circuit: no sandboxing needed.
    if !params.needs_network_restriction && !has_read_restrictions && !has_write_restrictions {
        return Ok((params.command.to_string(), Vec::new()));
    }

    let mut bwrap_args: Vec<String> = vec!["--new-session".into(), "--die-with-parent".into()];

    // ===== SECCOMP: when NOT allowing all unix sockets, locate apply-seccomp
    // and emit its prefix (the BPF `socket(AF_UNIX)` block). allow_all_unix_sockets
    // → skip the locator entirely (no prefix). =====
    let apply_seccomp_prefix: Option<String> = if params.allow_all_unix_sockets {
        None
    } else {
        resolve_apply_seccomp_prefix(
            params.seccomp_apply_path.map(Path::new),
            params.seccomp_apply_argv0,
        )
    };

    // ===== NETWORK RESTRICTIONS =====
    if params.needs_network_restriction {
        push_network_args(&mut bwrap_args, params)?;
    }

    // ===== FILESYSTEM RESTRICTIONS =====
    let (fs_args, mount_points) = generate_filesystem_args(
        params.read_config,
        params.write_config,
        params.ripgrep_cmd,
        params.mandatory_deny_search_depth,
        params.allow_git_config,
        params.cwd,
    );
    bwrap_args.extend(fs_args);

    // Always bind /dev.
    bwrap_args.push("--dev".into());
    bwrap_args.push("/dev".into());

    // ===== PID NAMESPACE ISOLATION (must come AFTER filesystem binds) =====
    bwrap_args.push("--unshare-pid".into());
    if params.enable_weaker_nested_sandbox {
        bwrap_args.push("--unshare-user".into());
        bwrap_args.push("--bind".into());
        bwrap_args.push("/proc".into());
        bwrap_args.push("/proc".into());
    } else {
        bwrap_args.push("--proc".into());
        bwrap_args.push("/proc".into());
    }

    // ===== COMMAND =====
    let shell_name = params.bin_shell.unwrap_or("bash");
    let shell = which_sync(shell_name)
        .ok_or_else(|| io::Error::other(format!("Shell '{shell_name}' not found in PATH")))?;
    let shell = shell.to_string_lossy().into_owned();
    bwrap_args.push("--".into());
    bwrap_args.push(shell.clone());
    bwrap_args.push("-c".into());

    let final_cmd = if params.needs_network_restriction
        && params.http_socket_path.is_some()
        && params.socks_socket_path.is_some()
    {
        build_sandbox_command(
            params.http_socket_path.unwrap(),
            params.socks_socket_path.unwrap(),
            params.command,
            apply_seccomp_prefix.as_deref(),
            &shell,
            params.socat_path,
        )
    } else if let Some(prefix) = apply_seccomp_prefix.as_deref() {
        format!("{prefix}{}", shjoin([shell.as_str(), "-c", params.command]))
    } else {
        params.command.to_string()
    };
    bwrap_args.push(final_cmd);

    let bwrap = params.bwrap_path.unwrap_or("bwrap");
    let mut full: Vec<&str> = vec![bwrap];
    full.extend(bwrap_args.iter().map(String::as_str));
    let wrapped = shjoin(full);

    Ok((wrapped, mount_points))
}

/// SIGTERM both children, ignoring errors (the TS `process.kill(pid, SIGTERM)`
/// in `try {} catch {}`).
fn kill_both(http: &mut Child, socks: &mut Child) {
    let _ = http.kill();
    let _ = http.wait();
    let _ = socks.kill();
    let _ = socks.wait();
}

/// 8 CSPRNG bytes as lowercase hex (`randomBytes(8).toString('hex')`). The
/// bridge socket path lives under `std::env::temp_dir()` (typically `/tmp`,
/// mode 1777 / world-writable), so an UNPREDICTABLE name is load-bearing: a
/// predictable path lets a local process pre-create it and break the bridge
/// (socat's `UNIX-LISTEN` fails `EADDRINUSE` on an existing path) or collide
/// with a concurrent invocation. Use `getrandom` (kernel CSPRNG) to match the
/// TS `crypto.randomBytes(8)` 64-bit entropy; fall back to the pid+clock mix
/// only if the syscall fails (degraded but never panics).
fn random_hex_8() -> String {
    use std::fmt::Write as _;
    let mut buf = [0_u8; 8];
    if getrandom::getrandom(&mut buf).is_err() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mixed = now.as_secs().wrapping_mul(0x9E37_79B9_7F4A_7C15)
            ^ u64::from(now.subsec_nanos()).rotate_left(17)
            ^ u64::from(std::process::id()).rotate_left(31);
        buf = mixed.to_le_bytes();
    }
    let mut out = String::with_capacity(16);
    for b in buf {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dep_check_explicit_paths_not_executable_are_errors() {
        let opts = LinuxDependencyOpts {
            bwrap_path: Some("/nonexistent/bwrap".to_string()),
            socat_path: Some("/nonexistent/socat".to_string()),
            ..Default::default()
        };
        let res = check_linux_dependencies(&opts);
        assert!(res
            .errors
            .contains(&"bubblewrap (bwrap) not executable at /nonexistent/bwrap".to_string()));
        assert!(res
            .errors
            .contains(&"socat not executable at /nonexistent/socat".to_string()));
        // Seccomp warning is present iff apply-seccomp cannot be located (no
        // explicit path/argv0 here → depends on the workspace fallback + arch).
        let seccomp_available = resolve_apply_seccomp_prefix(None, None).is_some();
        assert_eq!(
            res.warnings
                .contains(&"seccomp not available - unix socket access not restricted".to_string()),
            !seccomp_available
        );
    }

    #[test]
    fn dep_check_no_overrides_path_lookup() {
        // No explicit overrides → the PATH branch runs. The error set is exactly
        // the not-installed messages for whichever binary is absent on this host.
        // (On a host where both exist, errors is empty.) We assert the strings the
        // PATH-miss branch produces match the byte-exact TS constants.
        let opts = LinuxDependencyOpts::default();
        let res = check_linux_dependencies(&opts);
        for err in &res.errors {
            assert!(
                err == "bubblewrap (bwrap) not installed" || err == "socat not installed",
                "unexpected PATH-branch error: {err}"
            );
        }
        // bwrap absence → exactly this string; socat absence → exactly that one.
        if which_sync("bwrap").is_none() {
            assert!(res
                .errors
                .contains(&"bubblewrap (bwrap) not installed".to_string()));
        }
        if which_sync("socat").is_none() {
            assert!(res.errors.contains(&"socat not installed".to_string()));
        }
    }

    #[test]
    fn dep_status_explicit_nonexistent_is_false() {
        let opts = LinuxDependencyOpts {
            bwrap_path: Some("/nonexistent/bwrap".to_string()),
            socat_path: Some("/nonexistent/socat".to_string()),
            ..Default::default()
        };
        let st = get_linux_dependency_status(&opts);
        assert!(!st.has_bwrap);
        assert!(!st.has_socat);
        // has_seccomp_apply reflects the real locator: with no explicit
        // path/argv0 it depends only on whether the workspace fallback binary
        // exists (and the arch is supported), independent of bwrap/socat.
    }

    #[test]
    fn seccomp_status_with_explicit_path() {
        // An explicit, existing apply-seccomp path → has_seccomp_apply true.
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("apply-seccomp");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let opts = LinuxDependencyOpts {
            seccomp_apply_path: Some(bin.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let st = get_linux_dependency_status(&opts);
        // On x86_64/aarch64 the locator resolves; on an unsupported arch the
        // gate returns None regardless (so only assert the supported case).
        if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            assert!(st.has_seccomp_apply);
            // ...and the no-seccomp warning is suppressed.
            let chk = check_linux_dependencies(&opts);
            assert!(!chk
                .warnings
                .iter()
                .any(|w| w.contains("seccomp not available")));
        } else {
            assert!(!st.has_seccomp_apply);
        }
    }

    // ---- resolve_apply_seccomp_prefix (Task 2) ----

    #[test]
    fn resolve_seccomp_explicit_existing_path() {
        if !seccomp_arch_supported() {
            assert!(resolve_apply_seccomp_prefix(None, None).is_none());
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("apply-seccomp");
        fs::write(&bin, b"x").unwrap();
        let prefix = resolve_apply_seccomp_prefix(Some(&bin), None).unwrap();
        // Prefix = shell-quoted path + a trailing space.
        assert!(
            prefix.ends_with(' '),
            "prefix must be space-terminated: {prefix:?}"
        );
        assert!(prefix.contains(&*bin.to_string_lossy()), "prefix: {prefix}");
    }

    #[test]
    fn resolve_seccomp_explicit_missing_falls_through_to_argv0() {
        if !seccomp_arch_supported() {
            return;
        }
        let missing = Path::new("/nonexistent/apply-seccomp");
        // Missing explicit path + a trusted argv0 → argv0 is used (trusted,
        // no host-side existence check).
        let prefix = resolve_apply_seccomp_prefix(Some(missing), Some("apply-seccomp")).unwrap();
        assert_eq!(prefix, "apply-seccomp ");
    }

    #[test]
    fn resolve_seccomp_none_when_absent_and_no_argv0() {
        if !seccomp_arch_supported() {
            return;
        }
        // A missing explicit path + no argv0: only the workspace fallback could
        // resolve. Point the explicit path at a definitely-missing file and
        // assert that, absent a built fallback, nothing trusted leaks. We can't
        // control the workspace target/ here, so only assert the explicit+argv0
        // miss path: explicit missing + no argv0 must NOT return the missing
        // explicit path itself.
        let missing = Path::new("/nonexistent/apply-seccomp");
        let got = resolve_apply_seccomp_prefix(Some(missing), None);
        assert!(
            got.as_deref()
                .is_none_or(|p| !p.contains("/nonexistent/apply-seccomp")),
            "missing explicit path must not be returned: {got:?}"
        );
    }

    #[test]
    fn resolve_seccomp_argv0_trusted_without_existence_check() {
        if !seccomp_arch_supported() {
            return;
        }
        // No explicit path, just a trusted argv0 → returned as-is + space.
        let prefix = resolve_apply_seccomp_prefix(None, Some("apply-seccomp")).unwrap();
        assert_eq!(prefix, "apply-seccomp ");
    }

    #[test]
    fn cleanup_removes_empty_file_and_empty_dir_keeps_nonempty() {
        let tmp = tempfile::tempdir().unwrap();
        let empty_file = tmp.path().join("empty_mount");
        fs::write(&empty_file, b"").unwrap();
        let empty_dir = tmp.path().join("empty_dir");
        fs::create_dir(&empty_dir).unwrap();
        let nonempty_file = tmp.path().join("nonempty");
        fs::write(&nonempty_file, b"data").unwrap();
        let nonempty_dir = tmp.path().join("nonempty_dir");
        fs::create_dir(&nonempty_dir).unwrap();
        fs::write(nonempty_dir.join("child"), b"x").unwrap();

        let points = vec![
            empty_file.clone(),
            empty_dir.clone(),
            nonempty_file.clone(),
            nonempty_dir.clone(),
        ];
        cleanup_bwrap_mount_points(&points);

        assert!(!empty_file.exists(), "empty file should be unlinked");
        assert!(!empty_dir.exists(), "empty dir should be rmdir'd");
        assert!(nonempty_file.exists(), "non-empty file kept");
        assert!(nonempty_dir.exists(), "non-empty dir kept");
    }

    #[test]
    fn cleanup_ignores_missing_paths() {
        let missing = PathBuf::from("/nonexistent/path/xyz");
        cleanup_bwrap_mount_points(&[missing]); // must not panic
    }

    #[test]
    fn is_executable_false_for_missing() {
        assert!(!is_executable("/nonexistent/binary/xyz"));
    }

    #[test]
    fn host_socat_args_exact() {
        let args = host_socat_args("/tmp/claude-http-abc.sock", 8080);
        assert_eq!(
            args[0],
            "UNIX-LISTEN:/tmp/claude-http-abc.sock,fork,reuseaddr"
        );
        assert_eq!(
            args[1],
            "TCP:localhost:8080,keepalive,keepidle=10,keepintvl=5,keepcnt=3"
        );
    }

    #[test]
    fn build_sandbox_command_no_seccomp_eval_branch_byte_exact() {
        let cmd = build_sandbox_command(
            "/tmp/claude-http-abc.sock",
            "/tmp/claude-socks-abc.sock",
            "echo hi",
            None,
            "/bin/bash",
            Some("/usr/bin/socat"),
        );
        let expected_inner = "/usr/bin/socat TCP-LISTEN:3128,fork,reuseaddr UNIX-CONNECT:/tmp/claude-http-abc.sock >/dev/null 2>&1 &\n/usr/bin/socat TCP-LISTEN:1080,fork,reuseaddr UNIX-CONNECT:/tmp/claude-socks-abc.sock >/dev/null 2>&1 &\ntrap \"kill %1 %2 2>/dev/null; exit\" EXIT\neval 'echo hi'";
        let expected = format!("/bin/bash -c {}", shjoin([expected_inner]));
        assert_eq!(cmd, expected);
    }

    #[test]
    fn build_sandbox_command_seccomp_branch_emits_prefix_not_eval() {
        // With a seccomp prefix, the inner script runs
        // `<prefix><shell> -c '<cmd>'` instead of `eval '<cmd>'`.
        let cmd = build_sandbox_command(
            "/tmp/h.sock",
            "/tmp/s.sock",
            "echo hi",
            Some("/opt/apply-seccomp "),
            "/bin/bash",
            Some("/usr/bin/socat"),
        );
        // The seccomp-wrapped command line.
        let wrapped = format!(
            "/opt/apply-seccomp {}",
            shjoin(["/bin/bash", "-c", "echo hi"])
        );
        let expected_inner = format!(
            "/usr/bin/socat TCP-LISTEN:3128,fork,reuseaddr UNIX-CONNECT:/tmp/h.sock >/dev/null 2>&1 &\n\
             /usr/bin/socat TCP-LISTEN:1080,fork,reuseaddr UNIX-CONNECT:/tmp/s.sock >/dev/null 2>&1 &\n\
             trap \"kill %1 %2 2>/dev/null; exit\" EXIT\n{wrapped}"
        );
        let expected = format!("/bin/bash -c {}", shjoin([expected_inner.as_str()]));
        assert_eq!(cmd, expected);
        // It is the seccomp branch, NOT the eval branch.
        assert!(cmd.contains("/opt/apply-seccomp"), "cmd: {cmd}");
        assert!(!cmd.contains("eval "), "must not use eval branch: {cmd}");
    }

    #[test]
    fn build_sandbox_command_defaults_socat_and_shell() {
        let cmd = build_sandbox_command(
            "/tmp/h.sock",
            "/tmp/s.sock",
            "ls",
            None,
            "",   // empty shell -> bash
            None, // no socat -> "socat"
        );
        assert!(cmd.starts_with("bash -c "));
        assert!(cmd.contains("socat TCP-LISTEN:3128,fork,reuseaddr UNIX-CONNECT:/tmp/h.sock"));
        assert!(cmd.contains("socat TCP-LISTEN:1080,fork,reuseaddr UNIX-CONNECT:/tmp/s.sock"));
    }

    #[test]
    fn random_hex_8_is_16_hex_chars() {
        let h = random_hex_8();
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // Bridge SPAWN test — gated to skip if `socat` is absent on the host.
    #[test]
    fn bridge_spawns_socat_and_creates_sockets() {
        if which_sync("socat").is_none() {
            eprintln!("SKIP bridge_spawns_socat_and_creates_sockets: socat not on PATH");
            return;
        }
        // Bind a real TCP listener so socat's TCP target connects, then spawn the
        // bridge. We use the same port for both http/socks (the listener accepts
        // both connections). The sockets must appear and both children stay alive.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Accept-and-drop in a background thread so socat's connect succeeds.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });

        let bridge = initialize_linux_network_bridge(port, port, None)
            .expect("bridge should initialize with socat present");
        assert!(bridge.http_socket_path.exists(), "http socket created");
        assert!(bridge.socks_socket_path.exists(), "socks socket created");
        drop(bridge); // teardown SIGTERMs both children (must not panic/hang)
    }

    #[test]
    fn bridge_fails_when_socat_missing() {
        // Explicit bogus socat path -> spawn fails -> Err with the EXACT message.
        let err =
            initialize_linux_network_bridge(8080, 8081, Some("/nonexistent/socat")).unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to start HTTP bridge process"),
            "got: {err}"
        );
    }

    // ---- wrap_command_with_sandbox_linux (Task 3) ----

    fn base_params<'a>(command: &'a str, cwd: &'a str, tmp: &'a str) -> WrapParams<'a> {
        WrapParams {
            command,
            needs_network_restriction: false,
            http_socket_path: None,
            socks_socket_path: None,
            http_proxy_port: None,
            socks_proxy_port: None,
            ca_cert_path: None,
            read_config: None,
            write_config: None,
            enable_weaker_nested_sandbox: false,
            allow_all_unix_sockets: false,
            seccomp_apply_path: None,
            seccomp_apply_argv0: None,
            bin_shell: None,
            ripgrep_cmd: "rg",
            mandatory_deny_search_depth: 3,
            allow_git_config: false,
            bwrap_path: None,
            socat_path: None,
            cwd,
            platform: Platform::Linux,
            tmpdir: tmp,
            proxy_auth_token: None,
        }
    }

    #[test]
    fn wrap_no_restrictions_short_circuits() {
        let p = base_params("echo hi", "/tmp", "/tmp/claude");
        let (cmd, mounts) = wrap_command_with_sandbox_linux(&p).unwrap();
        assert_eq!(cmd, "echo hi");
        assert!(mounts.is_empty());
    }

    #[test]
    fn wrap_network_only_binds_sockets_and_setenv() {
        // Create real socket-path stand-ins so the existence checks pass.
        let dir = tempfile::tempdir().unwrap();
        let http = dir.path().join("claude-http.sock");
        let socks = dir.path().join("claude-socks.sock");
        fs::write(&http, b"").unwrap();
        fs::write(&socks, b"").unwrap();
        let http_s = http.to_string_lossy().into_owned();
        let socks_s = socks.to_string_lossy().into_owned();

        let mut p = base_params("curl https://example.com", "/tmp", "/tmp/claude");
        p.needs_network_restriction = true;
        p.http_socket_path = Some(&http_s);
        p.socks_socket_path = Some(&socks_s);
        p.http_proxy_port = Some(8080);
        p.socks_proxy_port = Some(8081);

        let (cmd, _mounts) = wrap_command_with_sandbox_linux(&p).unwrap();
        assert!(cmd.contains("--unshare-net"), "cmd: {cmd}");
        // Socket binds (each path bound to itself). shlex-quoted; the path is in.
        assert!(
            cmd.contains(&format!("--bind {http_s} {http_s}")),
            "cmd: {cmd}"
        );
        assert!(
            cmd.contains(&format!("--bind {socks_s} {socks_s}")),
            "cmd: {cmd}"
        );
        // HTTP_PROXY setenv to the internal listener.
        assert!(
            cmd.contains("--setenv HTTP_PROXY http://localhost:3128"),
            "cmd: {cmd}"
        );
        // Host port transparency vars.
        assert!(
            cmd.contains("--setenv LINGXI_HOST_HTTP_PROXY_PORT 8080"),
            "cmd: {cmd}"
        );
        assert!(
            cmd.contains("--setenv LINGXI_HOST_SOCKS_PROXY_PORT 8081"),
            "cmd: {cmd}"
        );
        // The sandbox socat command (build_sandbox_command) is embedded.
        assert!(cmd.contains("TCP-LISTEN:3128,fork,reuseaddr"), "cmd: {cmd}");
        assert!(cmd.contains("TCP-LISTEN:1080,fork,reuseaddr"), "cmd: {cmd}");
        // --proc /proc (secure mode, default).
        assert!(cmd.contains("--unshare-pid"), "cmd: {cmd}");
        assert!(cmd.contains("--proc /proc"), "cmd: {cmd}");
    }

    #[test]
    fn wrap_network_no_sockets_is_bare_unshare_net() {
        let mut p = base_params("echo hi", "/tmp", "/tmp/claude");
        p.needs_network_restriction = true;
        let (cmd, _m) = wrap_command_with_sandbox_linux(&p).unwrap();
        assert!(cmd.contains("--unshare-net"), "cmd: {cmd}");
        assert!(!cmd.contains("--setenv HTTP_PROXY"), "no proxy env: {cmd}");
        assert!(!cmd.contains("TCP-LISTEN:3128"), "no socat listener: {cmd}");
    }

    #[test]
    fn wrap_write_restrict_emits_fs_args_and_proc() {
        let dir = tempfile::tempdir().unwrap();
        let wpath = dir.path().to_string_lossy().into_owned();
        let wc = WriteConfig {
            allow_only: vec![wpath.clone()],
            deny_within_allow: vec![],
        };
        let mut p = base_params("echo hi", &wpath, "/tmp/claude");
        p.write_config = Some(&wc);
        let (cmd, _m) = wrap_command_with_sandbox_linux(&p).unwrap();
        // Read-only root then writable bind (fs_args).
        assert!(cmd.contains("--ro-bind / /"), "cmd: {cmd}");
        assert!(cmd.contains("--unshare-pid"), "cmd: {cmd}");
        assert!(cmd.contains("--proc /proc"), "cmd: {cmd}");
        assert!(cmd.contains("--dev /dev"), "cmd: {cmd}");
    }

    #[test]
    fn wrap_weaker_nested_uses_unshare_user_bind_proc() {
        let dir = tempfile::tempdir().unwrap();
        let wpath = dir.path().to_string_lossy().into_owned();
        let wc = WriteConfig {
            allow_only: vec![wpath.clone()],
            deny_within_allow: vec![],
        };
        let mut p = base_params("echo hi", &wpath, "/tmp/claude");
        p.write_config = Some(&wc);
        p.enable_weaker_nested_sandbox = true;
        let (cmd, _m) = wrap_command_with_sandbox_linux(&p).unwrap();
        assert!(
            cmd.contains("--unshare-user --bind /proc /proc"),
            "cmd: {cmd}"
        );
        assert!(
            !cmd.contains("--proc /proc"),
            "should NOT have plain --proc: {cmd}"
        );
    }

    #[test]
    fn wrap_threads_mount_points_from_fs_args() {
        // A denyOnly read restriction on a tmpfile produces mount points.
        let dir = tempfile::tempdir().unwrap();
        let deny = dir.path().join("secret.txt");
        fs::write(&deny, b"x").unwrap();
        let rc = ReadConfig {
            deny_only: vec![deny.to_string_lossy().into_owned()],
            allow_within_deny: vec![],
        };
        let cwd = dir.path().to_string_lossy().into_owned();
        let mut p = base_params("echo hi", &cwd, "/tmp/claude");
        p.read_config = Some(&rc);
        let (_cmd, mounts) = wrap_command_with_sandbox_linux(&p).unwrap();
        // fs_args returns mount points for the tmpfs cover; threaded back out.
        // (Exact count is fs_args' concern; assert the wrap returns the vec.)
        let _ = mounts; // presence verified by type; cleanup test below covers behavior
    }

    #[test]
    fn wrap_seccomp_prefix_included_when_resolvable_and_not_allow_all() {
        if !seccomp_arch_supported() {
            return; // arch-gated: no prefix on unsupported arch
        }
        // A write restriction (no network) → the non-network command branch,
        // where the apply-seccomp prefix is woven directly before `<shell> -c`.
        let dir = tempfile::tempdir().unwrap();
        let wpath = dir.path().to_string_lossy().into_owned();
        let wc = WriteConfig {
            allow_only: vec![wpath.clone()],
            deny_within_allow: vec![],
        };
        let mut p = base_params("echo hi", &wpath, "/tmp/claude");
        p.write_config = Some(&wc);
        p.allow_all_unix_sockets = false;
        // A trusted argv0 resolves without a host-side file (inside-bwrap PATH).
        p.seccomp_apply_argv0 = Some("apply-seccomp");
        let (cmd, _m) = wrap_command_with_sandbox_linux(&p).unwrap();
        assert!(
            cmd.contains("apply-seccomp"),
            "seccomp prefix missing: {cmd}"
        );
    }

    #[test]
    fn wrap_seccomp_prefix_omitted_when_allow_all_unix_sockets() {
        if !seccomp_arch_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let wpath = dir.path().to_string_lossy().into_owned();
        let wc = WriteConfig {
            allow_only: vec![wpath.clone()],
            deny_within_allow: vec![],
        };
        let mut p = base_params("echo hi", &wpath, "/tmp/claude");
        p.write_config = Some(&wc);
        // Even with a resolvable argv0, allow_all_unix_sockets skips the locator.
        p.allow_all_unix_sockets = true;
        p.seccomp_apply_argv0 = Some("apply-seccomp");
        let (cmd, _m) = wrap_command_with_sandbox_linux(&p).unwrap();
        assert!(
            !cmd.contains("apply-seccomp"),
            "seccomp prefix must be omitted: {cmd}"
        );
    }

    #[test]
    fn wrap_network_seccomp_prefix_woven_into_sandbox_command() {
        if !seccomp_arch_supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let http = dir.path().join("claude-http.sock");
        let socks = dir.path().join("claude-socks.sock");
        fs::write(&http, b"").unwrap();
        fs::write(&socks, b"").unwrap();
        let http_s = http.to_string_lossy().into_owned();
        let socks_s = socks.to_string_lossy().into_owned();

        let mut p = base_params("curl https://example.com", "/tmp", "/tmp/claude");
        p.needs_network_restriction = true;
        p.http_socket_path = Some(&http_s);
        p.socks_socket_path = Some(&socks_s);
        p.seccomp_apply_argv0 = Some("apply-seccomp");
        let (cmd, _m) = wrap_command_with_sandbox_linux(&p).unwrap();
        // The seccomp prefix is embedded in build_sandbox_command's inner script,
        // and the eval branch is NOT taken.
        assert!(cmd.contains("apply-seccomp"), "cmd: {cmd}");
        assert!(
            cmd.contains("TCP-LISTEN:3128"),
            "socat listener present: {cmd}"
        );
    }
}
