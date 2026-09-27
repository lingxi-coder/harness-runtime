//! Controlled per-app `SQLite` collection store.
//!
//! This module intentionally exposes records, filters, sorting, and bounded
//! transactions rather than SQL.  Every collection and field is validated
//! against the app manifest, and all dynamic values are bound parameters.

use crate::error::AppError;
use crate::manifest::{
    validate_identifier, AppLayout, AppManifest, DataCollectionSchema, DataFieldKind,
    DataFieldSchema,
};
use rusqlite::types::Value as SqlValue;
use rusqlite::{
    params, params_from_iter, Connection, DatabaseName, OptionalExtension, Transaction,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Current internal `SQLite` schema version.
pub const DATA_SCHEMA_VERSION: u32 = 1;
/// Largest query page accepted by the native bridge.
pub const MAX_QUERY_PAGE_SIZE: u32 = 100;
/// Largest atomic mutation batch accepted by the native bridge.
pub const MAX_MUTATION_BATCH_SIZE: usize = 50;
/// Largest filter count accepted by one query.
pub const MAX_QUERY_FILTERS: usize = 16;
/// Largest member list accepted by an `in` filter.
pub const MAX_FILTER_IN_VALUES: usize = 20;
/// Largest UTF-8 byte length accepted for a caller-owned record id.
pub const MAX_RECORD_ID_BYTES: usize = 128;
/// Largest serialized record document.
pub const MAX_RECORD_DOCUMENT_BYTES: usize = 1024 * 1024;
/// Pre-migration database copies retained under `apps/<id>/data/backups/`.
///
/// A destructive migration copies the whole database, and nothing else in the
/// tree enumerates, caps or deletes that directory, so without a bound at the
/// write site an app with a large database and a long design history keeps
/// every copy on device forever. The copy a migration just wrote always
/// occupies one of these slots and is never a prune candidate.
pub const MAX_DATABASE_BACKUPS: usize = 3;

/// One persisted collection record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataRecord {
    /// Collection id from the app manifest.
    pub collection: String,
    /// Stable caller-provided record id.
    pub record_id: String,
    /// Structured record fields.
    pub document: Map<String, Value>,
    /// Monotonic optimistic-concurrency revision, starting at one.
    pub revision: u64,
    /// Creation time in epoch milliseconds.
    pub created_at_ms: u64,
    /// Last update time in epoch milliseconds.
    pub updated_at_ms: u64,
}

/// Supported filter operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataFilterOperator {
    /// Equal to one scalar.
    Equal,
    /// Not equal to one scalar.
    NotEqual,
    /// Numerically or lexically less than one scalar.
    LessThan,
    /// Less than or equal.
    LessThanOrEqual,
    /// Numerically or lexically greater than one scalar.
    GreaterThan,
    /// Greater than or equal.
    GreaterThanOrEqual,
    /// String contains a literal substring.
    Contains,
    /// Equal to one member of a scalar array (maximum [`MAX_FILTER_IN_VALUES`]).
    In,
}

impl DataFilterOperator {
    /// Every serialized filter operator in stable catalog order.
    pub const ALL: [Self; 8] = [
        Self::Equal,
        Self::NotEqual,
        Self::LessThan,
        Self::LessThanOrEqual,
        Self::GreaterThan,
        Self::GreaterThanOrEqual,
        Self::Contains,
        Self::In,
    ];
}

/// Manifest-field filter used by [`DataQuery`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataFilter {
    /// Manifest field id.
    pub field_id: String,
    /// Restricted filter operation.
    pub operator: DataFilterOperator,
    /// Scalar value, or an array of scalars for [`DataFilterOperator::In`].
    pub value: Value,
}

/// Sort key for a collection query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "fieldId", rename_all = "snake_case")]
pub enum DataSortKey {
    /// Stable record id.
    RecordId,
    /// Creation timestamp.
    CreatedAt,
    /// Last-update timestamp.
    UpdatedAt,
    /// Record revision.
    Revision,
    /// Manifest field value.
    Field(String),
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSortDirection {
    /// Smallest/oldest first.
    Ascending,
    /// Largest/newest first.
    Descending,
}

/// Bounded collection query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataQuery {
    /// Collection id.
    pub collection: String,
    /// Restricted field filters, combined with logical AND.
    #[serde(default)]
    pub filters: Vec<DataFilter>,
    /// Optional sort key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<DataSortKey>,
    /// Sort direction.
    pub sort_direction: DataSortDirection,
    /// Page size, from 1 through [`MAX_QUERY_PAGE_SIZE`].
    pub limit: u32,
    /// Zero-based row offset.
    #[serde(default)]
    pub offset: u64,
}

/// One page returned by a controlled query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataPage {
    /// Records in requested order.
    pub records: Vec<DataRecord>,
    /// Offset for the next page, or `None` when this is the final page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
}

/// One operation in an atomic mutation batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DataMutation {
    /// Insert or replace a record document.
    #[serde(rename_all = "camelCase")]
    Upsert {
        /// Collection id.
        collection: String,
        /// Stable record id.
        record_id: String,
        /// Complete record document.
        document: Map<String, Value>,
        /// Optional optimistic-concurrency revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
    /// Delete one record.
    #[serde(rename_all = "camelCase")]
    Delete {
        /// Collection id.
        collection: String,
        /// Stable record id.
        record_id: String,
        /// Optional optimistic-concurrency revision.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_revision: Option<u64>,
    },
}

/// Result of one successful batch mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMutationResult {
    /// Collection id.
    pub collection: String,
    /// Record id.
    pub record_id: String,
    /// New revision, absent for a deletion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Whether the record was deleted.
    pub deleted: bool,
}

/// Schema metadata stored in `_lingxi_schema`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSchemaState {
    /// Internal `SQLite` schema version.
    pub schema_version: u32,
    /// SHA-256 of the currently approved app manifest, empty before first setup.
    pub manifest_hash: String,
    /// Last schema update time in epoch milliseconds.
    pub updated_at_ms: u64,
}

/// Preview produced before changing the manifest bound to a database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMigrationPreview {
    /// Existing manifest hash, absent for a new database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_manifest_hash: Option<String>,
    /// Proposed manifest hash.
    pub to_manifest_hash: String,
    /// Whether explicit destructive-migration confirmation is required.
    pub destructive: bool,
    /// Human-readable reasons why the migration is destructive.
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// Completed manifest migration and its durable pre-migration backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataMigrationResult {
    /// Applied manifest hash.
    pub manifest_hash: String,
    /// Root-relative backup path, present only for a destructive migration:
    /// first-time initialization has nothing to copy and a non-destructive
    /// change cannot stop addressing a record. At most
    /// [`MAX_DATABASE_BACKUPS`] such copies are retained per app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_rel: Option<String>,
}

/// Open handle to one app's native collection database.
pub struct AppDataStore {
    layout: AppLayout,
    connection: Connection,
    /// Manifest hash whose JSON-field indexes have been ensured for this
    /// connection. The schema check runs on every request, but index DDL only
    /// needs to run once per manifest revision.
    indexed_manifest_hash: Mutex<Option<String>>,
}

enum CachedStoreEntry {
    Active(Arc<Mutex<AppDataStore>>),
    /// A deleted/replaced database path. Keep this tombstone so an in-flight
    /// request that passed the service-level app lookup cannot reopen the
    /// database between invalidation and directory removal.
    Invalidating,
}

