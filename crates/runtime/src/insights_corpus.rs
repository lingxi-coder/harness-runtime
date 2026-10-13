//! Local `/insights` corpus, transcript and cache implementation draft.
//!
//! This file is an acceptance-only Runtime implementation artifact. It uses
//! `Utf16JsonProjection` as the source carrier for transcript paths, cached
//! values, formatted transcript strings, aggregate JSON and errors. It is
//! deliberately not wired into the production workspace by this task.
//!
//! Source anchors are pinned to Claude Code 2.1.295, binary SHA-256
//! `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`:
//! `Ht` `[207244862,207246570)`, `Te` `[207166374,207167308)`, `mt`
//! `[207177076,207177380)`, `ze` `[207167345,207168255)`, `je`
//! `[207171466,207171585)`, `Wt` `[207246570,207249716)`, `Ct`
//! `[207182285,207183111)`, `St` `[207183111,207187039)`, and `Ae`
//! `[207252504,207252877)`. The imported helpers `Rs`, `W9e`, and `ne` are
//! separately bound in `../insights-source/implementation-source-map.md`.
//!
//! The orchestration boundary is intentional: the caller performs facet
//! requests and passes only successfully parsed `Ae`-valid values to
//! [`PreparedInsightsCorpus::finish`]. Rust owns local discovery, cache I/O,
//! refresh selection, and ordering. Exact Native `eKt`/`Ce`/`mt`/`Te`,
//! `warmup_minimal`, and `St`/`Ct` transforms cross the injected Bun-worker
//! interface so JS behavior is not approximated in Rust. The transcript worker
//! receives the discovered file path and calls `eKt(path, undefined, undefined)`;
//! this preserves `gle`'s path-based large-file, torn-tail, and artifact paths.

use lingxi_core::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The source-preserving error carrier used by the `/insights` provider.
pub type InsightsCorpusError = Utf16JsonProjection;

/// A JSONL file's Native path identity and stat facts needed by `eKt`/`gle`.
#[derive(Debug, Clone)]
pub struct RawTranscriptSession {
    /// UUID taken from the filename by Native `Ht`/`zEt`.
    pub session_id: String,
    /// Root-string projection of the discovered path. Bun passes this directly
    /// to Native `eKt(path, undefined, undefined)`; Rust does not pre-read or
    /// decode the transcript bytes.
    pub transcript_path: Utf16JsonProjection,
    /// File mtime in JavaScript-compatible milliseconds.
    pub transcript_mtime_ms: f64,
    /// Native `stat.size`, used by `gle` to select its large-file path.
    pub file_size_bytes: u64,
    /// Host-resolved `MZo` q4 open results used by Native `cL()` when a
    /// transcript's first prompt contains a slash command. The action worker
    /// requires all five facts and memoizes the first resulting command-name
    /// set, matching the source's first-call cache.
    pub q4_open_results: Utf16JsonProjection,
}

/// Native session view created from one leaf in an `eKt` transcript result.
#[derive(Debug, Clone)]
pub struct NativeSessionAnalysis {
    /// The exact `Ce` metadata object before Rust adds `transcript_mtime`.
    pub metadata: Utf16JsonProjection,
    /// The exact normalized `eKt` message array consumed by Native `Te`.
    pub messages: Utf16JsonProjection,
}

/// A boxed future so the extracted Bun worker can implement this interface
/// without adding an async-trait dependency to the acceptance draft.
pub type InsightsSourceFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, InsightsCorpusError>> + Send + 'a>>;

/// Calls the exact pinned JavaScript helpers for source-sensitive transforms.
/// Rust owns filesystem traversal, JSONL/cache I/O, refresh selection, and
/// ordering; the isolated Bun worker owns Native `eKt`/`Ce`/`mt`/`St` logic.
pub trait InsightsCorpusSourceFunctions: Send + Sync {
    /// Call Native `eKt(transcript_path, undefined, undefined)` and return
    /// each leaf log's `Ce` metadata and `messages` array. This preserves the
    /// path-based `gle` small/large-file selection and streaming/tail behavior.
    /// Invalid `created`/`modified` dates are omitted, matching Native `Wt`.
    /// A worker error represents Native's caught transcript-read failure; the
    /// corpus then retains an existing metadata cache row, as `Wt` does.
    fn metadata_for_transcript<'a>(
        &'a self,
        transcript: RawTranscriptSession,
    ) -> InsightsSourceFuture<'a, Vec<NativeSessionAnalysis>>;

    /// Run the exact Native `mt`/`Te` formatter for one session.
    fn format_transcript<'a>(
        &'a self,
        metadata: &'a Utf16JsonProjection,
        messages: &'a Utf16JsonProjection,
    ) -> InsightsSourceFuture<'a, Utf16JsonProjection>;

    /// Apply the Native `start_time.localeCompare` sort and return stable
    /// source-row indices. `localeCompare` is locale-sensitive and cannot be
    /// replaced by Rust's Unicode scalar ordering.
    fn sort_sessions_by_start_time<'a>(
        &'a self,
        sessions: &'a [Utf16JsonProjection],
    ) -> InsightsSourceFuture<'a, Vec<usize>>;

    /// Evaluate Native's shallow `warmup_minimal` category predicate for each
    /// facet in insertion order. The source uses JavaScript `Object.keys` and
    /// relational coercion, so this remains in the Bun worker.
    fn warmup_only<'a>(
        &'a self,
        ordered_facets: &'a [InsightsFacetRow],
    ) -> InsightsSourceFuture<'a, Vec<bool>>;

    /// Run the exact Native `St`/`Ct` aggregation. Inputs preserve the
    /// source-order arrays; the worker converts ordered facet rows back to a
    /// JavaScript `Map` before aggregation.
    fn aggregate<'a>(
        &'a self,
        sessions: &'a [Utf16JsonProjection],
        ordered_facets: &'a [InsightsFacetRow],
    ) -> InsightsSourceFuture<'a, Utf16JsonProjection>;
}

