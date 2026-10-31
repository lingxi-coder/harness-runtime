//! Host-owned shell confinement, separate from mutable session settings.

use sandbox::runtime_config::{FilesystemRestrictionConfig, SandboxRuntimeConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An immutable, in-process capability for a private POSIX invocation.
/// It is never decoded from tool input or settings. The host must create and
/// validate these directories before publishing the capability.
#[derive(Clone, Debug)]
pub struct IsolatedShellPolicy {
    workspace: PathBuf,
    home: PathBuf,
    temporary: PathBuf,
    sandbox: SandboxRuntimeConfig,
}

impl IsolatedShellPolicy {
    /// Freeze the only mutable directories. `root` and its children must be
    /// canonical, host-owned private directories; the sandbox cannot rename
    /// them or write the configuration, memory or immutable input snapshot.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        let workspace = root.join("workspace");
        let home = root.join("shell-home");
        let temporary = root.join("tmp");
        let mut sandbox = SandboxRuntimeConfig {
            enabled: true,
            fail_if_unavailable: true,
            allow_unsandboxed_commands: false,
            auto_allow_bash_if_sandboxed: false,
            ..Default::default()
        };
        sandbox.network.strict_allowlist = true;
        // Read only the private scope and system executables/libraries. In
        // particular, /Users, /home, /Volumes and global temporary storage are
        // not ambient read grants. Network and local sockets have no grants.
        let mut allow_read = vec![root.to_string_lossy().into_owned()];
        allow_read.extend(
            [
                "/bin",
                "/sbin",
                "/usr/bin",
                "/usr/lib",
                "/usr/libexec",
                "/usr/share/locale",
                "/usr/share/zoneinfo",
                "/System/Library",
                "/Library/Apple",
                "/lib",
                "/lib64",
                "/etc/ld.so.cache",
                "/etc/ssl/certs",
                "/dev/null",
                "/dev/random",
                "/dev/urandom",
                "/dev/fd",
            ]
            .into_iter()
            .map(String::from),
        );
        sandbox.filesystem = FilesystemRestrictionConfig {
            deny_read: vec!["/".into()],
            allow_read,
            allow_write: vec![
                workspace.to_string_lossy().into_owned(),
                temporary.to_string_lossy().into_owned(),
            ],
            deny_write: vec![
                root.join("home").to_string_lossy().into_owned(),
                root.join("inputs").to_string_lossy().into_owned(),
                home.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };
        Self {
            workspace,
            home,
            temporary,
            sandbox,
        }
    }

    /// Every command starts here; model-selected cwd changes do not persist.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Temporary storage stays inside the same private principal.
    #[must_use]
    pub fn temporary(&self) -> &Path {
        &self.temporary
    }

    /// The frozen policy overrides settings and the live /sandbox toggle.
    #[must_use]
    pub fn sandbox(&self) -> &SandboxRuntimeConfig {
        &self.sandbox
    }

    /// Complete child environment, used with `env_clear` before exec. No
    /// credential, startup-script, loader or owner-specific PATH is inherited.
    #[must_use]
    pub fn environment(&self) -> HashMap<String, String> {
        HashMap::from([
            ("HOME".into(), self.home.to_string_lossy().into_owned()),
            (
                "TMPDIR".into(),
                self.temporary.to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("LANG".into(), "C".into()),
            ("LC_ALL".into(), "C".into()),
        ])
    }

    /// Both outer and wrapped shells use this fixed executable; ambient shell
    /// overrides and login/profile scripts must never run before confinement.
    pub const EXECUTABLE: &'static str = "/bin/bash";

    /// Non-login, non-interactive Bash with no startup files.
    #[must_use]
    pub fn arguments(command: String) -> Vec<String> {
        vec!["--noprofile".into(), "--norc".into(), "-c".into(), command]
    }
}
