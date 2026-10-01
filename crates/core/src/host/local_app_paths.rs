//! Product-specific guest paths for Local Apps build profiles.
//!
//! The literals live in `local-app-contracts`, which the build host and the
//! guest both name; the engine keeps its old module path for them.
pub use local_app_contracts::guest_paths::{
    local_app_build_project, LOCAL_APP_BUILD_PROJECT_DIR, LOCAL_APP_BUILD_ROOT,
    LOCAL_APP_DEPENDENCY_STORE,
};

#[cfg(test)]
mod tests {
    /// Byte-pins the atlas atoms. The Swift twin (`LXISHGuestPaths`)
    /// pins the SAME literals — drift on either side fails one of the
    /// twins.
    #[test]
    fn atlas_atoms_are_pinned() {
        assert_eq!(mobile_linux_api::guest_paths::HOME, "/root");
        assert_eq!(
            mobile_linux_api::guest_paths::SCRATCH,
            &["/tmp", "/var/tmp"]
        );
        assert_eq!(mobile_linux_api::guest_paths::WORKSPACE_ROOT, "/workspace");
        assert_eq!(super::LOCAL_APP_BUILD_ROOT, "/var/lingxi/local-app-build");
        assert_eq!(
            super::LOCAL_APP_DEPENDENCY_STORE,
            "/var/lingxi/local-app-dependency-store"
        );
        assert_eq!(super::LOCAL_APP_BUILD_PROJECT_DIR, "project");
        assert_eq!(
            mobile_linux_api::guest_paths::workspace("abc-123"),
            "/workspace/abc-123"
        );
        assert_eq!(
            super::local_app_build_project("abc-123", "store"),
            "/var/lingxi/local-app-build/abc-123/store/project"
        );
        assert_eq!(
            mobile_linux_api::guest_paths::writable_roots(),
            ["/root", "/tmp", "/var/tmp", "/workspace"]
        );
    }
}