fn cached_store_map() -> &'static Mutex<HashMap<PathBuf, CachedStoreEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedStoreEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

impl AppDataStore {
    /// Open (or create) the database at the app's fixed layout path.
    pub fn open(layout: AppLayout) -> Result<Self, AppError> {
        layout.initialize()?;
        reject_non_regular_database(&layout)?;
        let database_path = layout.database_path();
        let was_present = database_path.exists();
        let connection = Connection::open(&database_path)
            .map_err(|error| map_database_error("open app database", &error))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| map_database_error("configure app database timeout", &error))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS _lingxi_schema (
                     singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                     schema_version INTEGER NOT NULL,
                     manifest_hash TEXT NOT NULL,
                     manifest_json TEXT NOT NULL,
                     updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS _lingxi_records (
                     collection TEXT NOT NULL,
                     record_id TEXT NOT NULL,
                     document TEXT NOT NULL,
                     revision INTEGER NOT NULL CHECK (revision > 0),
                     created_at_ms INTEGER NOT NULL,
                     updated_at_ms INTEGER NOT NULL,
                     PRIMARY KEY (collection, record_id)
                 );
                 CREATE INDEX IF NOT EXISTS _lingxi_records_updated
                     ON _lingxi_records (collection, updated_at_ms DESC, record_id ASC);
                 CREATE INDEX IF NOT EXISTS _lingxi_records_created
                     ON _lingxi_records (collection, created_at_ms DESC, record_id ASC);
                 CREATE INDEX IF NOT EXISTS _lingxi_records_revision
                     ON _lingxi_records (collection, revision DESC, record_id ASC);
                 INSERT OR IGNORE INTO _lingxi_schema
                     (singleton, schema_version, manifest_hash, manifest_json, updated_at_ms)
                     VALUES (1, 1, '', '', 0);",
            )
            .map_err(|error| map_database_error("initialize app database", &error))?;
        if !was_present {
            set_private_database_permissions(&database_path)?;
        }
        let store = Self {
            layout,
            connection,
            indexed_manifest_hash: Mutex::new(None),
        };
        let state = store.schema_state()?;
        if state.schema_version != DATA_SCHEMA_VERSION {
            return Err(AppError::StorageCorrupt(format!(
                "app database schemaVersion {} is unsupported (expected {DATA_SCHEMA_VERSION})",
                state.schema_version
            )));
        }
        Ok(store)
    }

    /// Open (or reuse) a process-wide cached store for `layout`.
    ///
    /// Schema init and WAL pragmas run once per database path; subsequent
    /// query/mutate calls skip that work. Intended for the mobile local-app
    /// host hot path.
    pub fn with_cached<T>(
        layout: AppLayout,
        f: impl FnOnce(&mut Self) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let cache = cached_store_map();
        let key = layout.database_path();
        let mut map = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let store = match map.get(&key) {
            Some(CachedStoreEntry::Active(store)) => Arc::clone(store),
            Some(CachedStoreEntry::Invalidating) => {
                return Err(AppError::RuntimeBusy(format!(
                    "database {} is being removed",
                    key.display()
                )));
            }
            None => {
                let opened = Arc::new(Mutex::new(Self::open(layout)?));
                map.insert(key.clone(), CachedStoreEntry::Active(Arc::clone(&opened)));
                opened
            }
        };
        let mut store = store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep the map lock until the store lock is held. This gives
        // invalidation a clear happens-before edge: it cannot remove an entry
        // while a caller is between cache lookup and acquiring its store lock.
        drop(map);
        f(&mut store)
    }

    /// Drop a cached connection before its database path is deleted or
    /// replaced. This prevents a later recreate from reusing an unlinked
    /// SQLite connection.
    pub fn invalidate_cached(layout: &AppLayout) {
        let cache = cached_store_map();
        let key = layout.database_path();
        let store = {
            let mut map = cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let store = match map.remove(&key) {
                Some(CachedStoreEntry::Active(store)) => Some(store),
                Some(CachedStoreEntry::Invalidating) | None => None,
            };
            // Keep the tombstone installed across the caller's subsequent
            // delete/replace operation. There is no valid operation for this
            // app id after AppService retires it, so reopening would only
            // resurrect a stale database through a late in-flight request.
            map.insert(key, CachedStoreEntry::Invalidating);
            store
        };
        // Wait for any in-flight operation that already held the cached
        // connection before the directory is removed. The tombstone above
        // prevents a late opener from creating a new connection meanwhile.
        if let Some(store) = store {
            let _guard = store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Read current database schema metadata.
    pub fn schema_state(&self) -> Result<DataSchemaState, AppError> {
        self.connection
            .query_row(
                "SELECT schema_version, manifest_hash, updated_at_ms
                 FROM _lingxi_schema WHERE singleton = 1",
                [],
                |row| {
                    Ok(DataSchemaState {
                        schema_version: row.get(0)?,
                        manifest_hash: row.get(1)?,
                        updated_at_ms: row.get(2)?,
                    })
                },
            )
            .map_err(|error| map_database_error("read app database schema", &error))
    }

    /// Return one record by id, or `None` when it does not exist.
    pub fn get(
        &self,
        manifest: &AppManifest,
        collection: &str,
        record_id: &str,
    ) -> Result<Option<DataRecord>, AppError> {
        self.ensure_manifest(manifest)?;
        require_collection(manifest, collection)?;
        validate_record_id(record_id)?;
        self.connection
            .query_row(
                "SELECT document, revision, created_at_ms, updated_at_ms
                 FROM _lingxi_records WHERE collection = ?1 AND record_id = ?2",
                params![collection, record_id],
                |row| decode_record_row(collection, record_id, row),
            )
            .optional()
            .map_err(|error| map_database_error("get app record", &error))
    }

    /// Execute a bounded, manifest-validated record query.
    pub fn query(&self, manifest: &AppManifest, query: &DataQuery) -> Result<DataPage, AppError> {
        self.ensure_manifest(manifest)?;
        let collection = require_collection(manifest, &query.collection)?;
        validate_query(collection, query)?;

        let mut sql = String::from(
            "SELECT record_id, document, revision, created_at_ms, updated_at_ms
             FROM _lingxi_records WHERE collection = ?",
        );
        let mut values = vec![SqlValue::Text(query.collection.clone())];
        for filter in &query.filters {
            append_filter(&mut sql, &mut values, filter)?;
        }
        append_sort(&mut sql, query);
        sql.push_str(" LIMIT ? OFFSET ?");
        values.push(SqlValue::Integer(i64::from(query.limit) + 1));
        values.push(SqlValue::Integer(i64::try_from(query.offset).map_err(
            |_| AppError::InvalidRequest("query offset exceeds i64::MAX".into()),
        )?));

        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|error| map_database_error("prepare app record query", &error))?;
        let rows = statement
            .query_map(params_from_iter(values), |row| {
                let record_id: String = row.get(0)?;
                let document_json: String = row.get(1)?;
                let document = decode_document(&document_json)?;
                Ok(DataRecord {
                    collection: query.collection.clone(),
                    record_id,
                    document,
                    revision: row.get(2)?,
                    created_at_ms: row.get(3)?,
                    updated_at_ms: row.get(4)?,
                })
            })
            .map_err(|error| map_database_error("query app records", &error))?;
        let mut records = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| map_database_error("decode app records", &error))?;
        let has_more = records.len() > query.limit as usize;
        records.truncate(query.limit as usize);
        Ok(DataPage {
            records,
            next_offset: has_more.then(|| query.offset + u64::from(query.limit)),
        })
    }

    /// Apply up to 50 operations in one `SQLite` transaction.
    pub fn mutate(
        &mut self,
        manifest: &AppManifest,
        mutations: &[DataMutation],
        now_ms: u64,
    ) -> Result<Vec<DataMutationResult>, AppError> {
        self.ensure_manifest(manifest)?;
        if mutations.is_empty() || mutations.len() > MAX_MUTATION_BATCH_SIZE {
            return Err(AppError::InvalidRequest(format!(
                "mutation batch must contain 1..={MAX_MUTATION_BATCH_SIZE} operations"
            )));
        }
        for mutation in mutations {
            validate_mutation(manifest, mutation)?;
        }
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| map_database_error("begin app data transaction", &error))?;
        let mut results = Vec::with_capacity(mutations.len());
        for mutation in mutations {
            results.push(apply_mutation(&transaction, mutation, now_ms)?);
        }
        transaction
            .commit()
            .map_err(|error| map_database_error("commit app data transaction", &error))?;
        Ok(results)
    }

    /// Compare a proposed manifest with the database's current manifest.
    pub fn preview_migration(
        &self,
        manifest: &AppManifest,
    ) -> Result<DataMigrationPreview, AppError> {
        manifest.validate()?;
        if manifest.app_id != self.layout.app_id() {
            return Err(AppError::InvalidRequest(format!(
                "manifest app id {:?} does not match database app id {:?}",
                manifest.app_id,
                self.layout.app_id()
            )));
        }
        let new_hash = manifest.data_contract_hash()?;
        let (old_hash, old_json): (String, String) = self
            .connection
            .query_row(
                "SELECT manifest_hash, manifest_json FROM _lingxi_schema WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| map_database_error("read migration baseline", &error))?;
        if old_hash.is_empty() {
            return Ok(DataMigrationPreview {
                from_manifest_hash: None,
                to_manifest_hash: new_hash,
                destructive: false,
                reasons: Vec::new(),
            });
        }
        if old_hash == new_hash {
            return Ok(DataMigrationPreview {
                from_manifest_hash: Some(old_hash),
                to_manifest_hash: new_hash,
                destructive: false,
                reasons: Vec::new(),
            });
        }
        let old_manifest: AppManifest = serde_json::from_str(&old_json).map_err(|error| {
            AppError::StorageCorrupt(format!("stored app manifest cannot be decoded: {error}"))
        })?;
        let reasons = destructive_reasons(&old_manifest, manifest);
        Ok(DataMigrationPreview {
            from_manifest_hash: Some(old_hash),
            to_manifest_hash: new_hash,
            destructive: !reasons.is_empty(),
            reasons,
        })
    }

    /// Back up the database and atomically bind it to a new manifest.
    ///
    /// Removed fields and collections remain in the generic record table as
    /// legacy data; the migration changes what future writes may address.
    /// This prevents a code/design migration from silently erasing user data.
    ///
    /// Only a destructive migration copies the database, and the copy it writes
    /// is retained together with the [`MAX_DATABASE_BACKUPS`] - 1 next newest;
    /// older copies are deleted once the new one is durable.
    pub fn migrate_manifest(
        &mut self,
        manifest: &AppManifest,
        allow_destructive: bool,
        now_ms: u64,
    ) -> Result<DataMigrationResult, AppError> {
        let preview = self.preview_migration(manifest)?;
        if preview.destructive && !allow_destructive {
            return Err(AppError::InvalidRequest(format!(
                "destructive data migration requires explicit confirmation: {}",
                preview.reasons.join("; ")
            )));
        }
        if preview.from_manifest_hash.as_deref() == Some(&preview.to_manifest_hash) {
            return Ok(DataMigrationResult {
                manifest_hash: preview.to_manifest_hash,
                backup_rel: None,
            });
        }

        // The backup exists for the DESTRUCTIVE case — the one where the new
        // contract stops addressing records the old one could reach. A
        // non-destructive change rewrites only the `_lingxi_schema` row, so
        // copying the whole database for it is pure growth: `hash()` covers
        // `revision`, which every generation job rewrites, so every design
        // iteration would leave behind another full copy. The copies that are
        // written are bounded here and nowhere else: no caller, client or
        // background task enumerates or deletes `data/backups/`.
        let backup_rel = if let Some(old_hash) = preview
            .from_manifest_hash
            .as_deref()
            .filter(|_| preview.destructive)
        {
            let backup_rel = self
                .layout
                .app_dir_rel()
                .join("data")
                .join("backups")
                .join(format!("app-{now_ms}-{}.sqlite", &old_hash[..12]));
            let backup_path = self.layout.root().join(&backup_rel);
            let backup_parent = backup_path
                .parent()
                .ok_or_else(|| AppError::Io("database backup path has no parent".into()))?;
            ensure_real_directory(backup_parent)?;
            if backup_path.exists() {
                return Err(AppError::Io(format!(
                    "database backup already exists: {}",
                    backup_path.display()
                )));
            }
            self.connection
                .backup(DatabaseName::Main, &backup_path, None)
                .map_err(|error| map_database_error("back up app database", &error))?;
            set_private_database_permissions(&backup_path)?;
            // Prune only AFTER this copy is durable on disk, and pass its name
            // so it is excluded from the candidate set: a destructive migration
            // must never be able to delete its own backup, not even when the
            // device clock moved backwards and its name sorts oldest.
            let written = backup_path
                .file_name()
                .ok_or_else(|| AppError::Io("database backup path has no file name".into()))?;
            prune_database_backups(backup_parent, written)?;
            Some(path_to_forward_slashes(&backup_rel))
        } else {
            None
        };

        let manifest_json = serde_json::to_string(manifest)
            .map_err(|error| AppError::Io(format!("serialize migrated manifest: {error}")))?;
        let transaction = self
            .connection
            .transaction()
            .map_err(|error| map_database_error("begin schema migration", &error))?;
        ensure_json_field_indexes(&transaction, manifest)?;
        transaction
            .execute(
                "UPDATE _lingxi_schema SET schema_version = ?1, manifest_hash = ?2,
                    manifest_json = ?3, updated_at_ms = ?4 WHERE singleton = 1",
                params![
                    DATA_SCHEMA_VERSION,
                    preview.to_manifest_hash,
                    manifest_json,
                    now_ms
                ],
            )
            .map_err(|error| map_database_error("write schema migration", &error))?;
        transaction
            .commit()
            .map_err(|error| map_database_error("commit schema migration", &error))?;
        *self
            .indexed_manifest_hash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(preview.to_manifest_hash.clone());
        Ok(DataMigrationResult {
            manifest_hash: preview.to_manifest_hash,
            backup_rel,
        })
    }

    fn ensure_manifest(&self, manifest: &AppManifest) -> Result<(), AppError> {
        manifest.validate()?;
        if manifest.app_id != self.layout.app_id() {
            return Err(AppError::InvalidRequest(format!(
                "manifest app id {:?} does not match database app id {:?}",
                manifest.app_id,
                self.layout.app_id()
            )));
        }
        let expected = manifest.data_contract_hash()?;
        let actual: String = self
            .connection
            .query_row(
                "SELECT manifest_hash FROM _lingxi_schema WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| map_database_error("verify app database manifest", &error))?;
        if actual == expected {
            let mut indexed_hash = self
                .indexed_manifest_hash
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if indexed_hash.as_deref() != Some(expected.as_str()) {
                let transaction = self.connection.unchecked_transaction().map_err(|error| {
                    map_database_error("begin json index reconciliation", &error)
                })?;
                ensure_json_field_indexes(&transaction, manifest)?;
                transaction.commit().map_err(|error| {
                    map_database_error("commit json index reconciliation", &error)
                })?;
                *indexed_hash = Some(expected);
            }
            Ok(())
        } else {
            Err(AppError::WorkflowStateInvalid(format!(
                "database manifest mismatch for app {}; preview and apply the data migration first",
                self.layout.app_id()
            )))
        }
    }
}

fn ensure_json_field_indexes(
    connection: &Connection,
    manifest: &AppManifest,
) -> Result<(), AppError> {
    let existing: BTreeSet<String> = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'index' AND name GLOB '_lingxi_json_*'",
        )
        .map_err(|error| map_database_error("list json field indexes", &error))?
        .query_map([], |row| row.get(0))
        .map_err(|error| map_database_error("list json field indexes", &error))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()
        .map_err(|error| map_database_error("read json field indexes", &error))?;
    let field_ids: BTreeSet<&str> = manifest
        .collections
        .iter()
        .flat_map(|collection| collection.fields.iter().map(|field| field.id.as_str()))
        .collect();
    let desired: BTreeMap<String, &str> = field_ids
        .into_iter()
        .map(|field_id| (format!("_lingxi_json_field_{field_id}"), field_id))
        .collect();

    for name in existing.iter().filter(|name| !desired.contains_key(*name)) {
        let quoted = name.replace('"', "\"\"");
        connection
            .execute(&format!("DROP INDEX IF EXISTS \"{quoted}\""), [])
            .map_err(|error| map_database_error("drop json field index", &error))?;
    }

    // The expression is independent of collection, while collection is the
    // leading indexed column. One index per field id therefore serves every
    // collection and avoids duplicate full-table indexes when schemas reuse a
    // common field name. Existing canonical indexes remain untouched, so a
    // process reopen never rebuilds a valid large index.
    for (name, field_id) in desired {
        if existing.contains(&name) {
            continue;
        }
        let sql = format!(
            "CREATE INDEX \"{name}\" ON _lingxi_records (collection, json_extract(document, '$.{field_id}'))"
        );
        connection
            .execute(&sql, [])
            .map_err(|error| map_database_error("create json field index", &error))?;
    }
    Ok(())
}

