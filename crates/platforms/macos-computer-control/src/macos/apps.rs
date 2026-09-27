//! Installed-application enumeration.
//!
//! Parity target: claude-code's `listInstalledApps` (Spotlight-backed, via
//! Swift). Rather than depending on Spotlight being indexed, this scans the
//! standard app roots directly and reads each bundle's `Info.plist` —
//! deterministic and matches the same `PATH_ALLOWLIST` roots upstream's
//! `appNames.ts` uses to decide what counts as "user-facing" (`/Applications/`,
//! `/System/Applications/`, `~/Applications/`).

use platform_api::computer_control::AppInfo;
use std::path::{Path, PathBuf};

fn roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        roots.push(PathBuf::from(home).join("Applications"));
    }
    roots
}

fn read_app_bundle(app_dir: &Path) -> Option<AppInfo> {
    let plist_path = app_dir.join("Contents").join("Info.plist");
    let value = plist::Value::from_file(&plist_path).ok()?;
    let dict = value.as_dictionary()?;
    let bundle_id = dict.get("CFBundleIdentifier")?.as_string()?.to_string();
    let display_name = dict
        .get("CFBundleDisplayName")
        .and_then(plist::Value::as_string)
        .or_else(|| dict.get("CFBundleName").and_then(plist::Value::as_string))
        .map_or_else(
            || {
                app_dir
                    .file_stem()
                    .map_or_else(|| bundle_id.clone(), |s| s.to_string_lossy().into_owned())
            },
            str::to_string,
        );
    Some(AppInfo {
        bundle_id,
        display_name,
    })
}

/// Scan every root for top-level `*.app` bundles (non-recursive — nested
/// helper bundles inside `Contents/` are not top-level apps) and parse each
/// one's `Info.plist`. Unreadable bundles are skipped, not errors — a
/// malformed third-party app shouldn't blank the whole list.
#[must_use]
pub fn list_installed_apps() -> Vec<AppInfo> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for root in roots() {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("app") {
                continue;
            }
            if let Some(info) = read_app_bundle(&path) {
                if seen.insert(info.bundle_id.clone()) {
                    out.push(info);
                }
            }
        }
    }
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `<tmp>/<name>.app/Contents/Info.plist` with the given key/value
    /// pairs (string values only — enough for the fields `read_app_bundle`
    /// reads) and return the bundle directory path.
    fn make_bundle(tmp: &Path, name: &str, entries: &[(&str, &str)]) -> PathBuf {
        let bundle_dir = tmp.join(format!("{name}.app"));
        let contents_dir = bundle_dir.join("Contents");
        std::fs::create_dir_all(&contents_dir).unwrap();
        let mut dict = plist::Dictionary::new();
        for (k, v) in entries {
            dict.insert((*k).to_string(), plist::Value::String((*v).to_string()));
        }
        plist::Value::Dictionary(dict)
            .to_file_xml(contents_dir.join("Info.plist"))
            .unwrap();
        bundle_dir
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lx-computer-use-apps-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prefers_display_name_over_bundle_name() {
        let tmp = scratch_dir("display-name");
        let bundle = make_bundle(
            &tmp,
            "Foo",
            &[
                ("CFBundleIdentifier", "com.example.foo"),
                ("CFBundleDisplayName", "Foo Display"),
                ("CFBundleName", "Foo Bundle"),
            ],
        );
        let info = read_app_bundle(&bundle).expect("bundle should parse");
        assert_eq!(info.bundle_id, "com.example.foo");
        assert_eq!(info.display_name, "Foo Display");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn falls_back_to_bundle_name_when_display_name_is_absent() {
        let tmp = scratch_dir("bundle-name-fallback");
        let bundle = make_bundle(
            &tmp,
            "Bar",
            &[
                ("CFBundleIdentifier", "com.example.bar"),
                ("CFBundleName", "Bar Bundle"),
            ],
        );
        let info = read_app_bundle(&bundle).expect("bundle should parse");
        assert_eq!(info.display_name, "Bar Bundle");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn falls_back_to_file_stem_when_no_name_fields_are_present() {
        let tmp = scratch_dir("file-stem-fallback");
        let bundle = make_bundle(&tmp, "Baz", &[("CFBundleIdentifier", "com.example.baz")]);
        let info = read_app_bundle(&bundle).expect("bundle should parse");
        assert_eq!(info.display_name, "Baz");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_bundle_identifier_yields_none() {
        let tmp = scratch_dir("missing-id");
        let bundle = make_bundle(&tmp, "NoId", &[("CFBundleName", "No Id")]);
        assert!(read_app_bundle(&bundle).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn missing_info_plist_yields_none() {
        let tmp = scratch_dir("missing-plist");
        let bundle_dir = tmp.join("Empty.app");
        std::fs::create_dir_all(bundle_dir.join("Contents")).unwrap();
        assert!(read_app_bundle(&bundle_dir).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