/// A session that needs facet extraction after metadata and facet-cache reads.
#[derive(Debug, Clone)]
pub struct InsightsFacetInput {
    /// Session identifier from the UUID-named JSONL path.
    pub session_id: String,
    /// Native `ze` metadata object, kept as exact JSON source.
    pub metadata: Utf16JsonProjection,
    /// Native `mt`-formatted transcript, kept as exact UTF-16 string source.
    pub formatted_transcript: Utf16JsonProjection,
}

/// One facet row in Native `Map` insertion order.
#[derive(Debug, Clone)]
pub struct InsightsFacetRow {
    /// Session identifier used as the Native facet-map key.
    pub session_id: String,
    /// Exact cached or newly generated facet JSON object.
    pub value: Utf16JsonProjection,
}

/// The completed source-shaped aggregate and the retained facet map rows.
#[derive(Debug, Clone)]
pub struct InsightsCorpusResult {
    /// Native `St` aggregate, including `total_sessions_scanned`.
    pub aggregate: Utf16JsonProjection,
    /// Non-warmup facet rows, preserving Native Map insertion order.
    pub facets: Vec<InsightsFacetRow>,
}

/// Corpus state between local discovery/cache reads and the facet queries.
pub struct PreparedInsightsCorpus {
    /// Missing-cache facet inputs, already in Native `K`/`W` iteration order.
    pub facet_work: Vec<InsightsFacetInput>,
    config_root: PathBuf,
    source_functions: Arc<dyn InsightsCorpusSourceFunctions>,
    total_sessions_scanned: usize,
    eligible_sessions: Vec<Utf16JsonProjection>,
    cached_facets: Vec<InsightsFacetRow>,
    facet_work_ids: HashSet<String>,
}

impl PreparedInsightsCorpus {
    /// Persist successful new facets, remove warmup-only rows and run Native
    /// `St`/`Ct` aggregation.
    ///
    /// The caller must omit failed, malformed, or `Ae`-invalid query results.
    pub async fn finish(
        self,
        generated_facets: Vec<(String, Utf16JsonProjection)>,
    ) -> Result<InsightsCorpusResult, InsightsCorpusError> {
        let mut new_facets = Vec::new();
        let mut seen = self
            .cached_facets
            .iter()
            .map(|row| row.session_id.clone())
            .collect::<HashSet<_>>();

        for (session_id, facet) in generated_facets {
            if !self.facet_work_ids.contains(&session_id)
                || !seen.insert(session_id.clone())
                || !is_valid_facet(&facet)
            {
                continue;
            }
            write_facet_cache(&self.config_root, &session_id, &facet).await?;
            new_facets.push(InsightsFacetRow {
                session_id,
                value: facet,
            });
        }

        // Wt first inserts all cache hits while traversing K, then inserts
        // successful missing facets while traversing W. `Vec` preserves that
        // Map order for zt's session summary/friction/instruction prefixes.
        let mut facet_map = self.cached_facets;
        facet_map.extend(new_facets);
        let warmup_only = self.source_functions.warmup_only(&facet_map).await?;
        if warmup_only.len() != facet_map.len() {
            return Err(error_source(
                "insights source worker returned a mismatched warmup predicate count",
            ));
        }
        let warmup_ids = facet_map
            .iter()
            .zip(&warmup_only)
            .filter(|(_, is_warmup)| **is_warmup)
            .map(|(facet, _)| facet.session_id.clone())
            .collect::<HashSet<_>>();
        let sessions = self
            .eligible_sessions
            .into_iter()
            .filter(|session| {
                string_field(session, "session_id")
                    .is_none_or(|session_id| !warmup_ids.contains(&session_id))
            })
            .collect::<Vec<_>>();
        let facets = facet_map
            .into_iter()
            .zip(warmup_only)
            .filter_map(|(facet, is_warmup)| (!is_warmup).then_some(facet))
            .collect::<Vec<_>>();
        let mut aggregate = self.source_functions.aggregate(&sessions, &facets).await?;
        aggregate
            .set_field(
                "total_sessions_scanned",
                number_source(self.total_sessions_scanned as f64),
            )
            .map_err(projection_error)?;
        Ok(InsightsCorpusResult { aggregate, facets })
    }
}

