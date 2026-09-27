//! Config-migration events (`src/migrations/*.ts` `logEvent` sites). Appended
//! as their own registry block (after the `FileRead` global-tail trio).

/// `tengu_migrate_autoupdates_to_settings` (`migrateAutoUpdatesToSettings.ts:38`).
pub const MIGRATE_AUTOUPDATES_TO_SETTINGS: &str = "tengu_migrate_autoupdates_to_settings";
/// `tengu_migrate_autoupdates_error` (`migrateAutoUpdatesToSettings.ts:56`).
pub const MIGRATE_AUTOUPDATES_ERROR: &str = "tengu_migrate_autoupdates_error";
/// `tengu_migrate_bypass_permissions_accepted` (`migrateBypassPermissionsAcceptedToSettings.ts:29`).
pub const MIGRATE_BYPASS_PERMISSIONS_ACCEPTED: &str = "tengu_migrate_bypass_permissions_accepted";
/// `tengu_migrate_mcp_approval_fields_success` (`migrateEnableAllProjectMcpServersToSettings.ts:110`).
pub const MIGRATE_MCP_APPROVAL_FIELDS_SUCCESS: &str = "tengu_migrate_mcp_approval_fields_success";
/// `tengu_migrate_mcp_approval_fields_error` (`migrateEnableAllProjectMcpServersToSettings.ts:115`).
pub const MIGRATE_MCP_APPROVAL_FIELDS_ERROR: &str = "tengu_migrate_mcp_approval_fields_error";
/// `tengu_reset_pro_to_opus_default` (`resetProToOpusDefault.ts`).
pub const RESET_PRO_TO_OPUS_DEFAULT: &str = "tengu_reset_pro_to_opus_default";
/// `tengu_legacy_opus_migration` (`migrateLegacyOpusToCurrent.ts:53`).
pub const LEGACY_OPUS_MIGRATION: &str = "tengu_legacy_opus_migration";
/// `tengu_sonnet45_to_46_migration` (`migrateSonnet45ToSonnet46.ts:63`).
pub const SONNET45_TO_46_MIGRATION: &str = "tengu_sonnet45_to_46_migration";
/// `tengu_opus_to_opus1m_migration` (`migrateOpusToOpus1m.ts:41`).
pub const OPUS_TO_OPUS1M_MIGRATION: &str = "tengu_opus_to_opus1m_migration";

/// Registry block — order matches TS `runMigrations` execution order
/// (`main.tsx:328-336`), error events directly after their success twin.
pub const NAMES: [&str; 9] = [
    MIGRATE_AUTOUPDATES_TO_SETTINGS,
    MIGRATE_AUTOUPDATES_ERROR,
    MIGRATE_BYPASS_PERMISSIONS_ACCEPTED,
    MIGRATE_MCP_APPROVAL_FIELDS_SUCCESS,
    MIGRATE_MCP_APPROVAL_FIELDS_ERROR,
    RESET_PRO_TO_OPUS_DEFAULT,
    LEGACY_OPUS_MIGRATION,
    SONNET45_TO_46_MIGRATION,
    OPUS_TO_OPUS1M_MIGRATION,
];