fn require_collection<'a>(
    manifest: &'a AppManifest,
    collection: &str,
) -> Result<&'a DataCollectionSchema, AppError> {
    validate_identifier("collection", collection)?;
    manifest.collection(collection).ok_or_else(|| {
        AppError::InvalidRequest(format!(
            "collection {collection:?} is not declared by the app manifest"
        ))
    })
}

fn validate_query(collection: &DataCollectionSchema, query: &DataQuery) -> Result<(), AppError> {
    if query.limit == 0 || query.limit > MAX_QUERY_PAGE_SIZE {
        return Err(AppError::InvalidRequest(format!(
            "query limit must be 1..={MAX_QUERY_PAGE_SIZE}"
        )));
    }
    if query.offset > i64::MAX as u64 {
        return Err(AppError::InvalidRequest(
            "query offset exceeds i64::MAX".into(),
        ));
    }
    if query.filters.len() > MAX_QUERY_FILTERS {
        return Err(AppError::InvalidRequest(format!(
            "query has more than {MAX_QUERY_FILTERS} filters"
        )));
    }
    for filter in &query.filters {
        let field = collection_field(collection, &filter.field_id)?;
        match filter.operator {
            DataFilterOperator::Contains
                if !matches!(
                    field.kind,
                    DataFieldKind::Text
                        | DataFieldKind::LongText
                        | DataFieldKind::DateTime
                        | DataFieldKind::ImageRef
                ) =>
            {
                return Err(AppError::InvalidRequest(format!(
                    "contains is not valid for field {:?}",
                    filter.field_id
                )));
            }
            DataFilterOperator::In => {
                let values = filter.value.as_array().ok_or_else(|| {
                    AppError::InvalidRequest("in filter value must be an array".into())
                })?;
                if values.is_empty() || values.len() > MAX_FILTER_IN_VALUES {
                    return Err(AppError::InvalidRequest(format!(
                        "in filter must contain 1..={MAX_FILTER_IN_VALUES} scalar values"
                    )));
                }
                for value in values {
                    validate_filter_value(field, value)?;
                }
            }
            _ => validate_filter_value(field, &filter.value)?,
        }
    }
    if let Some(DataSortKey::Field(field_id)) = &query.sort_key {
        collection_field(collection, field_id)?;
    }
    Ok(())
}

