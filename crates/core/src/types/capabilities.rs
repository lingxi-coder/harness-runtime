//! Platform capability flags. Subsystems use these to filter their
//! available features at runtime (e.g., the Bash tool is hidden when
//! `process == false`).
//!
//! See spec §4.11 (Capability System) and Appendix A (platform matrix).

use serde::{Deserialize, Serialize};

/// Flags describing which platform features the host environment supports.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // capability flag struct; bools are the natural representation
pub struct PlatformCapabilities {
    /// Filesystem-related capabilities (size limits, sandboxing, watch).
    pub filesystem: FileSystemCapabilities,
    /// Whether process spawning (e.g., the Bash tool) is available.
    pub process: bool,
    /// Whether outbound HTTP requests are available.
    pub http: bool,
    /// Whether MCP server connections are available.
    pub mcp: bool,
    /// Whether git worktree subsystem is available.
    pub worktree: bool,
    /// Whether multi-agent swarm execution is available.
    pub swarm: bool,
    /// Whether OS-level desktop notifications are available.
    pub os_notifications: bool,
    /// Whether the IDE bridge integration is available.
    pub ide_bridge: bool,
}

/// Filesystem-specific capabilities and limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSystemCapabilities {
    /// Maximum single-read size in bytes.
    pub max_read_size: u64,
    /// Maximum single-write size in bytes.
    pub max_write_size: u64,
    /// Whether the filesystem supports symbolic links.
    pub supports_symlinks: bool,
    /// Whether the filesystem supports file/directory watching.
    pub supports_watch: bool,
    /// Optional sandbox root path; if set, all FS access is confined under it.
    pub sandbox_root: Option<String>,
}

impl PlatformCapabilities {
    /// Linux desktop defaults (used by `posix-minimal` demo host).
    #[must_use]
    pub fn desktop_posix() -> Self {
        Self {
            filesystem: FileSystemCapabilities {
                max_read_size: 10 * 1024 * 1024,
                max_write_size: 10 * 1024 * 1024,
                supports_symlinks: true,
                supports_watch: true,
                sandbox_root: None,
            },
            process: true,
            http: true,
            mcp: true,
            worktree: true,
            swarm: true,
            os_notifications: true,
            ide_bridge: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_posix_has_process_and_worktree() {
        let caps = PlatformCapabilities::desktop_posix();
        assert!(caps.process);
        assert!(caps.worktree);
    }

    #[test]
    fn capabilities_roundtrip_json() {
        let caps = PlatformCapabilities::desktop_posix();
        let s = serde_json::to_string(&caps).unwrap();
        let _: PlatformCapabilities = serde_json::from_str(&s).unwrap();
    }
}
