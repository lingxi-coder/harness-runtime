//! Each scenario runs in a fresh, single-threaded child process. The test
//! driver never changes its own process environment.

use openssl_probe::{
    has_ssl_cert_env_vars, probe, try_init_openssl_env_vars, ENV_CERT_DIR, ENV_CERT_FILE,
};
use std::env;
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
    #[cfg(unix)]
    non_utf8_paths_exist: bool,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("openssl-probe-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("first")).unwrap();
        fs::create_dir(root.join("second")).unwrap();
        fs::write(root.join("custom.pem"), b"fixture").unwrap();
        #[cfg(unix)]
        let non_utf8_paths_exist = {
            use std::os::unix::ffi::OsStringExt;
            match fs::create_dir(root.join(OsString::from_vec(b"non-utf8-dir-\xff".to_vec()))) {
                Ok(()) => {
                    fs::write(
                        root.join(OsString::from_vec(b"non-utf8-file-\xfe".to_vec())),
                        b"fixture",
                    )
                    .unwrap();
                    true
                }
                Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
                    println!(
                        "skip existing non-UTF8 paths: filesystem rejects invalid UTF-8 (EILSEQ)"
                    );
                    false
                }
                Err(error) => panic!("could not create non-UTF8 path fixture: {error}"),
            }
        };
        Fixture {
            root,
            #[cfg(unix)]
            non_utf8_paths_exist,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn run_child(mode: &str, root: &Path) {
    let file_before = env::var_os(ENV_CERT_FILE);
    let dirs_before = env::var_os(ENV_CERT_DIR);
    let result = probe();
    let before_has = has_ssl_cert_env_vars();
    let expected_found = result.cert_file.is_some() || !result.cert_dir.is_empty();
    match mode {
        "default" => {
            assert!(file_before.is_none() && dirs_before.is_none());
            assert!(!before_has);
            assert_eq!(
                result.cert_dir,
                openssl_probe::candidate_cert_dirs()
                    .map(Path::to_path_buf)
                    .collect::<Vec<_>>()
            );
            #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
            if Path::new("/etc/ssl/certs").is_dir() {
                assert!(result.cert_dir.contains(&PathBuf::from("/etc/ssl/certs")));
            }
        }
        "single-dir" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(result.cert_dir, vec![root.join("first")]);
            assert!(before_has);
        }
        "multiple-dirs" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(
                result.cert_dir,
                vec![root.join("first"), root.join("second")]
            );
            assert!(before_has);
            let raw = dirs_before.as_ref().unwrap();
            assert_eq!(env::split_paths(raw).collect::<Vec<_>>(), result.cert_dir);
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStrExt;
                assert!(raw.as_bytes().contains(&b':'));
            }
            #[cfg(windows)]
            assert!(raw.to_string_lossy().contains(';'));
        }
        "mixed-dirs" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(
                result.cert_dir,
                vec![root.join("first"), root.join("second")]
            );
            assert!(before_has);
        }
        "empty-components" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(
                result.cert_dir,
                vec![root.join("first"), root.join("second")]
            );
            assert!(before_has);
        }
        "duplicate-dirs" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(
                result.cert_dir,
                vec![root.join("first"), root.join("first")]
            );
            assert!(before_has);
        }
        "file-only" => {
            assert_eq!(result.cert_file, Some(root.join("custom.pem")));
            assert!(result.cert_dir.is_empty());
            assert!(before_has);
        }
        "both" => {
            assert_eq!(result.cert_file, Some(root.join("custom.pem")));
            assert_eq!(result.cert_dir, vec![root.join("first")]);
            assert!(before_has);
        }
        "valid-file-invalid-dir" => {
            assert_eq!(result.cert_file, Some(root.join("custom.pem")));
            assert!(result.cert_dir.is_empty());
            assert!(before_has);
        }
        "invalid-file-valid-dir" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(result.cert_dir, vec![root.join("first")]);
            assert!(before_has);
        }
        "invalid-dir" | "empty-dir" | "invalid-file" | "empty-file" | "file-is-dir"
        | "dir-is-file" => {
            assert_eq!(result.cert_file, None);
            assert!(result.cert_dir.is_empty());
            assert!(!before_has);
            assert!(!expected_found);
        }
        #[cfg(unix)]
        "non-utf8" => {
            use std::os::unix::ffi::OsStringExt;
            let file = root.join(OsString::from_vec(b"non-utf8-file-\xfe".to_vec()));
            let dir = root.join(OsString::from_vec(b"non-utf8-dir-\xff".to_vec()));
            assert_eq!(result.cert_file, Some(file));
            assert_eq!(result.cert_dir, vec![root.join("first"), dir]);
            assert!(before_has);
        }
        #[cfg(unix)]
        "non-utf8-invalid" => {
            assert_eq!(result.cert_file, None);
            assert_eq!(result.cert_dir, vec![root.join("first")]);
            assert!(before_has);
        }
        #[cfg(unix)]
        "non-utf8-invalid-only" => {
            assert_eq!(result.cert_file, None);
            assert!(result.cert_dir.is_empty());
            assert!(!before_has);
            assert!(!expected_found);
        }
        _ => panic!("unknown child case: {mode}"),
    }

    // This process has only its main thread and has not spawned children. No
    // concurrent access to the environment can occur during initialization.
    let initialized = unsafe { try_init_openssl_env_vars() };
    assert_eq!(initialized, expected_found);
    if mode == "default" {
        assert_eq!(
            env::var_os(ENV_CERT_FILE),
            result
                .cert_file
                .as_ref()
                .map(|path| path.as_os_str().to_owned())
        );
        let expected_dirs = if result.cert_dir.is_empty() {
            None
        } else {
            Some(env::join_paths(&result.cert_dir).unwrap())
        };
        assert_eq!(env::var_os(ENV_CERT_DIR), expected_dirs);
        assert_eq!(has_ssl_cert_env_vars(), expected_found);
    } else {
        assert_eq!(
            env::var_os(ENV_CERT_FILE),
            file_before,
            "explicit file configuration changed"
        );
        assert_eq!(
            env::var_os(ENV_CERT_DIR),
            dirs_before,
            "explicit directory configuration changed"
        );
        let repeated = probe();
        assert_eq!(repeated.cert_file, result.cert_file);
        assert_eq!(repeated.cert_dir, result.cert_dir);
    }
}