/// Discover every project-local `.jsonl` file and prepare Native metadata and
/// facet-cache state from `<config-root>/projects`.
pub async fn prepare_local_insights_corpus(
    config_root: &Path,
    source_functions: Arc<dyn InsightsCorpusSourceFunctions>,
    q4_open_results: Utf16JsonProjection,
) -> Result<PreparedInsightsCorpus, InsightsCorpusError> {
    if !valid_native_command_name_open_results(&q4_open_results) {
        return Err(error_source(
            "insights Native command-name gate facts must include five boolean q4 results",
        ));
    }
    let projects_root = config_root.join("projects");
    let files = discover_local_sessions(&projects_root).await;
    let total_sessions_scanned = files.len();
    let cached_metadata = read_metadata_caches(config_root, &files).await;
    let (metadata, logs) = refresh_metadata(
        config_root,
        files,
        cached_metadata,
        source_functions.as_ref(),
        &q4_open_results,
    )
    .await?;
    let metadata = deduplicate_metadata(metadata);
    let order = source_functions
        .sort_sessions_by_start_time(&metadata)
        .await?;
    if order.len() != metadata.len() {
        return Err(error_source(
            "insights source worker returned an invalid session sort length",
        ));
    }
    let mut seen = HashSet::with_capacity(order.len());
    let mut metadata_in_native_order = Vec::with_capacity(order.len());
    for index in order {
        if index >= metadata.len() || !seen.insert(index) {
            return Err(error_source(
                "insights source worker returned an invalid session sort index",
            ));
        }
        metadata_in_native_order.push(metadata[index].clone());
    }
    let metadata = metadata_in_native_order;
    let selected_ids = metadata
        .iter()
        .filter_map(|row| string_field(row, "session_id"))
        .collect::<HashSet<_>>();
    let logs = logs
        .into_iter()
        .filter(|(session_id, _)| selected_ids.contains(session_id))
        .collect::<HashMap<_, _>>();

    let eligible_sessions = metadata
        .into_iter()
        .filter(|row| number_field(row, "user_message_count") >= 2.0)
        .filter(|row| number_field(row, "duration_minutes") >= 1.0)
        .collect::<Vec<_>>();

    let session_ids = eligible_sessions
        .iter()
        .filter_map(|row| string_field(row, "session_id"))
        .collect::<Vec<_>>();
    let cached_facets = read_facet_caches(config_root, &session_ids).await;
    let cached_ids = cached_facets
        .iter()
        .map(|row| row.session_id.clone())
        .collect::<HashSet<_>>();
    let mut facet_work = Vec::new();
    let mut facet_work_ids = HashSet::new();

    for session in &eligible_sessions {
        if facet_work.len() >= 50 {
            break;
        }
        let Some(session_id) = string_field(session, "session_id") else {
            continue;
        };
        if cached_ids.contains(&session_id) {
            continue;
        }
        let Some(log) = logs.get(&session_id) else {
            continue;
        };
        let formatted_transcript = source_functions
            .format_transcript(session, &log.messages)
            .await?;
        if formatted_transcript
            .at_pointer("")
            .and_then(JsonValueView::code_units)
            .is_none()
        {
            return Err(error_source(
                "insights source worker returned a non-string formatted transcript",
            ));
        }
        facet_work_ids.insert(session_id.clone());
        facet_work.push(InsightsFacetInput {
            session_id,
            metadata: session.clone(),
            formatted_transcript,
        });
    }

    Ok(PreparedInsightsCorpus {
        facet_work,
        config_root: config_root.to_path_buf(),
        source_functions,
        total_sessions_scanned,
        eligible_sessions,
        cached_facets,
        facet_work_ids,
    })
}

#[derive(Debug, Clone)]
struct SessionFile {
    session_id: String,
    path: PathBuf,
    mtime_ms: f64,
    size_bytes: u64,
}

#[derive(Debug, Clone)]
struct SessionLog {
    messages: Utf16JsonProjection,
}

