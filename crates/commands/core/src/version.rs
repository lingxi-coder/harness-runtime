//! `/version` — print build info (semver + short git SHA).
//!
//! Locked display template (`LingXi` UX, M5-11 T0 step 2 L11):
//!   `"lingxi-cli {version} ({git_sha:short})"`
//! Host metadata is supplied at composition time; the runtime dependency's
//! build metadata remains separately available through [`runtime_build_info`].

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use telemetry::tengu::command as cmd_evt;

/// Rust-only build metadata supplied by the embedding host. This is deliberately
/// independent of wire DTOs and of this dependency's package/Git identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// Embedding host's package version.
    pub version: &'static str,
    /// Short revision of the embedding host's source checkout.
    pub git_sha_short: &'static str,
}

impl BuildInfo {
    /// Construct metadata captured by the embedding host at compile time.
    pub const fn new(version: &'static str, git_sha_short: &'static str) -> Self {
        Self {
            version,
            git_sha_short,
        }
    }
}

impl Default for BuildInfo {
    fn default() -> Self {
        Self::new("unknown", "unknown")
    }
}

/// The separately compiled runtime command package's identity. Never substituted
/// for host identity in `/version` when the host omitted build information.
pub const fn runtime_build_info() -> BuildInfo {
    BuildInfo::new(
        env!("CARGO_PKG_VERSION"),
        match option_env!("LINGXI_GIT_SHA_SHORT") {
            Some(sha) => sha,
            None => "unknown",
        },
    )
}

/// `/version` handler — preserves the LingXi display contract.
#[derive(Debug, Default, Clone)]
pub struct VersionHandler {
    build_info: BuildInfo,
}

impl VersionHandler {
    /// Construct a handler with unknown host identity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a handler using the embedding host's identity.
    #[must_use]
    pub fn with_build_info(build_info: BuildInfo) -> Self {
        Self { build_info }
    }
}

#[async_trait]
impl BuiltinCommandHandler for VersionHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::VERSION_STARTED);
        let s = format!(
            "lingxi-cli {} ({})",
            self.build_info.version, self.build_info.git_sha_short
        );
        telemetry::emit_command_completed(cmd_evt::VERSION_COMPLETED, "");
        CommandResult::Done { display: Some(s) }
    }
    fn name(&self) -> &str {
        "version"
    }
    fn description(&self) -> &str {
        core_description("version")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "version".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn version_format() {
        let h = VersionHandler::with_build_info(BuildInfo::new("9.8.7-host", "host123"));
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert!(s.starts_with("lingxi-cli "));
            assert!(s.ends_with(')'));
            assert!(s.contains('('));
            assert_eq!(s, "lingxi-cli 9.8.7-host (host123)");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn omitted_host_identity_does_not_report_runtime_identity() {
        let runtime = runtime_build_info();
        assert_eq!(runtime.version, env!("CARGO_PKG_VERSION"));
        let CommandResult::Done {
            display: Some(display),
        } = VersionHandler::new().handle(&args()).await
        else {
            panic!()
        };
        assert_eq!(display, "lingxi-cli unknown (unknown)");
    }

    #[tokio::test]
    async fn name_and_description() {
        let h = VersionHandler::new();
        assert_eq!(h.name(), "version");
        // SLASH-14: `Print version information` matched NEITHER oracle
        // `/version` object. Both twins @2.1.238 296268759 are
        // `isEnabled:()=>!1`, so the command is filtered out of `/help` and
        // the palette (`names::STATICALLY_DISABLED_COMMANDS`) while staying
        // dispatchable; the advertised string is now the interactive twin's,
        // matching the `tui::command` row.
        assert_eq!(
            h.description(),
            "Show this session's version (autoupdate may have a newer one)"
        );
    }
}