fn append_filter(
    sql: &mut String,
    values: &mut Vec<SqlValue>,
    filter: &DataFilter,
) -> Result<(), AppError> {
    match filter.operator {
        DataFilterOperator::Equal
        | DataFilterOperator::NotEqual
        | DataFilterOperator::LessThan
        | DataFilterOperator::LessThanOrEqual
        | DataFilterOperator::GreaterThan
        | DataFilterOperator::GreaterThanOrEqual => {
            let operator = match filter.operator {
                DataFilterOperator::Equal => "=",
                DataFilterOperator::NotEqual => "!=",
                DataFilterOperator::LessThan => "<",
                DataFilterOperator::LessThanOrEqual => "<=",
                DataFilterOperator::GreaterThan => ">",
                DataFilterOperator::GreaterThanOrEqual => ">=",
                DataFilterOperator::Contains | DataFilterOperator::In => unreachable!(),
            };
            sql.push_str(&format!(
                " AND json_extract(document, '$.{}') {operator} ?",
                filter.field_id
            ));
            values.push(json_scalar_to_sql(&filter.value)?);
        }
        DataFilterOperator::Contains => {
            let needle = filter.value.as_str().ok_or_else(|| {
                AppError::InvalidRequest("contains filter requires a string".into())
            })?;
            sql.push_str(&format!(
                " AND CAST(json_extract(document, '$.{}') AS TEXT) LIKE ? ESCAPE '\\'",
                filter.field_id
            ));
            values.push(SqlValue::Text(format!("%{}%", escape_like(needle))));
        }
        DataFilterOperator::In => {
            let members = filter
                .value
                .as_array()
                .ok_or_else(|| AppError::InvalidRequest("in filter requires an array".into()))?;
            sql.push_str(&format!(
                " AND json_extract(document, '$.{}') IN (",
                filter.field_id
            ));
            for (index, member) in members.iter().enumerate() {
                if index > 0 {
                    sql.push_str(", ");
                }
                sql.push('?');
                values.push(json_scalar_to_sql(member)?);
            }
            sql.push(')');
        }
    }
    Ok(())
}

fn append_sort(sql: &mut String, query: &DataQuery) {
    sql.push_str(" ORDER BY ");
    match query.sort_key.as_ref().unwrap_or(&DataSortKey::UpdatedAt) {
        DataSortKey::RecordId => sql.push_str("record_id"),
        DataSortKey::CreatedAt => sql.push_str("created_at_ms"),
        DataSortKey::UpdatedAt => sql.push_str("updated_at_ms"),
        DataSortKey::Revision => sql.push_str("revision"),
        DataSortKey::Field(field_id) => {
            sql.push_str(&format!("json_extract(document, '$.{field_id}')"));
        }
    }
    sql.push_str(match query.sort_direction {
        DataSortDirection::Ascending => " ASC",
        DataSortDirection::Descending => " DESC",
    });
    sql.push_str(", record_id ASC");
}