async fn discover_local_sessions(projects_root: &Path) -> Vec<SessionFile> {
    let Ok(mut projects) = tokio::fs::read_dir(projects_root).await else {
        // Native Ht catches an unreadable projects root and returns an empty
        // corpus rather than failing the command.
        return Vec::new();
    };
    let mut project_dirs = Vec::new();
    loop {
        match projects.next_entry().await {
            Ok(Some(entry)) => {
                if entry
                    .file_type()
                    .await
                    .is_ok_and(|file_type| file_type.is_dir())
                {
                    project_dirs.push(entry.path());
                }
            }
            Ok(None) | Err(_) => break,
        }
    }

    let mut sessions = Vec::new();
    for project_dir in project_dirs {
        let Ok(mut entries) = tokio::fs::read_dir(&project_dir).await else {
            continue;
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) | Err(_) => break,
            };
            if !entry
                .file_type()
                .await
                .is_ok_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_uuid_stem(stem) {
                continue;
            }
            let Ok(metadata) = tokio::fs::metadata(entry.path()).await else {
                continue;
            };
            sessions.push(SessionFile {
                session_id: stem.to_owned(),
                path: entry.path(),
                mtime_ms: system_time_ms(metadata.modified().unwrap_or(UNIX_EPOCH)),
                size_bytes: metadata.len(),
            });
        }
    }
    // Ht sorts newest mtime first. Stable sorting preserves the filesystem's
    // original order for equal mtimes, as JavaScript Array#sort does.
    sessions.sort_by(|left, right| {
        if left.mtime_ms == right.mtime_ms {
            std::cmp::Ordering::Equal
        } else {
            right
                .mtime_ms
                .partial_cmp(&left.mtime_ms)
                .unwrap_or(std::cmp::Ordering::Equal)
        }
    });
    sessions
}

async fn read_metadata_caches(
    config_root: &Path,
    files: &[SessionFile],
) -> Vec<Option<Utf16JsonProjection>> {
    let mut cached = Vec::with_capacity(files.len());
    for batch in files.chunks(50) {
        for file in batch {
            cached.push(read_metadata_cache(config_root, &file.session_id).await);
        }
    }
    cached
}

async fn read_metadata_cache(config_root: &Path, session_id: &str) -> Option<Utf16JsonProjection> {
    let path = metadata_cache_path(config_root, session_id);
    let raw = tokio::fs::read_to_string(path).await.ok()?;
    let projection = Utf16JsonProjection::parse(&raw).ok()?;
    if projection
        .at_pointer("")
        .is_some_and(JsonValueView::is_null)
    {
        return None;
    }
    if !valid_session_metadata(&projection) {
        return None;
    }
    Some(with_metadata_defaults(projection))
}

async fn read_facet_caches(config_root: &Path, session_ids: &[String]) -> Vec<InsightsFacetRow> {
    let mut facets = Vec::new();
    for batch in session_ids.chunks(50) {
        for session_id in batch {
            let path = facet_cache_path(config_root, session_id);
            let Ok(raw) = tokio::fs::read_to_string(&path).await else {
                continue;
            };
            let Some(facet) = Utf16JsonProjection::parse(&raw).ok() else {
                let _ = tokio::fs::remove_file(&path).await;
                continue;
            };
            if !is_valid_facet(&facet) {
                // Native bt/De delete malformed or Ae-invalid cache records.
                let _ = tokio::fs::remove_file(&path).await;
                continue;
            }
            facets.push(InsightsFacetRow {
                session_id: session_id.clone(),
                value: facet,
            });
        }
    }
    facets
}

fn metadata_cache_path(config_root: &Path, session_id: &str) -> PathBuf {
    config_root
        .join("usage-data")
        .join("session-meta")
        .join(format!("{session_id}.json"))
}

fn facet_cache_path(config_root: &Path, session_id: &str) -> PathBuf {
    config_root
        .join("usage-data")
        .join("facets")
        .join(format!("{session_id}.json"))
}

async fn write_metadata_cache(config_root: &Path, metadata: &Utf16JsonProjection) {
    let Some(session_id) = string_field(metadata, "session_id") else {
        return;
    };
    let path = metadata_cache_path(config_root, &session_id);
    let Ok(raw) = metadata.to_pretty_json_string() else {
        return;
    };
    if tokio::fs::create_dir_all(path.parent().unwrap_or(config_root))
        .await
        .is_ok()
    {
        let _ = write_private_file(&path, raw.as_bytes()).await;
    }
}

async fn write_facet_cache(
    config_root: &Path,
    session_id: &str,
    facet: &Utf16JsonProjection,
) -> Result<(), InsightsCorpusError> {
    let path = facet_cache_path(config_root, session_id);
    let _ = tokio::fs::create_dir_all(path.parent().unwrap_or(config_root)).await;
    let raw = facet
        .to_pretty_json_string()
        .map_err(|error| error_source(format!("insights facet serialization error: {error}")))?;
    write_private_file(&path, raw.as_bytes())
        .await
        .map_err(|error| error_source(format!("insights facet cache write error: {error}")))?;
    Ok(())
}

async fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path).await?;
    file.write_all(contents).await?;
    file.flush().await
}

