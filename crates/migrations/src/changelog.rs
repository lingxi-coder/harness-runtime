//! `migrateChangelogFromConfig` (`releaseNotes.ts:55-76`) — move the
//! deprecated `cachedChangelog` config field to
//! `<claude-config-home>/cache/changelog.md`. Fire-and-forget at startup
//! (the caller must `tokio::spawn` this — wired in `apps/cli`); errors are
//! silent (TS `.catch(() => {})`), retried next startup.

use crate::context::MigrationEnv;
use crate::global_config;
use serde_json::Value;

/// Run the async changelog migration.
pub async fn migrate_changelog_from_config(env: &MigrationEnv) {
    let Ok(cfg) = global_config::read_map(&env.global_config_path) else {
        return;
    };
    let Some(Value::String(changelog)) = cfg.get("cachedChangelog").cloned() else {
        return;
    };
    // TS gates on truthiness (`if (!config.cachedChangelog) return`,
    // releaseNotes.ts:57): an EMPTY string early-returns and the key is never
    // removed. Mirror that exactly.
    if changelog.is_empty() {
        return;
    }

    let cache_dir = env.lingxi_config_home.join("cache");
    let cache_path = cache_dir.join("changelog.md");
    if tokio::fs::create_dir_all(&cache_dir).await.is_ok() {
        // `wx` flag parity (`releaseNotes.ts:66`): write only if the file
        // doesn't exist; an existing file (or any write error) is silently fine.
        if let Ok(mut f) = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cache_path)
            .await
        {
            use tokio::io::AsyncWriteExt;
            if f.write_all(changelog.as_bytes()).await.is_ok() {
                // Flush + fsync before the handle drops. Tokio's `File`
                // defers the durable write to a background flush on drop,
                // which races a synchronous read-back (e.g. under parallel
                // test load): the reader can observe an empty file. Make
                // the write durable here so the data is on disk before the
                // function returns and `f` is dropped.
                let _ = f.flush().await;
                let _ = f.sync_all().await;
            }
        }
    }

    // Remove the deprecated field regardless (TS does this after the try).
    let _ = global_config::save_map(&env.global_config_path, |mut m| {
        m.remove("cachedChangelog");
        m
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_config;

    fn test_env(t: &crate::test_support::TempConfig) -> crate::context::MigrationEnv {
        crate::context::MigrationEnv {
            global_config_path: t.global.clone(),
            lingxi_config_home: t.home.clone(),
            project_dir: t.project.clone(),
            ctx: crate::context::MigrationContext {
                first_party: true,
                subscription_type: None,
            },
            bus: None,
        }
    }

    #[tokio::test]
    async fn moves_cached_changelog_to_file_and_drops_key() {
        let t = temp_config();
        std::fs::write(&t.global, r##"{"cachedChangelog": "# v1", "keep": 1}"##).unwrap();
        migrate_changelog_from_config(&test_env(&t)).await;
        let cache = t.home.join("cache").join("changelog.md");
        assert_eq!(std::fs::read_to_string(&cache).unwrap(), "# v1");
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("cachedChangelog").is_none());
        assert_eq!(m["keep"], serde_json::json!(1));
    }

    #[tokio::test]
    async fn existing_cache_file_is_not_overwritten_but_key_still_dropped() {
        let t = temp_config();
        std::fs::write(&t.global, r##"{"cachedChangelog": "# old"}"##).unwrap();
        let cache_dir = t.home.join("cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("changelog.md"), "# newer").unwrap();
        migrate_changelog_from_config(&test_env(&t)).await;
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("changelog.md")).unwrap(),
            "# newer"
        );
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert!(m.get("cachedChangelog").is_none());
    }

    #[tokio::test]
    async fn no_key_is_noop() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"a": 1}"#).unwrap();
        migrate_changelog_from_config(&test_env(&t)).await;
        assert!(!t.home.join("cache").join("changelog.md").exists());
    }

    /// TS truthiness gate (`releaseNotes.ts:57`): an EMPTY `cachedChangelog`
    /// early-returns — no cache file, key NOT removed.
    #[tokio::test]
    async fn empty_string_is_noop_and_key_kept() {
        let t = temp_config();
        std::fs::write(&t.global, r#"{"cachedChangelog": ""}"#).unwrap();
        migrate_changelog_from_config(&test_env(&t)).await;
        assert!(!t.home.join("cache").join("changelog.md").exists());
        let m = crate::global_config::read_map(&t.global).unwrap();
        assert_eq!(m["cachedChangelog"], serde_json::json!(""));
    }
}
