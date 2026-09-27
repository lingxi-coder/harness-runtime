//! Host-supplied (Kotlin → Rust) shell configuration (spec r3 §Android inputs).

use std::path::PathBuf;

/// Android shell/sandbox inputs. Provided explicitly by the Kotlin bootstrap —
/// shell support is never enabled by target OS alone.
#[derive(Debug, Clone)]
pub struct AndroidShellConfig {
    /// `ApplicationInfo.nativeLibraryDir` — the only legal root for bundled
    /// helper executables (P4+).
    pub native_library_dir: PathBuf,
    /// The directory the shell treats as `$HOME` / the workspace. A product
    /// boundary, not an OS boundary (spec Threat model).
    pub shell_workspace_root: PathBuf,
    /// App cache dir → `$TMPDIR`.
    pub app_cache_root: PathBuf,
    /// Application package name.
    pub package_name: String,
    /// `PackageInfo.longVersionCode` — capability-cache key component.
    pub package_version_code: i64,
    /// Roots that must never contain an exec target (filesDir, cacheDir,
    /// codeCacheDir, noBackupFilesDir, extracted-assets roots).
    pub app_writable_roots: Vec<PathBuf>,
    /// Master enable flag (registration gate #3).
    pub enable_shell: bool,
    /// D11: host attests no plaintext secrets live under shell-readable
    /// app-private paths (Keystore migration done).
    pub secrets_in_keystore: bool,
    /// D11: explicit user acceptance of the data-exposure reality.
    pub shell_data_exposure_accepted: bool,
    /// P5b: absolute path to the bundled mksh executable under
    /// `nativeLibraryDir` (e.g. `libmksh.so`). `None` = not bundled.
    pub bundled_mksh_path: Option<PathBuf>,
    /// P5b: content hash of the bundled mksh, for the P2 `BundledHelper`
    /// identity check. `None` = not bundled.
    pub bundled_mksh_hash: Option<String>,
    /// P5b: app-private directory holding the toybox applet symlink farm;
    /// goes first on `PATH`. `None` = not bundled.
    pub bundled_applet_dir: Option<PathBuf>,
}

impl AndroidShellConfig {
    /// D11 secrets gate (registration gate #4): Keystore attestation OR
    /// explicit user acceptance.
    #[must_use]
    pub fn secrets_gate_satisfied(&self) -> bool {
        self.secrets_in_keystore || self.shell_data_exposure_accepted
    }

    /// True only when all three bundled inputs are present (mksh path + hash + applet dir).
    #[must_use]
    pub fn bundled_ready(&self) -> bool {
        self.bundled_mksh_path.is_some()
            && self.bundled_applet_dir.is_some()
            && self.bundled_mksh_hash.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(keystore: bool, accepted: bool) -> AndroidShellConfig {
        AndroidShellConfig {
            native_library_dir: PathBuf::from("/data/app/x/lib/arm64"),
            shell_workspace_root: PathBuf::from("/data/user/0/x/files/ws"),
            app_cache_root: PathBuf::from("/data/user/0/x/cache"),
            package_name: "com.example".into(),
            package_version_code: 42,
            app_writable_roots: vec![PathBuf::from("/data/user/0/x/files")],
            enable_shell: true,
            secrets_in_keystore: keystore,
            shell_data_exposure_accepted: accepted,
            bundled_mksh_path: None,
            bundled_mksh_hash: None,
            bundled_applet_dir: None,
        }
    }

    #[test]
    fn secrets_gate_requires_keystore_or_acceptance() {
        assert!(!cfg(false, false).secrets_gate_satisfied());
        assert!(cfg(true, false).secrets_gate_satisfied());
        assert!(cfg(false, true).secrets_gate_satisfied());
    }

    #[test]
    fn bundled_fields_and_readiness() {
        let mut c = cfg(true, true); // existing test helper — reuse it
        assert!(!c.bundled_ready(), "no bundled paths -> not ready");
        c.bundled_mksh_path = Some("/nl/libmksh.so".into());
        c.bundled_applet_dir = Some("/app/applet-bin".into());
        c.bundled_mksh_hash = Some("abc".into());
        assert!(c.bundled_ready());
    }
}
