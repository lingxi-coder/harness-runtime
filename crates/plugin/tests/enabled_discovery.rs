//! Allowlist-driven discovery against the REAL claude-code on-disk layout
//! (GAP E verification fix #1).
//!
//! claude-code never flat-walks `plugins/*/` for manifests. Its cache-only
//! loader (`loadAllPluginsCacheOnly` → `loadPluginsFromMarketplaces`,
//! `pluginLoader.ts:1888`) is driven by `settings.enabledPlugins` — a map of
//! `plugin@marketplace` → enabled — and resolves each enabled entry through
//! `getVersionedCachePath` (`pluginLoader.ts:139`) to the versioned cache path
//!
//!     <plugins>/cache/{marketplace}/{plugin}/{version}/
//!
//! The plugin manifest lives at that versioned dir. `discover_enabled_plugins`
//! ports that resolution: it reads only the allowlisted entries and resolves
//! them to their versioned cache directory, so against a real plugins dir it
//! discovers exactly the enabled plugins (not zero, and not "every dir").

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Materialize a versioned cache entry exactly as claude-code lays it out:
/// `<root>/cache/{marketplace}/{plugin}/{version}/.lingxi-plugin/plugin.json`.
fn write_cached_plugin(root: &Path, marketplace: &str, plugin: &str, version: &str) {
    let versioned = root
        .join("cache")
        .join(marketplace)
        .join(plugin)
        .join(version);
    fs::create_dir_all(versioned.join(".lingxi-plugin")).unwrap();
    fs::write(
        versioned.join(".lingxi-plugin").join("plugin.json"),
        format!(r#"{{"name":"{plugin}","version":"{version}"}}"#),
    )
    .unwrap();
    fs::create_dir_all(versioned.join("commands")).unwrap();
    fs::write(
        versioned.join("commands").join("hi.md"),
        "---\ndescription: hi\n---\nbody\n",
    )
    .unwrap();
}

#[tokio::test]
async fn resolves_enabled_entries_to_versioned_cache_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Two cached plugins, one enabled, one disabled in the allowlist.
    write_cached_plugin(root, "acme", "weather", "1.0.0");
    write_cached_plugin(root, "acme", "calc", "2.3.1");

    // Real claude-code top-level junk that must NOT be mistaken for a plugin.
    fs::create_dir_all(root.join("npm-cache")).unwrap();
    fs::write(root.join("installed_plugins.json"), "{}").unwrap();

    let mut enabled: BTreeMap<String, bool> = BTreeMap::new();
    enabled.insert("weather@acme".to_string(), true);
    enabled.insert("calc@acme".to_string(), false); // disabled → skipped

    let discovered = plugin::discover_enabled_plugins(root, &enabled).await;

    assert_eq!(
        discovered.len(),
        1,
        "only the enabled allowlisted plugin is discovered"
    );
    let (_, manifest, dir) = &discovered[0];
    assert_eq!(manifest.name, "weather");
    assert_eq!(
        dir,
        &root
            .join("cache")
            .join("acme")
            .join("weather")
            .join("1.0.0"),
        "resolved to the versioned cache dir"
    );
    assert_eq!(manifest.components.commands.len(), 1);
}

#[tokio::test]
async fn resolves_the_recorded_version_when_multiple_cache_versions_exist() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_cached_plugin(root, "acme", "weather", "1.0.0");
    write_cached_plugin(root, "acme", "weather", "2.0.0");

    // The installed record is authoritative. Keeping both cache versions on
    // disk makes a directory probe ambiguous; the record selects 2.0.0.
    let installed = serde_json::json!({
        "version": 2,
        "plugins": {
            "weather@acme": [{
                "scope": "user",
                "installPath": root.join("cache/acme/weather/2.0.0"),
                "version": "2.0.0",
                "installedAt": "2026-08-31T00:00:00.000Z",
                "lastUpdated": "2026-08-31T00:00:00.000Z"
            }]
        }
    });
    fs::write(
        root.join("installed_plugins.json"),
        serde_json::to_string(&installed).unwrap(),
    )
    .unwrap();

    let enabled = BTreeMap::from([("weather@acme".to_string(), true)]);
    let discovered = plugin::discover_enabled_plugins(root, &enabled).await;

    assert_eq!(discovered.len(), 1);
    assert_eq!(
        discovered[0].2,
        root.join("cache/acme/weather/2.0.0"),
        "enabled discovery must use the installed record's exact version"
    );
}

#[tokio::test]
async fn an_authoritative_missing_record_does_not_probe_another_cache_version() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_cached_plugin(root, "acme", "weather", "1.0.0");
    write_cached_plugin(root, "acme", "weather", "2.0.0");

    let installed = serde_json::json!({
        "version": 2,
        "plugins": {
            "weather@acme": [{
                "scope": "user",
                "installPath": root.join("cache/acme/weather/3.0.0"),
                "version": "3.0.0"
            }]
        }
    });
    fs::write(
        root.join("installed_plugins.json"),
        serde_json::to_string(&installed).unwrap(),
    )
    .unwrap();

    let enabled = BTreeMap::from([("weather@acme".to_string(), true)]);
    assert!(
        plugin::discover_enabled_plugins(root, &enabled)
            .await
            .is_empty(),
        "a present but missing installed record must not silently select another version"
    );
}

#[tokio::test]
async fn flat_walk_of_a_real_plugins_dir_finds_nothing() {
    // The real ~/.lingxi/plugins holds cache/, npm-cache/, installed_plugins.json
    // — none with a direct .lingxi-plugin/plugin.json child. The legacy flat
    // walk must therefore find ZERO (documents the layout mismatch the
    // allowlist path fixes).
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_cached_plugin(root, "acme", "weather", "1.0.0");
    fs::create_dir_all(root.join("npm-cache")).unwrap();

    let flat = plugin::discover_installed_plugins(root).await;
    assert!(
        flat.is_empty(),
        "flat walk finds nothing against the real cache/ layout"
    );
}

#[tokio::test]
async fn missing_version_dir_for_enabled_entry_is_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    // Enabled but never installed → no cache dir.
    let mut enabled: BTreeMap<String, bool> = BTreeMap::new();
    enabled.insert("ghost@acme".to_string(), true);

    let discovered = plugin::discover_enabled_plugins(root, &enabled).await;
    assert!(
        discovered.is_empty(),
        "uninstalled enabled entry is skipped"
    );
}
