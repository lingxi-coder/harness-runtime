//! Startup config migrations — port of claude-code `runMigrations()`
//! (`main.tsx:323-353`, `CURRENT_MIGRATION_VERSION = 13`) plus the
//! `~/.lingxi.json` `GlobalConfig` substrate it requires (`utils/config.ts`).
//!
//! Desktop-only: wired in `apps/cli` pre-REPL; NEVER part of the
//! engine-mobile dependency tree.
//!
//! Excluded from the port (with reasons): `migrateFennecToOpus` (dead code in
//! the external build — `if ("external" === 'ant')`). The 2.1.245 runner also
//! includes `migrateUserIntentToSettings`; its 15-key/default matrix is
//! mirrored in [`migrate_user_intent_to_settings`].

#![forbid(unsafe_code)]

pub mod changelog;
pub mod context;
pub mod global_config;
pub mod migrate_auto_updates;
pub mod migrate_bypass_permissions;
pub mod migrate_legacy_opus;
pub mod migrate_mcp_servers;
pub mod migrate_model_alias_map;
pub mod migrate_notification_dismissals;
pub mod migrate_opus_to_opus1m;
pub mod migrate_repl_bridge;
pub mod migrate_reset_auto_mode_opt_in;
pub mod migrate_reset_pro_to_opus;
pub mod migrate_sonnet1m_to_sonnet45;
pub mod migrate_sonnet45_to_46;
pub mod migrate_user_intent_to_settings;
pub mod runner;
pub mod settings_update;

pub use changelog::migrate_changelog_from_config;
pub use context::{MigrationContext, MigrationEnv};
pub use runner::{run_migrations, CURRENT_MIGRATION_VERSION};

#[cfg(test)]
mod test_support;
