//! Asserts the LITERAL `~/.lingxi/ide/<port>.lock` filename and JSON shape
//! from claude-code's `src/utils/ide.ts` (`LockfileJsonContent` type).

use bridge::lockfile::{IdeLockfile, LockfileGuard};
use serde_json::Value;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn auth_token_is_32_hex_lowercase() {
    let t = IdeLockfile::generate_auth_token();
    assert_eq!(t.len(), 32, "auth token must be 32 chars");
    assert!(
        t.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "auth token must be lowercase hex, got {t:?}"
    );
}

#[test]
fn lockfile_path_is_port_dot_lock_under_ide_dir() {
    let tmp = TempDir::new().unwrap();
    let lf = IdeLockfile::new_for_ide_dir(
        tmp.path().to_path_buf(),
        40729,
        vec![PathBuf::from("/work/proj")],
    );
    let path = lf.path();
    let filename = path.file_name().unwrap().to_str().unwrap();
    assert_eq!(filename, "40729.lock");
    assert_eq!(path.parent().unwrap(), tmp.path());
}

#[test]
fn lockfile_json_uses_camelcase_keys_matching_claude_code() {
    let tmp = TempDir::new().unwrap();
    let lf = IdeLockfile::new_for_ide_dir(
        tmp.path().to_path_buf(),
        40729,
        vec![PathBuf::from("/work/proj")],
    );
    lf.write().expect("write lockfile");
    let raw = std::fs::read_to_string(lf.path()).unwrap();
    let v: Value = serde_json::from_str(&raw).expect("valid JSON");
    // Verify EXACT key set — no extras, no missing.
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "authToken",
            "ideName",
            "pid",
            "runningInWindows",
            "transport",
            "workspaceFolders",
        ],
        "lockfile must have EXACTLY these camelCase keys"
    );
    // Spot-check critical fields.
    assert_eq!(obj["pid"].as_u64().unwrap(), u64::from(std::process::id()));
    assert_eq!(obj["transport"].as_str().unwrap(), "ws");
    assert_eq!(
        obj["runningInWindows"].as_bool().unwrap(),
        cfg!(target_os = "windows")
    );
    assert_eq!(obj["ideName"].as_str().unwrap(), "LingXi");
    assert_eq!(
        obj["workspaceFolders"].as_array().unwrap()[0]
            .as_str()
            .unwrap(),
        "/work/proj"
    );
    let token = obj["authToken"].as_str().unwrap();
    assert_eq!(token.len(), 32);
    assert!(token
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn drop_guard_removes_lockfile_on_drop() {
    let tmp = TempDir::new().unwrap();
    let path = {
        let lf = IdeLockfile::new_for_ide_dir(
            tmp.path().to_path_buf(),
            40730,
            vec![PathBuf::from("/work/proj")],
        );
        lf.write().expect("write lockfile");
        let path = lf.path();
        let _guard = LockfileGuard::new(path.clone());
        assert!(path.exists(), "lockfile must exist while guard alive");
        path
        // _guard drops here.
    };
    assert!(
        !path.exists(),
        "lockfile must be deleted when LockfileGuard drops"
    );
}

#[test]
fn drop_guard_removes_lockfile_on_panic() {
    let tmp = TempDir::new().unwrap();
    let lf = IdeLockfile::new_for_ide_dir(
        tmp.path().to_path_buf(),
        40731,
        vec![PathBuf::from("/work/proj")],
    );
    lf.write().expect("write lockfile");
    let path = lf.path();
    let result = std::panic::catch_unwind(|| {
        let _guard = LockfileGuard::new(path.clone());
        panic!("simulated crash");
    });
    assert!(result.is_err(), "panic should propagate from closure");
    assert!(!path.exists(), "lockfile must be deleted even after panic");
}

// ── F2-04: dedicated bridge discovery lockfile ────────────────────────────
//
// The bridge writes its OWN `<port>.lock` under `~/.lingxi/bridge/` with a
// distinct `ideName` so it does NOT collide with the real IDE peer that scans
// `~/.lingxi/ide/` for the `IDE_NAME = "LingXi"` file.

#[test]
fn bridge_lockfile_writes_to_bridge_dir() {
    use bridge::lockfile::BRIDGE_IDE_NAME;
    let tmp = TempDir::new().unwrap();
    // `~/.lingxi/bridge` is mirrored here by `<tmp>/.lingxi/bridge`.
    let bridge_dir = tmp.path().join(".lingxi").join("bridge");
    let lf = IdeLockfile::new_for_bridge_dir(
        bridge_dir.clone(),
        40740,
        vec![PathBuf::from("/work/proj")],
    );
    // Filename is `<port>.lock` rooted at the bridge dir (NOT the ide dir).
    let path = lf.path();
    assert_eq!(path.parent().unwrap(), bridge_dir.as_path());
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), "40740.lock");

    std::fs::create_dir_all(&bridge_dir).unwrap();
    lf.write().expect("write bridge lockfile");
    assert!(
        path.exists(),
        "bridge lockfile must be written to bridge dir"
    );

    // Round-trips with the distinct bridge ideName and a real auth token.
    let (body, port) = IdeLockfile::read(&path).unwrap();
    assert_eq!(port, 40740);
    assert_eq!(body.transport, "ws");
    assert_eq!(body.ide_name, BRIDGE_IDE_NAME);
    assert_eq!(body.auth_token.len(), 32);
}

