//! Private command wrapper: no ordinary SandboxManager defaults, global tmp,
//! proxies, Git worktree expansion or owner-home write grants.

use sandbox::runtime_config::{Platform, SandboxRuntimeConfig};
use sandbox::wrap::SandboxWrapError;
use sandbox_runtime::fs_args::{ReadConfig, WriteConfig};
use std::path::Path;
use std::sync::Arc;
use tool_api::isolated_shell::IsolatedShellPolicy;

pub(super) struct TeamSandboxRunner(pub Arc<IsolatedShellPolicy>);

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl TeamSandboxRunner {
    fn wrap_command(
        &self,
        command: &str,
        platform: Platform,
        cwd: Option<&Path>,
    ) -> Result<String, SandboxWrapError> {
        if !cfg!(target_os = "macos")
            || platform != Platform::Mac
            || cwd != Some(self.0.workspace())
        {
            return Err(SandboxWrapError::Unsupported(
                "Private Team sandbox is available only on macOS with its fixed workspace".into(),
            ));
        }
        let fs = &self.0.sandbox().filesystem;
        let read = ReadConfig {
            deny_only: fs.deny_read.clone(),
            allow_within_deny: fs.allow_read.clone(),
        };
        let mut write = WriteConfig {
            allow_only: fs.allow_write.clone(),
            deny_within_allow: fs.deny_write.clone(),
        };
        write.allow_only.extend(
            ["/dev/stdout", "/dev/stderr", "/dev/null"]
                .into_iter()
                .map(String::from),
        );
        let workspace = self.0.workspace().to_string_lossy();
        let mut profile = sandbox_runtime::macos::generate_sandbox_profile(
            &sandbox_runtime::macos::ProfileParams {
                read_config: Some(&read),
                write_config: Some(&write),
                needs_network_restriction: true,
                cwd: Some(&workspace),
                log_tag: "lingxi-team",
                ..Default::default()
            },
        );
        // General desktop profiles permit system credential/UI services and
        // shared IPC. This file-only Team profile has no such capabilities.
        profile.push_str("\n(deny mach-lookup)\n(deny appleevent-send)\n(deny ipc-posix-shm)\n(deny ipc-posix-sem)\n(deny iokit-open)\n(deny distributed-notification-post)\n");
        Ok(format!(
            "exec /usr/bin/sandbox-exec -p {} /bin/bash --noprofile --norc -c {}",
            quote(&profile),
            quote(command)
        ))
    }
}

#[async_trait::async_trait]
impl tool_api::SandboxRunner for TeamSandboxRunner {
    async fn wrap(
        &self,
        command: &str,
        _: &SandboxRuntimeConfig,
        platform: Platform,
        _: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<String, SandboxWrapError> {
        self.wrap_command(command, platform, cwd)
    }
}
