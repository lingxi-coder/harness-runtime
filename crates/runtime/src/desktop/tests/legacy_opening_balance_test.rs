use super::*;

fn write(config: &Path, cwd: &Path, entries: serde_json::Map<String, serde_json::Value>) {
    let key = migrations::global_config::project_path_for_config(cwd);
    migrations::global_config::save_project_config(config, &key, |mut project| {
        for (name, value) in entries {
            project.insert(name, value);
        }
        project
    })
    .unwrap();
}

fn entries(session: &str, cost: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("lastSessionId".into(), serde_json::json!(session));
    map.insert("lastCost".into(), cost);
    map
}

#[test]
fn imports_the_cli_written_pair_for_a_matching_session() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(branding::GLOBAL_CONFIG_FILE);
    let cwd = Path::new("/proj/alpha");
    let session = protocol::SessionId::new();
    // Exactly what `save_session_cost` writes: `session_id.to_string()`.
    write(
        &config,
        cwd,
        entries(&session.to_string(), serde_json::json!(0.0175)),
    );

    assert_eq!(
        capture_legacy_opening_balance(Some(&config), cwd),
        Some((session, 17_500_000))
    );
}

#[test]
fn refuses_another_session_another_project_and_a_missing_file() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(branding::GLOBAL_CONFIG_FILE);
    let cwd = Path::new("/proj/alpha");
    let session = protocol::SessionId::new();
    write(
        &config,
        cwd,
        entries(&session.to_string(), serde_json::json!(0.0175)),
    );

    // A different project key must not inherit alpha's balance.
    assert_eq!(
        capture_legacy_opening_balance(Some(&config), Path::new("/proj/beta")),
        None
    );
    // No config path at all (a TempDir-homed host) imports nothing.
    assert_eq!(capture_legacy_opening_balance(None, cwd), None);
    // A file with no project entry imports nothing.
    let empty = directory.path().join("empty.json");
    assert_eq!(capture_legacy_opening_balance(Some(&empty), cwd), None);
}

#[test]
fn rejects_values_that_are_not_a_positive_finite_amount() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = Path::new("/proj/alpha");
    let session = protocol::SessionId::new().to_string();
    for cost in [
        serde_json::json!(0.0),
        serde_json::json!(-1.0),
        serde_json::json!("0.5"),
        serde_json::json!(null),
    ] {
        let config = directory.path().join(format!("{cost}.json"));
        write(&config, cwd, entries(&session, cost.clone()));
        assert_eq!(
            capture_legacy_opening_balance(Some(&config), cwd),
            None,
            "{cost} is not an importable opening balance"
        );
    }
}

/// An unparseable id is not a reason to import into whatever session is
/// booting: the balance belongs to a named session or to no one.
#[test]
fn refuses_an_unparseable_session_id() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(branding::GLOBAL_CONFIG_FILE);
    let cwd = Path::new("/proj/alpha");
    write(
        &config,
        cwd,
        entries("not-a-uuid", serde_json::json!(0.0175)),
    );

    assert_eq!(capture_legacy_opening_balance(Some(&config), cwd), None);
}

/// Pins the RESULT, not the explicit clamp above it: a float-to-integer
/// `as` cast already saturates, so removing that branch leaves this green.
/// What must never change is that an absurd figure comes back as a ceiling
/// rather than a small wrapped number that would read as a real balance.
#[test]
fn saturates_instead_of_wrapping_on_an_absurd_amount() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join(branding::GLOBAL_CONFIG_FILE);
    let cwd = Path::new("/proj/alpha");
    let session = protocol::SessionId::new();
    write(
        &config,
        cwd,
        entries(&session.to_string(), serde_json::json!(1.0e30)),
    );

    assert_eq!(
        capture_legacy_opening_balance(Some(&config), cwd),
        Some((session, u64::MAX))
    );
}