fn validate_mutation(manifest: &AppManifest, mutation: &DataMutation) -> Result<(), AppError> {
    match mutation {
        DataMutation::Upsert {
            collection,
            record_id,
            document,
            expected_revision,
        } => {
            let schema = require_collection(manifest, collection)?;
            validate_record_id(record_id)?;
            validate_expected_revision(*expected_revision)?;
            validate_document(schema, document)
        }
        DataMutation::Delete {
            collection,
            record_id,
            expected_revision,
        } => {
            require_collection(manifest, collection)?;
            validate_record_id(record_id)?;
            validate_expected_revision(*expected_revision)
        }
    }
}

fn apply_mutation(
    transaction: &Transaction<'_>,
    mutation: &DataMutation,
    now_ms: u64,
) -> Result<DataMutationResult, AppError> {
    match mutation {
        DataMutation::Upsert {
            collection,
            record_id,
            document,
            expected_revision,
        } => {
            let existing: Option<(u64, u64)> = transaction
                .query_row(
                    "SELECT revision, created_at_ms FROM _lingxi_records
                     WHERE collection = ?1 AND record_id = ?2",
                    params![collection, record_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|error| map_database_error("read mutation baseline", &error))?;
            let (revision, created_at_ms) = if let Some((actual, created_at_ms)) = existing {
                ensure_expected_revision(*expected_revision, actual)?;
                (
                    actual.checked_add(1).ok_or_else(|| {
                        AppError::StorageCorrupt("record revision overflow".into())
                    })?,
                    created_at_ms,
                )
            } else {
                if let Some(expected) = expected_revision {
                    return Err(AppError::RevisionConflict {
                        expected: *expected,
                        actual: 0,
                    });
                }
                (1, now_ms)
            };
            let document = serde_json::to_string(document)
                .map_err(|error| AppError::InvalidRequest(format!("serialize record: {error}")))?;
            transaction
                .execute(
                    "INSERT INTO _lingxi_records
                        (collection, record_id, document, revision, created_at_ms, updated_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(collection, record_id) DO UPDATE SET
                        document = excluded.document,
                        revision = excluded.revision,
                        updated_at_ms = excluded.updated_at_ms",
                    params![
                        collection,
                        record_id,
                        document,
                        revision,
                        created_at_ms,
                        now_ms
                    ],
                )
                .map_err(|error| map_database_error("upsert app record", &error))?;
            Ok(DataMutationResult {
                collection: collection.clone(),
                record_id: record_id.clone(),
                revision: Some(revision),
                deleted: false,
            })
        }
        DataMutation::Delete {
            collection,
            record_id,
            expected_revision,
        } => {
            let actual: Option<u64> = transaction
                .query_row(
                    "SELECT revision FROM _lingxi_records
                     WHERE collection = ?1 AND record_id = ?2",
                    params![collection, record_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| map_database_error("read delete baseline", &error))?;
            let actual = actual
                .ok_or_else(|| AppError::NotFound(format!("record {collection}/{record_id}")))?;
            ensure_expected_revision(*expected_revision, actual)?;
            transaction
                .execute(
                    "DELETE FROM _lingxi_records WHERE collection = ?1 AND record_id = ?2",
                    params![collection, record_id],
                )
                .map_err(|error| map_database_error("delete app record", &error))?;
            Ok(DataMutationResult {
                collection: collection.clone(),
                record_id: record_id.clone(),
                revision: None,
                deleted: true,
            })
        }
    }
}

fn ensure_expected_revision(expected: Option<u64>, actual: u64) -> Result<(), AppError> {
    if expected.is_none() || expected == Some(actual) {
        Ok(())
    } else {
        Err(AppError::RevisionConflict {
            expected: expected.unwrap_or_default(),
            actual,
        })
    }
}

fn validate_expected_revision(revision: Option<u64>) -> Result<(), AppError> {
    if revision == Some(0) {
        Err(AppError::InvalidRequest(
            "expected revision must be greater than zero".into(),
        ))
    } else {
        Ok(())
    }
}

fn validate_record_id(record_id: &str) -> Result<(), AppError> {
    if record_id.is_empty()
        || record_id.len() > MAX_RECORD_ID_BYTES
        || record_id.chars().any(char::is_control)
        || record_id.trim() != record_id
    {
        Err(AppError::InvalidRequest(format!(
            "invalid record id {record_id:?}: expected 1..={MAX_RECORD_ID_BYTES} non-control bytes"
        )))
    } else {
        Ok(())
    }
}

fn validate_document(
    collection: &DataCollectionSchema,
    document: &Map<String, Value>,
) -> Result<(), AppError> {
    let encoded = serde_json::to_vec(document)
        .map_err(|error| AppError::InvalidRequest(format!("serialize record: {error}")))?;
    if encoded.len() > MAX_RECORD_DOCUMENT_BYTES {
        return Err(AppError::InvalidRequest(format!(
            "record document is {} bytes (limit {MAX_RECORD_DOCUMENT_BYTES})",
            encoded.len()
        )));
    }
    for key in document.keys() {
        if !collection.fields.iter().any(|field| field.id == *key) {
            return Err(AppError::InvalidRequest(format!(
                "field {key:?} is not declared in collection {:?}",
                collection.id
            )));
        }
    }
    for field in &collection.fields {
        match document.get(&field.id) {
            Some(value) => validate_field_value(field, value)?,
            None if field.required => {
                return Err(AppError::InvalidRequest(format!(
                    "required field {:?}.{:?} is missing",
                    collection.id, field.id
                )));
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_field_value(field: &DataFieldSchema, value: &Value) -> Result<(), AppError> {
    let valid = match field.kind {
        DataFieldKind::Text | DataFieldKind::LongText => {
            value.as_str().is_some_and(|text| text.len() <= 64 * 1024)
        }
        DataFieldKind::Integer => value.as_i64().is_some(),
        DataFieldKind::Decimal => value.as_f64().is_some_and(f64::is_finite),
        DataFieldKind::Boolean => value.is_boolean(),
        DataFieldKind::DateTime => value.as_str().is_some_and(valid_date_time_shape),
        DataFieldKind::Enum => value
            .as_str()
            .is_some_and(|candidate| field.enum_options.iter().any(|option| option == candidate)),
        DataFieldKind::ImageRef => value
            .as_str()
            .is_some_and(|reference| !reference.is_empty() && reference.len() <= 2048),
    };
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidRequest(format!(
            "value for field {:?} does not match {:?}",
            field.id, field.kind
        )))
    }
}

fn validate_filter_value(field: &DataFieldSchema, value: &Value) -> Result<(), AppError> {
    if value.is_null() || value.is_array() || value.is_object() {
        return Err(AppError::InvalidRequest(
            "filter values must be non-null scalars".into(),
        ));
    }
    validate_field_value(field, value)
}

fn collection_field<'a>(
    collection: &'a DataCollectionSchema,
    field_id: &str,
) -> Result<&'a DataFieldSchema, AppError> {
    validate_identifier("field", field_id)?;
    collection
        .fields
        .iter()
        .find(|field| field.id == field_id)
        .ok_or_else(|| {
            AppError::InvalidRequest(format!(
                "field {field_id:?} is not declared in collection {:?}",
                collection.id
            ))
        })
}

fn valid_date_time_shape(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    date.len() == 10
        && date.as_bytes().get(4) == Some(&b'-')
        && date.as_bytes().get(7) == Some(&b'-')
        && time.contains(':')
        && (time.ends_with('Z') || time.rfind(['+', '-']).is_some_and(|index| index > 4))
}

fn json_scalar_to_sql(value: &Value) -> Result<SqlValue, AppError> {
    match value {
        Value::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(SqlValue::Integer(value))
            } else if let Some(value) = value.as_f64().filter(|value| value.is_finite()) {
                Ok(SqlValue::Real(value))
            } else {
                Err(AppError::InvalidRequest(
                    "filter number is outside SQLite range".into(),
                ))
            }
        }
        Value::String(value) => Ok(SqlValue::Text(value.clone())),
        Value::Null | Value::Array(_) | Value::Object(_) => Err(AppError::InvalidRequest(
            "filter values must be non-null scalars".into(),
        )),
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn decode_record_row(
    collection: &str,
    record_id: &str,
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<DataRecord> {
    let document_json: String = row.get(0)?;
    let document = decode_document(&document_json)?;
    Ok(DataRecord {
        collection: collection.to_string(),
        record_id: record_id.to_string(),
        document,
        revision: row.get(1)?,
        created_at_ms: row.get(2)?,
        updated_at_ms: row.get(3)?,
    })
}

fn decode_document(document: &str) -> rusqlite::Result<Map<String, Value>> {
    serde_json::from_str::<Map<String, Value>>(document).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            document.len(),
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn destructive_reasons(old: &AppManifest, new: &AppManifest) -> Vec<String> {
    let old_collections: BTreeMap<_, _> = old
        .collections
        .iter()
        .map(|collection| (collection.id.as_str(), collection))
        .collect();
    let new_collections: BTreeMap<_, _> = new
        .collections
        .iter()
        .map(|collection| (collection.id.as_str(), collection))
        .collect();
    let mut reasons = Vec::new();
    for (collection_id, old_collection) in old_collections {
        let Some(new_collection) = new_collections.get(collection_id) else {
            reasons.push(format!("collection {collection_id:?} was removed"));
            continue;
        };
        let new_fields: BTreeMap<_, _> = new_collection
            .fields
            .iter()
            .map(|field| (field.id.as_str(), field))
            .collect();
        let old_fields: BTreeSet<_> = old_collection
            .fields
            .iter()
            .map(|field| field.id.as_str())
            .collect();
        for old_field in &old_collection.fields {
            match new_fields.get(old_field.id.as_str()) {
                None => reasons.push(format!(
                    "field {collection_id:?}.{:?} was removed",
                    old_field.id
                )),
                Some(new_field) if new_field.kind != old_field.kind => reasons.push(format!(
                    "field {collection_id:?}.{:?} changed type",
                    old_field.id
                )),
                Some(new_field)
                    if old_field.kind == DataFieldKind::Enum
                        && old_field
                            .enum_options
                            .iter()
                            .any(|option| !new_field.enum_options.contains(option)) =>
                {
                    reasons.push(format!(
                        "enum field {collection_id:?}.{:?} removed options",
                        old_field.id
                    ));
                }
                Some(_) => {}
            }
        }
        for new_field in &new_collection.fields {
            if new_field.required && !old_fields.contains(new_field.id.as_str()) {
                reasons.push(format!(
                    "new required field {collection_id:?}.{:?} has no value in existing records",
                    new_field.id
                ));
            }
        }
    }
    reasons
}

fn reject_non_regular_database(layout: &AppLayout) -> Result<(), AppError> {
    match std::fs::symlink_metadata(layout.database_path()) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(AppError::StorageCorrupt(format!(
            "{} is not a regular database file",
            layout.database_path().display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Io(format!(
            "inspect app database {}: {error}",
            layout.database_path().display()
        ))),
    }
}

fn ensure_real_directory(path: &std::path::Path) -> Result<(), AppError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(AppError::StorageCorrupt(format!(
            "{} is not a real directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| AppError::Io(format!("{} has no parent", path.display())))?;
            let metadata = std::fs::symlink_metadata(parent)
                .map_err(|error| AppError::Io(format!("inspect {}: {error}", parent.display())))?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AppError::StorageCorrupt(format!(
                    "{} is not a real directory",
                    parent.display()
                )));
            }
            std::fs::create_dir(path)
                .map_err(|error| AppError::Io(format!("create {}: {error}", path.display())))?;
            set_private_directory_permissions(path)
        }
        Err(error) => Err(AppError::Io(format!("inspect {}: {error}", path.display()))),
    }
}

/// Delete the oldest pre-migration backups until at most
/// [`MAX_DATABASE_BACKUPS`] remain in `directory`.
///
/// `keep` is the backup the caller just wrote: it is never a candidate, so it
/// survives regardless of how its name orders, and it occupies one of the
/// retained slots. Only regular files named like a backup this module writes
/// are considered — anything else in the directory is left untouched.
fn prune_database_backups(
    directory: &std::path::Path,
    keep: &std::ffi::OsStr,
) -> Result<(), AppError> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| AppError::Io(format!("list {}: {error}", directory.display())))?;
    let mut candidates: Vec<(u64, std::ffi::OsString)> = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| AppError::Io(format!("list {}: {error}", directory.display())))?;
        let name = entry.file_name();
        if name == keep {
            continue;
        }
        let Some(stamp) = name.to_str().and_then(backup_timestamp_ms) else {
            continue;
        };
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| AppError::Io(format!("inspect {}: {error}", path.display())))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        candidates.push((stamp, name));
    }
    // The backup just written already holds one slot.
    let retain = MAX_DATABASE_BACKUPS.saturating_sub(1);
    if candidates.len() <= retain {
        return Ok(());
    }
    candidates.sort_unstable();
    let doomed = candidates.len() - retain;
    for (_, name) in candidates.into_iter().take(doomed) {
        let path = directory.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Another migration of the same app pruned it first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AppError::Io(format!("remove {}: {error}", path.display())));
            }
        }
    }
    Ok(())
}

