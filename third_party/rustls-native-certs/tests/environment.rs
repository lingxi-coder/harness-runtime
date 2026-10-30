//! Each case runs in a new child process with its own certificate environment.
//! The driver does not mutate its own process environment.

use rustls_native_certs::{load_certs_from_paths, load_native_certs};
use std::env;
use std::ffi::OsStr;
#[cfg(unix)]
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const FILE: &str = "SSL_CERT_FILE";
const DIR: &str = "SSL_CERT_DIR";
const CERT_ONE: &[u8] = include_bytes!("fixtures/example-root-1.pem");
const CERT_TWO: &[u8] = include_bytes!("fixtures/example-root-2.pem");

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
        let root = env::temp_dir().join(format!(
            "native-certs-env-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        for (name, cert) in [("first", CERT_ONE), ("second", CERT_TWO)] {
            let dir = root.join(name);
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("root.pem"), cert).unwrap();
            fs::write(root.join(format!("{name}.pem")), cert).unwrap();
        }
        #[cfg(unix)]
        let non_utf8_paths_exist = {
            use std::os::unix::ffi::OsStringExt;
            let dir = root.join(OsString::from_vec(b"dir-\xff".to_vec()));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    fs::write(dir.join("root.pem"), CERT_TWO).unwrap();
                    fs::write(
                        root.join(OsString::from_vec(b"file-\xfe.pem".to_vec())),
                        CERT_ONE,
                    )
                    .unwrap();
                    true
                }
                Err(error) if cfg!(target_os = "macos") && error.raw_os_error() == Some(92) => {
                    println!("skip existing non-UTF8 paths: filesystem rejects them (EILSEQ)");
                    false
                }
                Err(error) => panic!("could not create non-UTF8 fixture: {error}"),
            }
        };
        Self {
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
    let before_file = env::var_os(FILE);
    let before_dir = env::var_os(DIR);
    let mut first = load_certs_from_paths(Some(&root.join("first.pem")), None);
    let mut second = load_certs_from_paths(Some(&root.join("second.pem")), None);
    assert!(first.errors.is_empty() && second.errors.is_empty());
    assert_eq!(first.certs.len(), 1);
    assert_eq!(second.certs.len(), 1);
    assert_ne!(first.certs, second.certs);
    let one = first.certs.clone();
    let two = second.certs.clone();
    first.certs.append(&mut second.certs);
    first
        .certs
        .sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    first.certs.dedup();
    let both = first.certs;
    let result = load_native_certs();
    assert_eq!(env::var_os(FILE), before_file, "file environment changed");
    assert_eq!(
        env::var_os(DIR),
        before_dir,
        "directory environment changed"
    );
    match mode {
        "default" => {
            assert!(before_file.is_none() && before_dir.is_none());
        }
        "empty-dir" | "separator-only" => {
            assert!(before_file.is_none() && before_dir.is_some());
            assert!(result.certs.is_empty());
            assert!(result.errors.is_empty());
        }
        "empty-file" | "invalid-file" | "invalid-dir" => {
            assert!(result.certs.is_empty());
            assert_eq!(result.errors.len(), 1);
        }
        "both-empty" => {
            assert!(before_file.is_some() && before_dir.is_some());
            assert!(result.certs.is_empty());
            assert_eq!(result.errors.len(), 1);
        }
        "file-only" | "single-dir" | "empty-components" => {
            assert_eq!(result.certs, one);
            assert!(result.errors.is_empty());
        }
        "multiple-dirs" | "both" => {
            assert_eq!(result.certs, both);
            assert!(result.errors.is_empty());
        }
        "valid-file-invalid-dir" => {
            assert_eq!(result.certs, one);
            assert_eq!(result.errors.len(), 1);
        }
        "invalid-file-valid-dir" => {
            assert_eq!(result.certs, two);
            assert_eq!(result.errors.len(), 1);
        }
        #[cfg(unix)]
        "non-utf8-valid" => {
            assert_eq!(result.certs, both);
            assert!(result.errors.is_empty());
        }
        #[cfg(unix)]
        "non-utf8-invalid" => {
            assert!(result.certs.is_empty());
            assert_eq!(result.errors.len(), 2);
        }
        _ => panic!("unknown child case {mode}"),
    }
    println!(
        "certificates={} errors={}",
        result.certs.len(),
        result.errors.len()
    );
}

fn run_case(mode: &str, root: &Path, file: Option<&OsStr>, dir: Option<&OsStr>) {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .arg("--child")
        .arg(mode)
        .arg(root)
        .env_remove(FILE)
        .env_remove(DIR);
    if let Some(value) = file {
        command.env(FILE, value);
    }
    if let Some(value) = dir {
        command.env(DIR, value);
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{mode} child failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    println!(
        "ok {mode} {}",
        String::from_utf8_lossy(&result.stdout).trim()
    );
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
    let first_file = root.join("first.pem");
    let first_dir = root.join("first");
    let second_dir = root.join("second");
    let missing = root.join("not-present");
    let dirs = env::join_paths([&first_dir, &second_dir]).unwrap();
    let separators = env::join_paths([Path::new(""), Path::new("")]).unwrap();
    let empty_components =
        env::join_paths([Path::new(""), first_dir.as_path(), Path::new("")]).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        assert!(dirs.as_bytes().contains(&b':'));
    }
    #[cfg(windows)]
    assert!(dirs.to_string_lossy().contains(';'));
    run_case("default", root, None, None);
    run_case("empty-dir", root, None, Some(OsStr::new("")));
    run_case("separator-only", root, None, Some(&separators));
    run_case("empty-file", root, Some(OsStr::new("")), None);
    run_case("invalid-file", root, Some(missing.as_os_str()), None);
    run_case("invalid-dir", root, None, Some(missing.as_os_str()));
    run_case(
        "both-empty",
        root,
        Some(OsStr::new("")),
        Some(OsStr::new("")),
    );
    run_case("file-only", root, Some(first_file.as_os_str()), None);
    run_case("single-dir", root, None, Some(first_dir.as_os_str()));
    run_case("empty-components", root, None, Some(&empty_components));
    run_case("multiple-dirs", root, None, Some(&dirs));
    run_case(
        "both",
        root,
        Some(first_file.as_os_str()),
        Some(second_dir.as_os_str()),
    );
    run_case(
        "valid-file-invalid-dir",
        root,
        Some(first_file.as_os_str()),
        Some(missing.as_os_str()),
    );
    run_case(
        "invalid-file-valid-dir",
        root,
        Some(missing.as_os_str()),
        Some(second_dir.as_os_str()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        if fixture.non_utf8_paths_exist {
            let file = root.join(OsString::from_vec(b"file-\xfe.pem".to_vec()));
            let dir = root.join(OsString::from_vec(b"dir-\xff".to_vec()));
            run_case(
                "non-utf8-valid",
                root,
                Some(file.as_os_str()),
                Some(dir.as_os_str()),
            );
        }
        let file = root.join(OsString::from_vec(b"missing-file-\xfe".to_vec()));
        let dir = root.join(OsString::from_vec(b"missing-dir-\xff".to_vec()));
        run_case(
            "non-utf8-invalid",
            root,
            Some(file.as_os_str()),
            Some(dir.as_os_str()),
        );
    }
}