async fn refresh_metadata(
    config_root: &Path,
    files: Vec<SessionFile>,
    cached: Vec<Option<Utf16JsonProjection>>,
    source_functions: &dyn InsightsCorpusSourceFunctions,
    q4_open_results: &Utf16JsonProjection,
) -> Result<(Vec<Utf16JsonProjection>, HashMap<String, SessionLog>), InsightsCorpusError> {
    const REFRESH_CAP: usize = 200;
    const RECENT_METADATA_WINDOW: usize = 60;
    const READ_BATCH: usize = 10;
    let mut reuse = Vec::new();
    let mut work = Vec::new();
    let mut uncached_count = 0;
    let mut stale_count = 0;

    for (index, (file, cached)) in files.into_iter().zip(cached).enumerate() {
        let refresh_gate = cached
            .as_ref()
            .is_some_and(|row| field_view(row, "bash_would_prompt_count").is_some())
            || index >= RECENT_METADATA_WINDOW;
        let fresh = cached.as_ref().is_some_and(|row| {
            refresh_gate
                && (field_view(row, "transcript_mtime").is_none()
                    || number_field(row, "transcript_mtime") >= file.mtime_ms)
        });
        if let Some(row) = cached.as_ref().filter(|_| fresh) {
            reuse.push(row.clone());
            continue;
        }
        if cached.is_none() {
            if uncached_count >= REFRESH_CAP {
                continue;
            }
            uncached_count += 1;
        } else if stale_count < REFRESH_CAP {
            stale_count += 1;
        } else if let Some(row) = cached {
            reuse.push(row);
            continue;
        }
        work.push((file, cached));
    }

    let mut metadata = reuse;
    let mut logs = HashMap::new();
    for batch in work.chunks(READ_BATCH) {
        let mut selected_by_id = Vec::<(String, Utf16JsonProjection)>::new();
        let mut selected_positions = HashMap::<String, usize>::new();
        let mut batch_logs = Vec::<(String, SessionLog)>::new();
        for (file, cached) in batch {
            let transcript_path = path_source(&file.path)?;
            let logs_for_file = match source_functions
                .metadata_for_transcript(RawTranscriptSession {
                    session_id: file.session_id.clone(),
                    transcript_path,
                    transcript_mtime_ms: file.mtime_ms,
                    file_size_bytes: file.size_bytes,
                    q4_open_results: q4_open_results.clone(),
                })
                .await
            {
                Ok(logs) => logs,
                Err(_) => {
                    if let Some(cached) = cached {
                        metadata.push(cached.clone());
                    }
                    continue;
                }
            };
            let mut valid_log_seen = false;
            for log in logs_for_file {
                if log.metadata.object_entries().is_none() || log.messages.array_len("").is_none() {
                    return Err(error_source(
                        "insights source worker returned malformed session metadata or messages",
                    ));
                }
                if generated_facet_session(&log.messages) {
                    continue;
                }
                let mut row = log.metadata;
                if row
                    .set_field("transcript_mtime", number_source(file.mtime_ms))
                    .is_err()
                {
                    return Err(error_source(
                        "insights source worker metadata is not a JSON object",
                    ));
                }
                let Some(id) = string_field(&row, "session_id") else {
                    continue;
                };
                valid_log_seen = true;
                if let Some(index) = selected_positions.get(&id).copied() {
                    if better_metadata(&row, &selected_by_id[index].1) {
                        selected_by_id[index].1 = row;
                    }
                } else {
                    selected_positions.insert(id.clone(), selected_by_id.len());
                    selected_by_id.push((id.clone(), row));
                }
                batch_logs.push((
                    id,
                    SessionLog {
                        messages: log.messages,
                    },
                ));
            }
            if !valid_log_seen {
                if let Some(cached) = cached {
                    metadata.push(cached.clone());
                }
            }
        }
        for (_, row) in selected_by_id {
            write_metadata_cache(config_root, &row).await;
            metadata.push(row);
        }
        for (session_id, log) in batch_logs {
            logs.insert(session_id, log);
        }
    }
    Ok((metadata, logs))
}

fn path_source(path: &Path) -> Result<Utf16JsonProjection, InsightsCorpusError> {
    // Node/Bun receive filesystem paths as JS strings. Match their UTF-8
    // replacement decoding for non-UTF-8 Unix path bytes, then preserve the
    // resulting JavaScript UTF-16 units in the worker request carrier.
    let text = path.to_string_lossy().into_owned();
    let code_units = text.encode_utf16().collect();
    Utf16JsonProjection::root_string(text, code_units).map_err(projection_error)
}

fn valid_session_metadata(row: &Utf16JsonProjection) -> bool {
    ["session_id", "project_path", "start_time", "first_prompt"]
        .into_iter()
        .all(|field| string_field(row, field).is_some())
        && [
            "duration_minutes",
            "user_message_count",
            "assistant_message_count",
            "git_commits",
            "git_pushes",
            "input_tokens",
            "output_tokens",
        ]
        .into_iter()
        .all(|field| required_number(row, field))
        && optional_number(row, "transcript_mtime")
        && optional_record_of_numbers(row, "tool_counts")
        && optional_record_of_numbers(row, "languages")
        && optional_string(row, "summary")
        && optional_number(row, "user_interruptions")
        && optional_number_array(row, "user_response_times")
        && optional_number(row, "tool_errors")
        && optional_record_of_numbers(row, "tool_error_categories")
        && optional_bool(row, "uses_task_agent")
        && optional_bool(row, "uses_mcp")
        && optional_bool(row, "uses_web_search")
        && optional_bool(row, "uses_web_fetch")
        && optional_number(row, "lines_added")
        && optional_number(row, "lines_removed")
        && optional_number(row, "files_modified")
        && optional_number_array(row, "message_hours")
        && optional_string_array(row, "user_message_timestamps")
        && optional_string(row, "permission_mode")
        && optional_number(row, "bash_would_prompt_count")
        && optional_number(row, "file_edit_tool_count")
        && optional_number(row, "destructive_command_count")
}

