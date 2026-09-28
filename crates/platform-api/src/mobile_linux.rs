//! Compatibility paths for the standalone mobile Linux SDK.

pub use mobile_linux_api::mobile_linux::*;

/// Product guest-path profile layered on the SDK's portable conventions.
pub mod guest_paths {
    pub use mobile_linux_api::guest_paths::*;
    /// Root of the local-app build channels.
    pub const LOCAL_APP_BUILD_ROOT: &str = "/var/lingxi/local-app-build";
    /// Host-owned pnpm content-addressable store used only during dependency
    /// installation. It is never mounted for Vite builds or generated code.
    pub const LOCAL_APP_DEPENDENCY_STORE: &str = "/var/lingxi/local-app-dependency-store";
    /// The project-root leaf below a local-app build channel.
    pub const LOCAL_APP_BUILD_PROJECT_DIR: &str = "project";

    /// The isolated project root used by a local-app build.
    #[must_use]
    pub fn local_app_build_project(app_id: &str, channel: &str) -> String {
        format!("{LOCAL_APP_BUILD_ROOT}/{app_id}/{channel}/{LOCAL_APP_BUILD_PROJECT_DIR}")
    }

    #[cfg(test)]
    mod tests {
        /// Byte-pins the atlas atoms. The Swift twin (`LXISHGuestPaths`)
        /// pins the SAME literals — drift on either side fails one of the
        /// twins.
        #[test]
        fn atlas_atoms_are_pinned() {
            assert_eq!(super::HOME, "/root");
            assert_eq!(super::SCRATCH, &["/tmp", "/var/tmp"]);
            assert_eq!(super::WORKSPACE_ROOT, "/workspace");
            assert_eq!(super::LOCAL_APP_BUILD_ROOT, "/var/lingxi/local-app-build");
            assert_eq!(
                super::LOCAL_APP_DEPENDENCY_STORE,
                "/var/lingxi/local-app-dependency-store"
            );
            assert_eq!(super::LOCAL_APP_BUILD_PROJECT_DIR, "project");
            assert_eq!(super::workspace("abc-123"), "/workspace/abc-123");
            assert_eq!(
                super::local_app_build_project("abc-123", "store"),
                "/var/lingxi/local-app-build/abc-123/store/project"
            );
            assert_eq!(
                super::writable_roots(),
                ["/root", "/tmp", "/var/tmp", "/workspace"]
            );
        }
    }
}