fn run_case(mode: &str, root: &Path, file: Option<&OsStr>, dirs: Option<&OsStr>) {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .arg("--child")
        .arg(mode)
        .arg(root)
        .env_remove(ENV_CERT_FILE)
        .env_remove(ENV_CERT_DIR);
    if let Some(value) = file {
        command.env(ENV_CERT_FILE, value);
    }
    if let Some(value) = dirs {
        command.env(ENV_CERT_DIR, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{mode} child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("ok {mode}");
}

fn main() {
    let mut args = env::args_os();
    args.next();
    if args.next().as_deref() == Some(OsStr::new("--child")) {
        let mode = args.next().unwrap().into_string().unwrap();
        let root = PathBuf::from(args.next().unwrap());
        run_child(&mode, &root);
        return;
    }

    let fixture = Fixture::new();
    let root = &fixture.root;
    let first = root.join("first");
    let second = root.join("second");
    let missing = root.join("does-not-exist");
    let file = root.join("custom.pem");
    let multi = env::join_paths([&first, &second]).unwrap();
    let mixed = env::join_paths([&first, &missing, &second]).unwrap();
    let empty_components = env::join_paths([
        Path::new(""),
        first.as_path(),
        Path::new(""),
        second.as_path(),
        Path::new(""),
    ])
    .unwrap();
    let duplicate_dirs = env::join_paths([&first, &first]).unwrap();
    run_case("default", root, None, None);
    run_case("single-dir", root, None, Some(first.as_os_str()));
    run_case("multiple-dirs", root, None, Some(&multi));
    run_case("mixed-dirs", root, None, Some(&mixed));
    run_case("empty-components", root, None, Some(&empty_components));
    run_case("duplicate-dirs", root, None, Some(&duplicate_dirs));
    run_case("file-only", root, Some(file.as_os_str()), None);
    run_case(
        "both",
        root,
        Some(file.as_os_str()),
        Some(first.as_os_str()),
    );
    run_case(
        "valid-file-invalid-dir",
        root,
        Some(file.as_os_str()),
        Some(missing.as_os_str()),
    );
    run_case(
        "invalid-file-valid-dir",
        root,
        Some(missing.as_os_str()),
        Some(first.as_os_str()),
    );
    run_case("invalid-dir", root, None, Some(missing.as_os_str()));
    run_case("empty-dir", root, None, Some(OsStr::new("")));
    run_case("invalid-file", root, Some(missing.as_os_str()), None);
    run_case("empty-file", root, Some(OsStr::new("")), None);
    run_case("file-is-dir", root, Some(first.as_os_str()), None);
    run_case("dir-is-file", root, None, Some(file.as_os_str()));
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let non_utf8_dir = root.join(OsString::from_vec(b"non-utf8-dir-\xff".to_vec()));
        let non_utf8_file = root.join(OsString::from_vec(b"non-utf8-file-\xfe".to_vec()));
        if fixture.non_utf8_paths_exist {
            let dirs = env::join_paths([&first, &non_utf8_dir]).unwrap();
            run_case(
                "non-utf8",
                root,
                Some(non_utf8_file.as_os_str()),
                Some(&dirs),
            );
        }
        let missing_dir = root.join(OsString::from_vec(b"missing-dir-\xff".to_vec()));
        let missing_file = root.join(OsString::from_vec(b"missing-file-\xfe".to_vec()));
        let mixed_non_utf8 = env::join_paths([&first, &missing_dir]).unwrap();
        run_case(
            "non-utf8-invalid",
            root,
            Some(missing_file.as_os_str()),
            Some(&mixed_non_utf8),
        );
        run_case(
            "non-utf8-invalid-only",
            root,
            Some(missing_file.as_os_str()),
            Some(missing_dir.as_os_str()),
        );
    }
}