fn with_metadata_defaults(mut row: Utf16JsonProjection) -> Utf16JsonProjection {
    // Native `Le` supplies these defaults after the `ze` cache schema has
    // accepted a row. Null is rejected by `ze`, so only missing fields default.
    let defaults = [
        ("tool_counts", "{}"),
        ("languages", "{}"),
        ("tool_error_categories", "{}"),
        ("user_response_times", "[]"),
        ("message_hours", "[]"),
        ("user_message_timestamps", "[]"),
        ("user_interruptions", "0"),
        ("tool_errors", "0"),
        ("lines_added", "0"),
        ("lines_removed", "0"),
        ("files_modified", "0"),
        ("uses_task_agent", "false"),
        ("uses_mcp", "false"),
        ("uses_web_search", "false"),
        ("uses_web_fetch", "false"),
    ];
    for (field, source) in defaults {
        if row.get_property(field).is_none() {
            let value = Utf16JsonProjection::parse(source)
                .expect("static JSON defaults are valid projections");
            row.set_field(field, value)
                .expect("validated metadata cache rows are objects");
        }
    }
    row
}

fn required_number(row: &Utf16JsonProjection, field: &str) -> bool {
    field_view(row, field)
        .and_then(JsonValueView::as_number)
        .is_some_and(f64::is_finite)
}

fn optional_number(row: &Utf16JsonProjection, field: &str) -> bool {
    row.get_property(field).is_none() || required_number(row, field)
}

fn optional_bool(row: &Utf16JsonProjection, field: &str) -> bool {
    row.get_property(field).is_none()
        || field_view(row, field)
            .and_then(JsonValueView::as_bool)
            .is_some()
}

fn optional_string(row: &Utf16JsonProjection, field: &str) -> bool {
    row.get_property(field).is_none() || string_field(row, field).is_some()
}

fn optional_number_array(row: &Utf16JsonProjection, field: &str) -> bool {
    let Some(value) = field_view(row, field) else {
        return true;
    };
    let Some(length) = value.array_len() else {
        return false;
    };
    (0..length).all(|index| {
        value
            .get_index(index)
            .and_then(JsonValueView::as_number)
            .is_some_and(f64::is_finite)
    })
}

fn optional_string_array(row: &Utf16JsonProjection, field: &str) -> bool {
    let Some(value) = field_view(row, field) else {
        return true;
    };
    let Some(length) = value.array_len() else {
        return false;
    };
    (0..length).all(|index| {
        value
            .get_index(index)
            .and_then(JsonValueView::code_units)
            .is_some()
    })
}

fn optional_record_of_numbers(row: &Utf16JsonProjection, field: &str) -> bool {
    let Some(value) = field_view(row, field) else {
        return true;
    };
    let Some(entries) = value.object_entries() else {
        return false;
    };
    entries
        .into_iter()
        .all(|entry| entry.value.as_number().is_some_and(f64::is_finite))
}

fn deduplicate_metadata(rows: Vec<Utf16JsonProjection>) -> Vec<Utf16JsonProjection> {
    let mut selected = Vec::<Utf16JsonProjection>::new();
    let mut positions = HashMap::<String, usize>::new();
    for row in rows {
        let Some(session_id) = string_field(&row, "session_id") else {
            continue;
        };
        if let Some(index) = positions.get(&session_id).copied() {
            if better_metadata(&row, &selected[index]) {
                selected[index] = row;
            }
        } else {
            positions.insert(session_id, selected.len());
            selected.push(row);
        }
    }
    selected
}

fn better_metadata(candidate: &Utf16JsonProjection, current: &Utf16JsonProjection) -> bool {
    let candidate_users = number_field(candidate, "user_message_count");
    let current_users = number_field(current, "user_message_count");
    if candidate_users != current_users {
        return candidate_users > current_users;
    }
    number_field(candidate, "duration_minutes") > number_field(current, "duration_minutes")
}

