//! Where an app is built, inside the isolated environment the build runs in.
//!
//! These are literals the build host and the guest agree on, so a drift on
//! either side breaks every build; the test pins them.

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
    use super::*;

    #[test]
    fn the_guest_literals_are_pinned() {
        assert_eq!(LOCAL_APP_BUILD_ROOT, "/var/lingxi/local-app-build");
        assert_eq!(
            LOCAL_APP_DEPENDENCY_STORE,
            "/var/lingxi/local-app-dependency-store"
        );
        assert_eq!(LOCAL_APP_BUILD_PROJECT_DIR, "project");
        assert_eq!(
            local_app_build_project("abc-123", "store"),
            "/var/lingxi/local-app-build/abc-123/store/project"
        );
    }
}
