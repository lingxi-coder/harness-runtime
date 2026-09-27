//! Git metadata for version build scripts, including linked worktrees.
use std::path::{Path, PathBuf};
use std::process::Command;

fn git_output(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim_end_matches(['\r', '\n']);
    (!value.is_empty()).then(|| value.to_owned())
}

fn git_path(cwd: &Path, name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git_output(cwd, &["rev-parse", "--git-path", name])?);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn watch_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(head) = git_path(cwd, "HEAD").and_then(|path| path.canonicalize().ok()) {
        paths.push(head);
    }
    if let Some(reference) = git_output(cwd, &["symbolic-ref", "--quiet", "HEAD"]) {
        if let Some(mut path) = git_path(cwd, &reference) {
            // An unborn or packed branch has no loose ref yet. Watch its
            // nearest existing directory so creation of that ref is observed.
            while !path.exists() && path.pop() {}
            if let Ok(path) = path.canonicalize() {
                paths.push(path);
            }
        }
        if let Some(packed) = git_path(cwd, "packed-refs").and_then(|path| path.canonicalize().ok())
        {
            paths.push(packed);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// Emit only existing Git watch paths; nonexistent paths make Cargo rebuild
/// on every invocation. The caller also tracks its own build script.
pub fn emit() {
    let cwd = std::env::current_dir().expect("build script requires its crate directory");
    let sha =
        git_output(&cwd, &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    // The name is assembled from `branding::ENV_PREFIX` rather than spelled
    // out: the brand-leak gate tracks every hardcoded brand token, and the
    // reading end (`option_env!` in commands/core/src/version.rs) already
    // has to carry one literal that a macro cannot compute.
    println!(
        "cargo:rustc-env={}GIT_SHA_SHORT={sha}",
        branding::ENV_PREFIX
    );
    for path in watch_paths(&cwd) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for name in ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "lingxi-git-metadata-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            // Exclusive creation: never take ownership of a preexisting path.
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn git(&self, args: &[&str]) {
            self.git_at(&self.0, args);
        }
        fn git_at(&self, cwd: &Path, args: &[&str]) {
            let result = Command::new("git")
                .current_dir(cwd)
                .env_remove("GIT_DIR")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_OBJECT_DIRECTORY")
                .args(args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        fn init(&self) {
            self.git(&["init", "--quiet"]);
            self.git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        }
        fn commit(&self) {
            self.git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture",
            ]);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn archive_has_no_nonexistent_git_watches() {
        let fixture = Fixture::new();
        assert!(watch_paths(&fixture.0).is_empty());
        assert!(git_output(&fixture.0, &["rev-parse", "--short", "HEAD"]).is_none());
    }

    #[test]
    fn unborn_loose_and_packed_refs_are_watched() {
        let fixture = Fixture::new();
        fixture.init();
        let head = fixture.0.join(".git/HEAD").canonicalize().unwrap();
        let refs = fixture.0.join(".git/refs/heads").canonicalize().unwrap();
        assert_eq!(watch_paths(&fixture.0), vec![head.clone(), refs.clone()]);
        fixture.commit();
        let loose = fixture
            .0
            .join(".git/refs/heads/main")
            .canonicalize()
            .unwrap();
        assert_eq!(watch_paths(&fixture.0), vec![head.clone(), loose]);
        fixture.git(&["pack-refs", "--all"]);
        let packed = fixture.0.join(".git/packed-refs").canonicalize().unwrap();
        assert_eq!(watch_paths(&fixture.0), vec![head, packed, refs]);
        assert!(watch_paths(&fixture.0).iter().all(|path| path.exists()));
    }

    #[test]
    fn linked_worktree_and_detached_head_use_actual_git_paths() {
        let fixture = Fixture::new();
        fixture.init();
        fixture.commit();
        let linked = fixture.0.join("linked");
        fixture.git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "codex/probe",
            linked.to_str().unwrap(),
        ]);
        assert!(linked.join(".git").is_file());
        let nested = linked.join("lingxi-code/commands/core");
        std::fs::create_dir_all(&nested).unwrap();
        let head = git_path(&nested, "HEAD").unwrap().canonicalize().unwrap();
        let reference = git_path(&nested, "refs/heads/codex/probe")
            .unwrap()
            .canonicalize()
            .unwrap();
        let paths = watch_paths(&nested);
        assert!(paths.contains(&head) && paths.contains(&reference));
        assert!(paths.iter().all(|path| path.is_absolute() && path.exists()));
        fixture.git_at(&linked, &["checkout", "--quiet", "--detach"]);
        assert_eq!(watch_paths(&nested), vec![head]);
    }
}