#[test]
fn bridge_lockfile_uses_distinct_ide_name() {
    use bridge::lockfile::{BRIDGE_IDE_NAME, IDE_NAME};
    // The bridge ideName MUST differ from the IDE-peer ideName so the real
    // IDE scanner does not pick up the bridge's lockfile (peer collision).
    assert_ne!(
        BRIDGE_IDE_NAME, IDE_NAME,
        "bridge ideName must differ from the IDE-peer ideName"
    );

    let tmp = TempDir::new().unwrap();
    let bridge_dir = tmp.path().to_path_buf();
    let lf = IdeLockfile::new_for_bridge_dir(bridge_dir, 40741, vec![PathBuf::from("/work/proj")]);
    assert_eq!(
        lf.body().ide_name,
        BRIDGE_IDE_NAME,
        "bridge lockfile body must carry the distinct bridge ideName"
    );
}

#[test]
fn bridge_lockfile_drop_cleans_up() {
    let tmp = TempDir::new().unwrap();
    let bridge_dir = tmp.path().to_path_buf();
    let path = {
        let lf =
            IdeLockfile::new_for_bridge_dir(bridge_dir, 40742, vec![PathBuf::from("/work/proj")]);
        lf.write().expect("write bridge lockfile");
        let path = lf.path();
        let _guard = LockfileGuard::new(path.clone());
        assert!(
            path.exists(),
            "bridge lockfile must exist while guard alive"
        );
        path
        // _guard drops here.
    };
    assert!(
        !path.exists(),
        "bridge lockfile must be deleted when LockfileGuard drops"
    );
}

#[cfg(unix)]
#[test]
fn lockfile_is_private_regular_and_exclusive() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let bridge_dir = tmp.path().join("bridge");
    let lf = IdeLockfile::new_for_bridge_dir(bridge_dir.clone(), 43123, vec![]);
    lf.write().expect("first exclusive write succeeds");

    let dir_mode = std::fs::metadata(&bridge_dir).unwrap().permissions().mode() & 0o777;
    let file_metadata = std::fs::symlink_metadata(lf.path()).unwrap();
    assert_eq!(dir_mode, 0o700);
    assert!(file_metadata.file_type().is_file());
    assert_eq!(file_metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        lf.write().unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists,
        "an existing lockfile must never be overwritten"
    );
}

#[cfg(unix)]
#[test]
fn lockfile_write_refuses_symlink_targets_and_directories() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let real_dir = tmp.path().join("real");
    std::fs::create_dir(&real_dir).unwrap();
    let linked_dir = tmp.path().join("linked");
    symlink(&real_dir, &linked_dir).unwrap();
    let via_link = IdeLockfile::new_for_bridge_dir(linked_dir, 43124, vec![]);
    assert!(
        via_link.write().is_err(),
        "symlink directory must be refused"
    );

    let bridge_dir = tmp.path().join("bridge");
    std::fs::create_dir(&bridge_dir).unwrap();
    let target = tmp.path().join("target");
    std::fs::write(&target, b"do not overwrite").unwrap();
    let lf = IdeLockfile::new_for_bridge_dir(bridge_dir, 43125, vec![]);
    symlink(&target, lf.path()).unwrap();
    assert!(lf.write().is_err(), "symlink lockfile must be refused");
    assert_eq!(std::fs::read(&target).unwrap(), b"do not overwrite");
}