fn generated_facet_session(messages: &Utf16JsonProjection) -> bool {
    const MARKERS: [&str; 2] = ["RESPOND WITH ONLY A VALID JSON OBJECT", "record_facets"];
    let Some(count) = messages.array_len("") else {
        return false;
    };
    for index in 0..count.min(5) {
        let pointer = format!("/{index}");
        let Some(message) = messages.at_pointer(&pointer) else {
            continue;
        };
        if message.get_property("type").and_then(JsonValueView::as_str) != Some("user")
            || message.get_property("message").is_none()
        {
            continue;
        }
        let Some(content) = messages
            .at_pointer(&format!("/{index}/message/content"))
            .and_then(JsonValueView::code_units)
        else {
            continue;
        };
        if MARKERS.iter().any(|marker| {
            let marker = marker.encode_utf16().collect::<Vec<_>>();
            content.windows(marker.len()).any(|part| part == marker)
        }) {
            return true;
        }
    }
    false
}

fn is_valid_facet(facet: &Utf16JsonProjection) -> bool {
    ["underlying_goal", "outcome", "brief_summary"]
        .into_iter()
        .all(|field| facet.string_at(&format!("/{field}")).is_some())
        && [
            "goal_categories",
            "user_satisfaction_counts",
            "friction_counts",
        ]
        .into_iter()
        .all(|field| {
            facet.at_pointer(&format!("/{field}")).is_some_and(|value| {
                !value.is_null()
                    && (value.object_entries().is_some() || value.array_len().is_some())
            })
        })
}

fn valid_native_command_name_open_results(value: &Utf16JsonProjection) -> bool {
    value.object_entries().is_some()
        && [
            "fleetFork",
            "fleetBackground",
            "daemon",
            "skillDoctor",
            "logout",
        ]
        .into_iter()
        .all(|field| {
            value
                .get_property(field)
                .and_then(JsonValueView::as_bool)
                .is_some()
        })
}

fn field_view<'a>(row: &'a Utf16JsonProjection, field: &str) -> Option<JsonValueView<'a>> {
    row.get_property(field)
}

fn string_field(row: &Utf16JsonProjection, field: &str) -> Option<String> {
    field_view(row, field)
        .and_then(JsonValueView::as_str)
        .map(str::to_owned)
}

fn number_field(row: &Utf16JsonProjection, field: &str) -> f64 {
    field_view(row, field)
        .and_then(JsonValueView::as_number)
        .unwrap_or(f64::NAN)
}

fn number_source(number: f64) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&number.to_string())
        .expect("finite filesystem and count values are valid JSON numbers")
}

fn error_source(message: impl AsRef<str>) -> InsightsCorpusError {
    let text = message.as_ref().to_owned();
    Utf16JsonProjection::root_string(text.clone(), text.encode_utf16().collect())
        .expect("valid Rust strings always produce valid UTF-16 projections")
}

fn projection_error(error: impl std::fmt::Display) -> InsightsCorpusError {
    error_source(format!("insights corpus projection error: {error}"))
}

fn system_time_ms(time: SystemTime) -> f64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as f64,
        Err(error) => -(error.duration().as_millis() as f64),
    }
}