/// Epoch-millisecond stamp encoded in a `app-<ms>-<hash>.sqlite` backup name.
fn backup_timestamp_ms(name: &str) -> Option<u64> {
    let (stamp, _) = name
        .strip_prefix("app-")?
        .strip_suffix(".sqlite")?
        .split_once('-')?;
    stamp.parse().ok()
}

fn path_to_forward_slashes(path: &std::path::Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn map_database_error(operation: &str, error: &rusqlite::Error) -> AppError {
    match error {
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ) =>
        {
            AppError::StorageCorrupt(format!("{operation}: {error}"))
        }
        rusqlite::Error::FromSqlConversionFailure(..) | rusqlite::Error::InvalidColumnType(..) => {
            AppError::StorageCorrupt(format!("{operation}: {error}"))
        }
        _ => AppError::Io(format!("{operation}: {error}")),
    }
}

#[cfg(unix)]
fn set_private_database_permissions(path: &std::path::Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::Io(format!("secure {}: {error}", path.display())))
}

#[cfg(not(unix))]
fn set_private_database_permissions(_path: &std::path::Path) -> Result<(), AppError> {
    Ok(())
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &std::path::Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| AppError::Io(format!("secure {}: {error}", path.display())))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &std::path::Path) -> Result<(), AppError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{DataCollectionSchema, DataFieldSchema};
    use crate::types::APPS_SCHEMA_VERSION;
    use serde_json::json;

    fn manifest() -> AppManifest {
        AppManifest {
            schema_version: APPS_SCHEMA_VERSION,
            runtime_api_version: crate::runtime_v2::RUNTIME_API_MAJOR,
            app_id: "abcd1234".into(),
            revision: 1,
            name: "Tracker".into(),
            collections: vec![DataCollectionSchema {
                id: "items".into(),
                name: "Items".into(),
                fields: vec![
                    DataFieldSchema {
                        id: "title".into(),
                        label: "Title".into(),
                        kind: DataFieldKind::Text,
                        required: true,
                        enum_options: vec![],
                    },
                    DataFieldSchema {
                        id: "status".into(),
                        label: "Status".into(),
                        kind: DataFieldKind::Enum,
                        required: true,
                        enum_options: vec!["todo".into(), "done".into()],
                    },
                    DataFieldSchema {
                        id: "score".into(),
                        label: "Score".into(),
                        kind: DataFieldKind::Integer,
                        required: false,
                        enum_options: vec![],
                    },
                ],
            }],
            allowed_domains: vec![],
            capabilities: vec![],
            device_context: None,
            surface: None,
            runtime_profile: None,
            dependency_snapshot: None,
            template_origin: None,
            active_mcp_catalog: None,
        }
    }

    fn document(title: &str, status: &str, score: i64) -> Map<String, Value> {
        let Value::Object(document) = json!({
            "title": title,
            "status": status,
            "score": score
        }) else {
            unreachable!();
        };
        document
    }

    fn open_initialized() -> (tempfile::TempDir, AppDataStore, AppManifest) {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        let mut store = AppDataStore::open(layout).unwrap();
        let manifest = manifest();
        store.migrate_manifest(&manifest, false, 1).unwrap();
        (root, store, manifest)
    }

    #[test]
    fn reopening_preserves_existing_json_indexes() {
        let (root, store, manifest) = open_initialized();
        drop(store);
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        let reopened = AppDataStore::open(layout).unwrap();
        let schema_before: i64 = reopened
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();

        reopened.ensure_manifest(&manifest).unwrap();

        let schema_after: i64 = reopened
            .connection
            .query_row("PRAGMA schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(schema_after, schema_before);
    }

    #[test]
    fn persists_records_with_revisions_and_pages() {
        let (root, mut store, manifest) = open_initialized();
        let results = store
            .mutate(
                &manifest,
                &[
                    DataMutation::Upsert {
                        collection: "items".into(),
                        record_id: "a".into(),
                        document: document("Alpha", "todo", 10),
                        expected_revision: None,
                    },
                    DataMutation::Upsert {
                        collection: "items".into(),
                        record_id: "b".into(),
                        document: document("Beta", "done", 20),
                        expected_revision: None,
                    },
                ],
                10,
            )
            .unwrap();
        assert_eq!(results[0].revision, Some(1));
        let page = store
            .query(
                &manifest,
                &DataQuery {
                    collection: "items".into(),
                    filters: vec![],
                    sort_key: Some(DataSortKey::RecordId),
                    sort_direction: DataSortDirection::Ascending,
                    limit: 1,
                    offset: 0,
                },
            )
            .unwrap();
        assert_eq!(page.records[0].record_id, "a");
        assert_eq!(page.next_offset, Some(1));
        drop(store);

        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        let reopened = AppDataStore::open(layout).unwrap();
        assert_eq!(
            reopened
                .get(&manifest, "items", "b")
                .unwrap()
                .unwrap()
                .revision,
            1
        );
    }

    #[test]
    fn filters_and_sorts_without_raw_sql() {
        let (_root, mut store, manifest) = open_initialized();
        for (id, title, status, score) in [
            ("a", "Alpha", "todo", 5),
            ("b", "Beta", "done", 20),
            ("c", "Alphabet", "done", 15),
        ] {
            store
                .mutate(
                    &manifest,
                    &[DataMutation::Upsert {
                        collection: "items".into(),
                        record_id: id.into(),
                        document: document(title, status, score),
                        expected_revision: None,
                    }],
                    10,
                )
                .unwrap();
        }
        let page = store
            .query(
                &manifest,
                &DataQuery {
                    collection: "items".into(),
                    filters: vec![
                        DataFilter {
                            field_id: "status".into(),
                            operator: DataFilterOperator::Equal,
                            value: json!("done"),
                        },
                        DataFilter {
                            field_id: "score".into(),
                            operator: DataFilterOperator::GreaterThan,
                            value: json!(10),
                        },
                    ],
                    sort_key: Some(DataSortKey::Field("score".into())),
                    sort_direction: DataSortDirection::Descending,
                    limit: 100,
                    offset: 0,
                },
            )
            .unwrap();
        assert_eq!(
            page.records
                .iter()
                .map(|record| record.record_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    #[test]
    fn mutation_batch_rolls_back_on_revision_conflict() {
        let (_root, mut store, manifest) = open_initialized();
        store
            .mutate(
                &manifest,
                &[DataMutation::Upsert {
                    collection: "items".into(),
                    record_id: "a".into(),
                    document: document("Original", "todo", 1),
                    expected_revision: None,
                }],
                1,
            )
            .unwrap();
        let error = store
            .mutate(
                &manifest,
                &[
                    DataMutation::Upsert {
                        collection: "items".into(),
                        record_id: "b".into(),
                        document: document("Transient", "todo", 2),
                        expected_revision: None,
                    },
                    DataMutation::Upsert {
                        collection: "items".into(),
                        record_id: "a".into(),
                        document: document("Wrong", "done", 3),
                        expected_revision: Some(99),
                    },
                ],
                2,
            )
            .unwrap_err();
        assert!(matches!(error, AppError::RevisionConflict { .. }));
        assert!(store.get(&manifest, "items", "b").unwrap().is_none());
        assert_eq!(
            store
                .get(&manifest, "items", "a")
                .unwrap()
                .unwrap()
                .document["title"],
            json!("Original")
        );
    }

    #[test]
    fn enforces_query_and_batch_limits_and_manifest_types() {
        let (_root, mut store, manifest) = open_initialized();
        let query = DataQuery {
            collection: "items".into(),
            filters: vec![],
            sort_key: None,
            sort_direction: DataSortDirection::Ascending,
            limit: 101,
            offset: 0,
        };
        assert!(matches!(
            store.query(&manifest, &query),
            Err(AppError::InvalidRequest(_))
        ));
        let too_many = (0..51)
            .map(|index| DataMutation::Delete {
                collection: "items".into(),
                record_id: index.to_string(),
                expected_revision: None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            store.mutate(&manifest, &too_many, 1),
            Err(AppError::InvalidRequest(_))
        ));
        assert!(matches!(
            store.mutate(
                &manifest,
                &[DataMutation::Upsert {
                    collection: "items".into(),
                    record_id: "bad".into(),
                    document: document("Bad", "unknown", 1),
                    expected_revision: None,
                }],
                1
            ),
            Err(AppError::InvalidRequest(_))
        ));
    }

    /// `AppManifest::hash()` covers `revision`, which every generation job
    /// rewrites, so a design iteration that leaves the collection schema
    /// byte-identical still counts as a migration. Copying the whole database
    /// for one would be pure growth for a migration that cannot lose
    /// anything, so the directory must not even be created.
    #[test]
    fn non_destructive_migration_does_not_copy_the_database() {
        let (root, mut store, old_manifest) = open_initialized();
        store
            .mutate(
                &old_manifest,
                &[DataMutation::Upsert {
                    collection: "items".into(),
                    record_id: "a".into(),
                    document: document("Keep", "todo", 1),
                    expected_revision: None,
                }],
                2,
            )
            .unwrap();
        let mut new_manifest = old_manifest.clone();
        new_manifest.revision = old_manifest.revision + 1;
        let preview = store.preview_migration(&new_manifest).unwrap();
        assert!(!preview.destructive);
        assert_ne!(preview.from_manifest_hash.as_deref(), None);

        let migrated = store.migrate_manifest(&new_manifest, false, 3).unwrap();
        assert_eq!(migrated.backup_rel, None);
        assert!(!root.path().join("apps/abcd1234/data/backups").exists());
        // The migration still bound the new contract.
        store.ensure_manifest(&new_manifest).unwrap();
        assert_eq!(
            store
                .get(&new_manifest, "items", "a")
                .unwrap()
                .unwrap()
                .document["title"],
            json!("Keep")
        );
    }

    #[test]
    fn destructive_migration_requires_confirmation_and_creates_backup() {
        let (root, mut store, old_manifest) = open_initialized();
        store
            .mutate(
                &old_manifest,
                &[DataMutation::Upsert {
                    collection: "items".into(),
                    record_id: "a".into(),
                    document: document("Keep", "todo", 1),
                    expected_revision: None,
                }],
                2,
            )
            .unwrap();
        let mut new_manifest = old_manifest.clone();
        new_manifest.revision = 2;
        new_manifest.collections[0]
            .fields
            .retain(|field| field.id != "score");
        let preview = store.preview_migration(&new_manifest).unwrap();
        assert!(preview.destructive);
        assert!(matches!(
            store.migrate_manifest(&new_manifest, false, 3),
            Err(AppError::InvalidRequest(_))
        ));
        let migrated = store.migrate_manifest(&new_manifest, true, 3).unwrap();
        let backup = migrated.backup_rel.unwrap();
        assert!(root.path().join(backup).is_file());
        // Generic storage preserves legacy fields rather than erasing data.
        assert_eq!(
            store
                .get(&new_manifest, "items", "a")
                .unwrap()
                .unwrap()
                .document["score"],
            json!(1)
        );
    }

    fn backup_dir_names(directory: &std::path::Path) -> Vec<String> {
        let mut names = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    /// Flip `score` between two types so each round is a fresh destructive
    /// migration, and return the backup it wrote.
    fn destructive_round(
        store: &mut AppDataStore,
        manifest: &AppManifest,
        round: u64,
    ) -> (AppManifest, String) {
        let mut next = manifest.clone();
        next.revision = manifest.revision + 1;
        next.collections[0].fields[2].kind = if round % 2 == 0 {
            DataFieldKind::Text
        } else {
            DataFieldKind::Integer
        };
        let preview = store.preview_migration(&next).unwrap();
        assert!(preview.destructive, "round {round} was not destructive");
        let migrated = store.migrate_manifest(&next, true, 1_000 + round).unwrap();
        let backup = migrated
            .backup_rel
            .expect("a destructive migration backs the database up");
        (next, backup)
    }

    /// A destructive migration copies the WHOLE database, and nothing
    /// downstream enumerates, caps or deletes `data/backups/` — the engine's
    /// `apply_manifest_migration` even discards `backup_rel`. So the bound has
    /// to live at the write site.
    #[test]
    fn destructive_migrations_retain_only_the_newest_backups() {
        let (root, mut store, first) = open_initialized();
        store
            .mutate(
                &first,
                &[DataMutation::Upsert {
                    collection: "items".into(),
                    record_id: "a".into(),
                    document: document("Keep", "todo", 1),
                    expected_revision: None,
                }],
                2,
            )
            .unwrap();

        let mut current = first;
        let mut written = Vec::new();
        for round in 0..5u64 {
            let (next, backup) = destructive_round(&mut store, &current, round);
            assert!(
                root.path().join(&backup).is_file(),
                "round {round} reported a backup that is not on disk"
            );
            written.push(backup);
            current = next;
        }

        let backups = root.path().join("apps/abcd1234/data/backups");
        assert_eq!(backup_dir_names(&backups).len(), MAX_DATABASE_BACKUPS);
        let kept_from = written.len() - MAX_DATABASE_BACKUPS;
        for stale in &written[..kept_from] {
            assert!(
                !root.path().join(stale).exists(),
                "{stale} should have been pruned"
            );
        }
        for kept in &written[kept_from..] {
            assert!(
                root.path().join(kept).is_file(),
                "{kept} should have been retained"
            );
        }
        // Pruning is not allowed to disturb the migration itself.
        store.ensure_manifest(&current).unwrap();
        assert_eq!(
            store.get(&current, "items", "a").unwrap().unwrap().document["title"],
            json!("Keep")
        );
    }

    /// The prune runs after the new copy is durable and excludes it by name,
    /// so a destructive migration can never delete its own backup — not even
    /// when the device clock moved backwards and the new name is the oldest
    /// one in the directory. Files that are not backups are left alone.
    #[test]
    fn a_backup_survives_the_prune_its_own_migration_triggers() {
        let (root, mut store, old_manifest) = open_initialized();
        let backups = root.path().join("apps/abcd1234/data/backups");
        std::fs::create_dir_all(&backups).unwrap();
        for stamp in 0..5u64 {
            std::fs::write(
                backups.join(format!("app-90000000000{stamp}-aaaaaaaaaaaa.sqlite")),
                b"older in fact, newer by name",
            )
            .unwrap();
        }
        std::fs::write(backups.join("README.txt"), b"not a backup").unwrap();

        let mut new_manifest = old_manifest.clone();
        new_manifest.revision = 2;
        new_manifest.collections[0].fields[2].kind = DataFieldKind::Text;
        // `7` sorts oldest among the stamps in the directory.
        let migrated = store.migrate_manifest(&new_manifest, true, 7).unwrap();
        let backup = migrated
            .backup_rel
            .expect("a destructive migration backs the database up");
        assert!(
            root.path().join(&backup).is_file(),
            "the migration deleted its own backup {backup}"
        );

        let new_name = backup.rsplit('/').next().unwrap().to_string();
        let mut expected = vec!["README.txt".to_string(), new_name];
        expected.extend(
            (0..5u64)
                .rev()
                .take(MAX_DATABASE_BACKUPS - 1)
                .map(|stamp| format!("app-90000000000{stamp}-aaaaaaaaaaaa.sqlite")),
        );
        expected.sort();
        assert_eq!(backup_dir_names(&backups), expected);
    }

    #[test]
    fn malformed_database_is_reported_as_storage_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let layout = AppLayout::new(root.path(), "abcd1234").unwrap();
        layout.initialize().unwrap();
        std::fs::write(layout.database_path(), b"not a sqlite database").unwrap();
        assert!(matches!(
            AppDataStore::open(layout),
            Err(AppError::StorageCorrupt(_))
        ));
    }
}