fn is_uuid_stem(value: &str) -> bool {
    // Native `Jt` is `/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i`;
    // it imposes no UUID version or variant-bit restriction.
    value.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| value.as_bytes()[index] == b'-')
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("test JSON source is valid")
    }

    fn metadata(session_id: &str, users: usize, minutes: usize) -> Utf16JsonProjection {
        projection(&format!(
            r#"{{"session_id":"{session_id}","project_path":"/project","start_time":"2026-10-11T00:00:00.000Z","duration_minutes":{minutes},"user_message_count":{users},"assistant_message_count":1,"git_commits":0,"git_pushes":0,"input_tokens":0,"output_tokens":0,"first_prompt":"first"}}"#
        ))
    }

    #[test]
    fn metadata_cache_schema_and_native_defaults_are_applied_in_order() {
        let row = metadata("00000000-0000-4000-8000-000000000001", 2, 1);
        assert!(valid_session_metadata(&row));
        let row = with_metadata_defaults(row);
        assert_eq!(row.number_at("/user_interruptions"), Some(0.0));
        assert_eq!(
            row.at_pointer("/uses_task_agent")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(row.array_len("/user_response_times"), Some(0));
        let serialized = row.to_json_string().expect("serialize projection");
        assert!(
            serialized.find("\"user_interruptions\"").unwrap()
                < serialized.find("\"uses_task_agent\"").unwrap()
        );

        let invalid = projection(
            r#"{"session_id":"x","project_path":"/p","start_time":"t","duration_minutes":1,"user_message_count":2,"assistant_message_count":1,"git_commits":0,"git_pushes":0,"input_tokens":0,"output_tokens":0,"first_prompt":"p","tool_counts":null}"#,
        );
        assert!(!valid_session_metadata(&invalid));
    }

    #[test]
    fn generated_facet_exclusion_checks_only_first_five_string_user_messages() {
        let matching = projection(
            r#"[{"type":"assistant"},{"type":"user","message":{"content":"please record_facets now"}}]"#,
        );
        assert!(generated_facet_session(&matching));

        let lone_surrogate_marker =
            projection(r#"[{"type":"user","message":{"content":"\ud800 record_facets"}}]"#);
        assert_eq!(
            lone_surrogate_marker.string_units("/0/message/content"),
            Some(vec![
                0xd800, 0x20, 0x72, 0x65, 0x63, 0x6f, 0x72, 0x64, 0x5f, 0x66, 0x61, 0x63, 0x65,
                0x74, 0x73
            ])
        );
        assert!(generated_facet_session(&lone_surrogate_marker));

        let array_content = projection(
            r#"[{"type":"user","message":{"content":[{"type":"text","text":"record_facets"}]}}]"#,
        );
        assert!(!generated_facet_session(&array_content));

        let sixth_message = projection(
            r#"[{"type":"assistant"},{"type":"assistant"},{"type":"assistant"},{"type":"assistant"},{"type":"assistant"},{"type":"user","message":{"content":"record_facets"}}]"#,
        );
        assert!(!generated_facet_session(&sixth_message));
    }

    #[test]
    fn facet_validation_matches_native_shallow_ae_contract() {
        let arrays_are_objects_to_native_ae = projection(
            r#"{"underlying_goal":"","outcome":"","brief_summary":"","goal_categories":[],"user_satisfaction_counts":[],"friction_counts":[]}"#,
        );
        assert!(is_valid_facet(&arrays_are_objects_to_native_ae));

        let explicit_null_is_rejected = projection(
            r#"{"underlying_goal":"","outcome":"","brief_summary":"","goal_categories":null,"user_satisfaction_counts":{},"friction_counts":{}}"#,
        );
        assert!(!is_valid_facet(&explicit_null_is_rejected));

        for primitive in ["\"text\"", "1", "false"] {
            let invalid = projection(&format!(
                r#"{{"underlying_goal":"","outcome":"","brief_summary":"","goal_categories":{primitive},"user_satisfaction_counts":{{}},"friction_counts":{{}}}}"#
            ));
            assert!(
                !is_valid_facet(&invalid),
                "primitive {primitive} is not an object"
            );
        }

        let surrogate_strings = projection(
            r#"{"underlying_goal":"\ud800","outcome":"\ud800","brief_summary":"\ud800","goal_categories":{},"user_satisfaction_counts":{},"friction_counts":{}}"#,
        );
        assert!(is_valid_facet(&surrogate_strings));
        assert_eq!(
            surrogate_strings.string_units("/underlying_goal"),
            Some(vec![0xd800])
        );
    }

    #[test]
    fn transcript_mtime_uses_javascript_date_integer_milliseconds() {
        let positive = UNIX_EPOCH + std::time::Duration::from_nanos(1_234_567);
        let negative = UNIX_EPOCH - std::time::Duration::from_nanos(1_234_567);
        assert_eq!(system_time_ms(positive), 1.0);
        assert_eq!(system_time_ms(negative), -1.0);
    }

    #[test]
    fn native_command_name_gate_requires_all_explicit_boolean_facts() {
        let complete = projection(
            r#"{"fleetFork":false,"fleetBackground":true,"daemon":false,"skillDoctor":false,"logout":true}"#,
        );
        assert!(valid_native_command_name_open_results(&complete));

        let missing = projection(
            r#"{"fleetFork":false,"fleetBackground":true,"daemon":false,"skillDoctor":false}"#,
        );
        assert!(!valid_native_command_name_open_results(&missing));

        let wrong_type = projection(
            r#"{"fleetFork":false,"fleetBackground":true,"daemon":false,"skillDoctor":"false","logout":true}"#,
        );
        assert!(!valid_native_command_name_open_results(&wrong_type));
    }

    #[test]
    fn metadata_dedup_keeps_first_map_position_and_native_better_row() {
        let rows = deduplicate_metadata(vec![
            metadata("00000000-0000-4000-8000-000000000001", 2, 2),
            metadata("00000000-0000-4000-8000-000000000002", 3, 1),
            metadata("00000000-0000-4000-8000-000000000001", 4, 1),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            string_field(&rows[0], "session_id").as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(number_field(&rows[0], "user_message_count"), 4.0);
        assert_eq!(
            string_field(&rows[1], "session_id").as_deref(),
            Some("00000000-0000-4000-8000-000000000002")
        );
    }

    #[test]
    fn filename_filter_requires_canonical_uuid_shape() {
        assert!(is_uuid_stem("00000000-0000-4000-8000-000000000001"));
        assert!(is_uuid_stem("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"));
        assert!(!is_uuid_stem("not-a-session-id"));
        assert!(!is_uuid_stem("00000000000040008000000000000001"));
    }

    #[test]
    fn discovered_path_is_sent_as_a_javascript_string_projection() {
        let path = Path::new("/tmp/会话 transcript.jsonl");
        let projection = path_source(path).expect("path is a valid JS source string");
        let expected = path.to_string_lossy().encode_utf16().collect::<Vec<_>>();
        assert_eq!(projection.string_units(""), Some(expected));
    }
}
