//! Source-owned, content-free Assistant activity. A write-ahead intent is sealed
//! by the exact committed transcript row. Result ordinals have no gaps for tools,
//! edits or streamed text blocks. Immutable identity indexes survive transcript GC.
use lingxi_core::host::{rooted_fs as fs, FsError};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Row;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_UUIDS: usize = 16;
const PAGE: u64 = 50;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActivitySummary {
    pub source_id: String,
    pub cursor: u64,
    pub baseline_cursor: u64,
    pub last_meaningful_at: i64,
    pub preview_kind: Option<String>,
    pub pending_action_count: u32,
    #[serde(default)]
    pub pending_action_at: i64,
    #[serde(default)]
    pub pending_action_revision: u64,
    #[serde(default)]
    pub pending_action_kind: Option<ActionKind>,
    pub failed: bool,
    #[serde(default)]
    pub failure_at: i64,
    #[serde(default)]
    pub failure_revision: u64,
    pub source_revision: u64,
    pub latest_result_uuids: Vec<String>,
    /// Host-local proof that the user explicitly supplied the catalog title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_title_digest: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityEntry {
    pub cursor: u64,
    pub result_uuids: Vec<String>,
    pub kind: String,
    pub at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityPage {
    pub items: Vec<ActivityEntry>,
    pub cursor: u64,
    pub baseline_cursor: u64,
    pub has_more: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Candidate {
    group: String,
    outer: String,
    kind: String,
    at: i64,
}

fn digest(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn error(e: impl std::fmt::Display) -> FsError {
    FsError::Io(e.to_string())
}

fn classify(row: &Row, session: Uuid) -> Option<Candidate> {
    let outer = Uuid::parse_str(row.string_at("/uuid")?).ok()?.to_string();
    if Uuid::parse_str(row.string_at("/sessionId")?).ok()? != session
        || row.at_pointer("/isSidechain").and_then(|v| v.as_bool()) == Some(true)
        || row
            .at_pointer("/isCompactSummary")
            .and_then(|v| v.as_bool())
            == Some(true)
    {
        return None;
    }
    let (kind, group) = match row.string_at("/type")? {
        "assistant" if row.at_pointer("/isMeta").and_then(|v| v.as_bool()) != Some(true) => {
            let failed = row
                .at_pointer("/isApiErrorMessage")
                .and_then(|v| v.as_bool())
                == Some(true);
            if !failed
                && !matches!(
                    row.string_at("/message/stop_reason"),
                    Some(
                        "end_turn" | "stop_sequence" | "refusal" | "max_tokens" | "stop" | "length"
                    )
                )
            {
                return None;
            }
            let visible = (0..row.array_len("/message/content").unwrap_or(0)).any(|index| {
                let block = format!("/message/content/{index}");
                row.string_at(&format!("{block}/type")) == Some("text")
                    && row
                        .string_units(&format!("{block}/text"))
                        .is_some_and(|units| !units.is_empty())
            }) || row
                .string_at("/message/content")
                .is_some_and(|text| !text.trim().is_empty());
            if !visible {
                return None;
            }
            let group = row
                .string_at("/logicalMessageId")
                .or_else(|| row.string_at("/message/id"))
                .unwrap_or(&outer);
            (
                if failed { "failure" } else { "reply" },
                format!("assistant/{group}"),
            )
        }
        "user"
            if matches!(
                row.string_at("/fusionStatus"),
                Some("analyzed" | "unanalyzed")
            ) && row.string_at("/fusionRunId").is_some()
                && row
                    .at_pointer("/isModelContextExcluded")
                    .and_then(|v| v.as_bool())
                    == Some(true) =>
        {
            (
                "taskResult",
                format!("fusion/{}", row.string_at("/fusionRunId")?),
            )
        }
        "attachment" if row.string_at("/attachment/commandMode") == Some("task-notification") => {
            let group = row
                .string_at("/attachment/taskDelivery/taskId")
                .or_else(|| row.string_at("/attachment/delivery_id"))
                .or_else(|| row.string_at("/attachment/source_uuid"))
                .unwrap_or(&outer);
            ("taskResult", format!("task/{group}"))
        }
        _ => return None,
    };
    let at = row
        .string_at("/timestamp")
        .and_then(|s| s.parse::<jiff::Timestamp>().ok())
        .map_or(0, |t| t.as_millisecond());
    Some(Candidate {
        group: digest(group.as_bytes()),
        outer,
        kind: kind.into(),
        at,
    })
}

fn options(overwrite: bool) -> fs::AtomicWriteOptions {
    fs::AtomicWriteOptions {
        overwrite,
        create_parents: true,
        dir_mode: 0o700,
        file_mode: 0o600,
    }
}

/// Stable host namespace. Concurrent first users converge through no-replace publication.
pub fn source_id(home: &Path) -> Result<String, FsError> {
    let pin = fs::root_identity(home)?;
    let path = Path::new("inbox-activity/source-id");
    match fs::read_to_string_pinned(home, path, Some(&pin)) {
        Ok(text) => return Uuid::parse_str(&text).map(|v| v.to_string()).map_err(error),
        Err(FsError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    let id = Uuid::new_v4().to_string();
    match fs::atomic_write_pinned(home, path, id.as_bytes(), options(false), &pin) {
        Ok(()) => Ok(id),
        Err(FsError::AlreadyExists(_)) => {
            Uuid::parse_str(&fs::read_to_string_pinned(home, path, Some(&pin))?)
                .map(|v| v.to_string())
                .map_err(error)
        }
        Err(e) => Err(e),
    }
}

struct Store {
    home: PathBuf,
    dir: PathBuf,
    pin: fs::RootIdentity,
    session: Uuid,
    _guard: fs::RootedFileLock,
}

impl Store {
    fn open(home: &Path, session: Uuid) -> Result<Self, FsError> {
        let pin = fs::root_identity(home)?;
        let dir = Path::new("inbox-activity").join(session.to_string());
        let guard = fs::lock_exclusive_pinned(home, &dir.join("lock"), 0o700, 0o600, Some(&pin))?;
        Ok(Self {
            home: home.into(),
            dir,
            pin,
            session,
            _guard: guard,
        })
    }
    fn get<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, FsError> {
        match fs::read_to_string_pinned(&self.home, &self.dir.join(name), Some(&self.pin)) {
            Ok(text) => serde_json::from_str(&text).map(Some).map_err(error),
            Err(FsError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn put(&self, name: &str, value: &impl Serialize) -> Result<(), FsError> {
        fs::atomic_write_pinned(
            &self.home,
            &self.dir.join(name),
            &serde_json::to_vec(value).map_err(error)?,
            options(true),
            &self.pin,
        )
    }

    fn summary(&self, path: &Path) -> Result<ActivitySummary, FsError> {
        if let Some(mut s) = self.get("summary.json")? {
            self.recover(&mut s)?;
            self.recover_terminal(&mut s)?;
            self.refresh_actions(&mut s)?;
            return Ok(s);
        }
        let mut s = ActivitySummary {
            source_id: source_id(&self.home)?,
            source_revision: 1,
            ..Default::default()
        };
        // Capture B while holding the same lock every eligible writer takes before
        // appending. Persisted entries below B also deduplicate later imports.
        // B needs only a compact membership index. Historical ordinals are
        // already read and never paged. Avoid four durable files per old row.
        let mut history = std::collections::BTreeSet::<[u8; 32]>::new();
        if let Some(file) = self.transcript(path)? {
            for line in BufReader::new(file).split(b'\n') {
                let bytes = line.map_err(error)?;
                let Some(c) = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|line| Row::parse(line).ok())
                    .and_then(|row| classify(&row, self.session))
                else {
                    continue;
                };
                let outer = Sha256::digest(c.outer.as_bytes()).into();
                if !history.insert(outer) {
                    continue;
                }
                let group = Sha256::digest(c.group.as_bytes()).into();
                if history.insert(group) {
                    s.cursor += 1;
                    s.last_meaningful_at = s.last_meaningful_at.max(c.at);
                    s.preview_kind = Some(c.kind.clone());
                    s.failed = c.kind == "failure";
                    s.latest_result_uuids.clear();
                }
                if s.latest_result_uuids.len() == MAX_UUIDS {
                    s.latest_result_uuids.remove(0);
                }
                s.latest_result_uuids.push(c.outer);
            }
        }
        if !history.is_empty() {
            fs::atomic_write_stream_pinned(
                &self.home,
                &self.dir.join("baseline-index"),
                options(true),
                &self.pin,
                |out| {
                    use std::io::Write;
                    for hash in history {
                        out.write_all(&hash).map_err(error)?;
                    }
                    Ok(())
                },
            )?;
        }
        s.baseline_cursor = s.cursor;
        self.put("summary.json", &s)?;
        Ok(s)
    }
    fn transcript(&self, path: &Path) -> Result<Option<std::fs::File>, FsError> {
        let relative = path.strip_prefix(&self.home).map_err(error)?;
        match fs::open_read_file_pinned(&self.home, relative, Some(&self.pin)) {
            Ok(file) => {
                fs::validate_open_file_single_link(&file)?;
                Ok(Some(file))
            }
            Err(FsError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn apply(
        &self,
        summary: &mut ActivitySummary,
        c: &Candidate,
        history: bool,
    ) -> Result<(), FsError> {
        let group_key = format!("groups/{}", c.group);
        let row_key = format!("rows/{}", c.outer);
        let existing: Option<u64> = self.get(&row_key)?;
        let ordinal = match existing.or(self.get(&group_key)?) {
            Some(ordinal) => ordinal,
            None => summary
                .cursor
                .checked_add(1)
                .ok_or_else(|| error("activity cursor exhausted"))?,
        };
        let entry_key = format!("entries/{ordinal}.json");
        let mut entry = self
            .get::<ActivityEntry>(&entry_key)?
            .unwrap_or_else(|| ActivityEntry {
                cursor: ordinal,
                kind: c.kind.clone(),
                at: c.at,
                result_uuids: Vec::new(),
            });
        if existing.is_none() && !entry.result_uuids.contains(&c.outer) {
            if entry.result_uuids.len() == MAX_UUIDS {
                entry.result_uuids.remove(0);
            }
            entry.result_uuids.push(c.outer.clone());
            self.put(&entry_key, &entry)?;
        }
        // Entry and identity indexes reach disk before the public summary. Replaying
        // the same intent finishes a partially installed ordinal, including after GC.
        if existing.is_none() {
            self.put(&group_key, &ordinal)?;
            self.put(&row_key, &ordinal)?;
        }
        if ordinal > summary.cursor
            || (ordinal == summary.cursor && summary.latest_result_uuids != entry.result_uuids)
        {
            summary.cursor = ordinal;
            summary.last_meaningful_at = summary.last_meaningful_at.max(entry.at);
            summary.preview_kind = Some(entry.kind.clone());
            summary.latest_result_uuids = entry.result_uuids;
            summary.source_revision += 1;
            if !history {
                self.put("summary.json", summary)?;
            }
        }
        Ok(())
    }
    fn historical(&self, c: &Candidate) -> Result<bool, FsError> {
        let mut index = match fs::open_read_file_pinned(
            &self.home,
            &self.dir.join("baseline-index"),
            Some(&self.pin),
        ) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let bytes = index.metadata().map_err(error)?.len();
        if bytes % 32 != 0 {
            return Err(error("Invalid Assistant baseline index"));
        }
        for id in [&c.group, &c.outer] {
            let needle: [u8; 32] = Sha256::digest(id.as_bytes()).into();
            let (mut low, mut high) = (0, bytes / 32);
            while low < high {
                let mid = low + (high - low) / 2;
                index.seek(SeekFrom::Start(mid * 32)).map_err(error)?;
                let mut record = [0; 32];
                index.read_exact(&mut record).map_err(error)?;
                match record.cmp(&needle) {
                    std::cmp::Ordering::Less => low = mid + 1,
                    std::cmp::Ordering::Equal => return Ok(true),
                    std::cmp::Ordering::Greater => high = mid,
                }
            }
        }
        Ok(false)
    }

    fn record_committed(
        &self,
        s: &mut ActivitySummary,
        candidate: &Candidate,
    ) -> Result<(), FsError> {
        // A task notification is itself the host's final delivery record. A
        // provider stop is provisional: stop hooks and budgets may continue
        // the same human turn. Only the orchestrator's final disposition
        // promotes its committed group, with all visible blocks in one update.
        if self.historical(candidate)? {
            return Ok(());
        }
        if candidate.kind == "taskResult" {
            return self.apply(s, candidate, false);
        }
        if self
            .get::<u64>(&format!("rows/{}", candidate.outer))?
            .is_some()
            || self
                .get::<u64>(&format!("groups/{}", candidate.group))?
                .is_some()
        {
            return Ok(());
        }
        let key = format!("staged/{}.json", candidate.group);
        let mut blocks: Vec<Candidate> = self.get(&key)?.unwrap_or_default();
        if !blocks.iter().any(|b| b.outer == candidate.outer) {
            if blocks.len() == MAX_UUIDS {
                blocks.remove(0);
            }
            blocks.push(candidate.clone());
            self.put(&key, &blocks)?;
        }
        self.put(
            &format!("staged-rows/{}", candidate.outer),
            &candidate.group,
        )
    }

    fn recover_terminal(&self, s: &mut ActivitySummary) -> Result<(), FsError> {
        let Some(terminal): Option<Terminal> = self.get("terminal.json")? else {
            return Ok(());
        };
        let old = s.clone();
        for block in &terminal.blocks {
            self.apply(s, block, true)?;
        }
        if s.failed != terminal.failed {
            s.failed = terminal.failed;
            if terminal.failed {
                s.failure_at = terminal.at;
            }
            s.failure_revision = s
                .failure_revision
                .checked_add(1)
                .ok_or_else(|| error("Failure revision exhausted"))?;
            s.source_revision += 1;
        }
        if s != &old {
            self.put("summary.json", s)?;
        }
        if let Some(block) = terminal.blocks.first() {
            self.remove_if_present(&format!("staged/{}.json", block.group))?;
            for block in &terminal.blocks {
                self.remove_if_present(&format!("staged-rows/{}", block.outer))?;
            }
        }
        self.remove_if_present("terminal.json")
    }

    fn remove_if_present(&self, name: &str) -> Result<(), FsError> {
        match fs::remove_file_pinned(&self.home, &self.dir.join(name), &self.pin) {
            Ok(()) | Err(FsError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn commit_intent(&self, s: &mut ActivitySummary, intent: &Intent) -> Result<(), FsError> {
        // Keep only the location and digest outside the source transcript.
        // Replayed edits sharing a UUID may not replace the original proof.
        let key = format!("locations/{}", intent.candidate.outer);
        if self.get::<Intent>(&key)?.is_none() {
            self.put(&key, intent)?;
        }
        self.record_committed(s, &intent.candidate)
    }

    fn recover(&self, s: &mut ActivitySummary) -> Result<(), FsError> {
        let Some(p): Option<Intent> = self.get("intent.json")? else {
            return Ok(());
        };
        if let Some(mut file) = self.transcript(&self.home.join(&p.transcript))? {
            let end = file.metadata().map_err(error)?.len();
            if end >= p.offset.saturating_add(p.length) {
                file.seek(SeekFrom::Start(p.offset)).map_err(error)?;
                let mut hash = Sha256::new();
                std::io::copy(&mut (&mut file).take(p.length), &mut hash).map_err(error)?;
                if format!("{:x}", hash.finalize()) == p.digest {
                    file.sync_all().map_err(error)?;
                    fs::sync_parent_pinned(&self.home, &p.transcript, Some(&self.pin))?;
                    self.commit_intent(s, &p)?;
                }
            }
        }
        fs::remove_file_pinned(&self.home, &self.dir.join("intent.json"), &self.pin)
    }
}

#[derive(Serialize, Deserialize)]
struct Intent {
    transcript: PathBuf,
    offset: u64,
    length: u64,
    digest: String,
    candidate: Candidate,
}

fn catalog_location(path: &Path) -> Option<(&Path, Uuid)> {
    let projects = path.parent()?.parent()?;
    if projects.file_name()? != "projects" || path.extension()? != "jsonl" {
        return None;
    }
    Some((
        projects.parent()?,
        Uuid::parse_str(path.file_stem()?.to_str()?).ok()?,
    ))
}

/// Guard spans transcript append, authority confirmation and derived publication.
pub(crate) struct ActivityAppend {
    store: Store,
    summary: ActivitySummary,
    intent: Intent,
}

impl ActivityAppend {
    pub(crate) fn begin(
        path: &Path,
        row: &Row,
        bytes: &[u8],
        offset: impl FnOnce() -> Result<u64, FsError>,
    ) -> Result<Option<Self>, FsError> {
        let Some((home, session)) = catalog_location(path) else {
            return Ok(None);
        };
        let Some(candidate) = classify(row, session) else {
            return Ok(None);
        };
        let store = Store::open(home, session)?;
        let summary = store.summary(path)?;
        let intent = Intent {
            candidate,
            offset: offset()?,
            length: bytes.len() as u64,
            digest: digest(bytes),
            transcript: path.strip_prefix(home).map_err(error)?.into(),
        };
        store.put("intent.json", &intent)?;
        Ok(Some(Self {
            store,
            summary,
            intent,
        }))
    }

    pub(crate) async fn begin_host(path: &Path, bytes: &str) -> Result<Option<Self>, FsError> {
        let Some((_, session)) = catalog_location(path) else {
            return Ok(None);
        };
        let Ok(row) = Row::parse(bytes.trim_end_matches('\n')) else {
            return Ok(None);
        };
        if classify(&row, session).is_none() {
            return Ok(None);
        }
        let path = path.to_owned();
        let bytes = bytes.as_bytes().to_vec();
        tokio::task::spawn_blocking(move || {
            Self::begin(&path, &row, &bytes, || match std::fs::metadata(&path) {
                Ok(m) => Ok(m.len()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
                Err(e) => Err(error(e)),
            })
        })
        .await
        .map_err(error)?
    }
    pub(crate) async fn commit_host(mut self) -> Result<(), FsError> {
        tokio::task::spawn_blocking(move || self.store.recover(&mut self.summary))
            .await
            .map_err(error)?
    }
    pub(crate) fn settle_after_append(activity: Option<Self>) {
        let Some(activity) = activity else { return };
        if let Err(error) = activity.commit() {
            // The transcript is already durable. Its admitted intent survives;
            // reporting an append failure here could duplicate the source row.
            tracing::warn!(%error, "Inbox projection will resume from its durable intent");
        }
    }

    fn commit(mut self) -> Result<(), FsError> {
        self.store.commit_intent(&mut self.summary, &self.intent)?;
        fs::remove_file_pinned(
            &self.store.home,
            &self.store.dir.join("intent.json"),
            &self.store.pin,
        )
    }
}

/// Record only names supplied by a user rename command, after its transcript
/// append succeeds. Hooks, inferred names and draft anchors use other writers.
pub(crate) fn record_user_title(path: &Path, title: &str) -> Result<(), FsError> {
    let Some((home, session)) = catalog_location(path) else {
        return Ok(());
    };
    let store = Store::open(home, session)?;
    let mut state = store.summary(path)?;
    let hash = digest(title.as_bytes());
    if state.user_title_digest.as_ref() != Some(&hash) {
        state.user_title_digest = Some(hash);
        state.source_revision += 1;
        store.put("summary.json", &state)?;
    }
    Ok(())
}

impl ActivitySummary {
    /// Redact the local provenance digest before transport and expose a name
    /// only when it is still the actual persisted catalog label.
    pub fn take_user_title(&mut self, current: Option<&str>) -> Option<String> {
        let exact = self.user_title_digest.take()?;
        current
            .filter(|title| digest(title.as_bytes()) == exact)
            .map(str::to_owned)
    }
}

pub fn summary(path: &Path) -> Result<Option<ActivitySummary>, FsError> {
    let Some((home, session)) = catalog_location(path) else {
        return Ok(None);
    };
    Store::open(home, session)?.summary(path).map(Some)
}

pub fn page(path: &Path, after: u64) -> Result<ActivityPage, FsError> {
    let (home, session) = catalog_location(path).ok_or_else(|| error("Invalid session source"))?;
    let store = Store::open(home, session)?;
    let _source = store
        .transcript(path)?
        .ok_or_else(|| error("Session source is unavailable"))?;
    let s = store.summary(path)?;
    let start = after.max(s.baseline_cursor);
    let end = start.saturating_add(PAGE).min(s.cursor);
    let mut items = Vec::new();
    if start < end {
        for ordinal in start + 1..=end {
            items.push(
                store
                    .get(&format!("entries/{ordinal}.json"))?
                    .ok_or_else(|| error("Assistant activity entry is missing"))?,
            );
        }
    }
    Ok(ActivityPage {
        items,
        cursor: s.cursor,
        baseline_cursor: s.baseline_cursor,
        has_more: end < s.cursor,
    })
}

/// A text-only, exact committed source result. The source bytes are loaded
/// solely on explicit preview and never copied into activity or cloud storage.
pub struct PreviewMessage {
    pub uuid: String,
    pub at: i64,
    pub blocks: Vec<Vec<u16>>,
}

pub fn preview(path: &Path, through: u64) -> Result<Vec<PreviewMessage>, FsError> {
    const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
    const MAX_TEXT_UNITS: usize = 32 * 1024;
    let (home, session) = catalog_location(path).ok_or_else(|| error("Invalid session source"))?;
    let store = Store::open(home, session)?;
    let Some(mut file) = store.transcript(path)? else {
        return Ok(Vec::new());
    };
    let s = store.summary(path)?;
    if through <= s.baseline_cursor || through > s.cursor {
        return Ok(Vec::new());
    }
    let Some(entry) = store.get::<ActivityEntry>(&format!("entries/{through}.json"))? else {
        return Ok(Vec::new());
    };
    if entry.result_uuids.is_empty() || entry.result_uuids.len() > MAX_UUIDS {
        return Ok(Vec::new());
    }
    let relative = path.strip_prefix(home).map_err(error)?;
    let mut remaining = MAX_SOURCE_BYTES;
    let mut text_units = 0usize;
    let mut block_count = 0;
    let mut result = Vec::new();
    for id in entry.result_uuids {
        let Some(location) = store.get::<Intent>(&format!("locations/{id}"))? else {
            return Ok(Vec::new());
        };
        if location.transcript != relative || location.length > remaining {
            return Ok(Vec::new());
        }
        remaining -= location.length;
        file.seek(SeekFrom::Start(location.offset)).map_err(error)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take(location.length)
            .read_to_end(&mut bytes)
            .map_err(error)?;
        if bytes.len() as u64 != location.length || digest(&bytes) != location.digest {
            return Ok(Vec::new());
        }
        let row = Row::parse(std::str::from_utf8(&bytes).map_err(error)?.trim()).map_err(error)?;
        let Some(c) = classify(&row, session) else {
            return Ok(Vec::new());
        };
        if c.outer != id || c.kind != entry.kind {
            return Ok(Vec::new());
        }
        let mut blocks = Vec::new();
        if row.string_at("/type") == Some("attachment") {
            // The reminder prompt is model-only. Expose only an explicit,
            // typed result or summary belonging to the delivered task.
            if let Some(units) = row
                .string_units("/attachment/taskDelivery/result")
                .or_else(|| row.string_units("/attachment/taskDelivery/summary"))
            {
                blocks.push(units);
            }
        } else if let Some(units) = row.string_units("/message/content") {
            blocks.push(units);
        } else {
            for i in 0..row.array_len("/message/content").unwrap_or(0) {
                if row.string_at(&format!("/message/content/{i}/type")) == Some("text") {
                    if let Some(units) = row.string_units(&format!("/message/content/{i}/text")) {
                        blocks.push(units);
                    }
                }
            }
        }
        blocks.retain(|units| !units.is_empty());
        text_units += blocks.iter().map(Vec::len).sum::<usize>();
        block_count += blocks.len();
        if blocks.is_empty() || text_units > MAX_TEXT_UNITS || block_count > MAX_UUIDS {
            return Ok(Vec::new());
        }
        result.push(PreviewMessage {
            uuid: id,
            at: c.at,
            blocks,
        });
    }
    Ok(result)
}

/// The host broker determines the kind, independently of message content.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ActionKind {
    Approval,
    Input,
}

#[derive(Serialize, Deserialize)]
struct ParkedAction {
    key: Uuid,
    kind: ActionKind,
}

#[derive(Default, Serialize, Deserialize)]
struct Actions {
    revision: u64,
    at: i64,
    leases: Vec<ParkedAction>,
}

impl Store {
    fn actions(&self) -> Result<Actions, FsError> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Current(Actions),
            Legacy(Vec<Uuid>),
        }
        match self.get::<Stored>("actions.json")? {
            Some(Stored::Current(state)) => Ok(state),
            Some(Stored::Legacy(keys)) => {
                // The previous format retained no action type. An old live
                // broker and the replacement process cannot share its lease.
                Ok(Actions {
                    revision: 0,
                    at: 0,
                    leases: keys
                        .into_iter()
                        .map(|key| ParkedAction {
                            key,
                            kind: ActionKind::Input,
                        })
                        .collect(),
                })
            }
            None => Ok(Actions::default()),
        }
    }

    fn publish_actions(
        &self,
        state: &mut Actions,
        s: &mut ActivitySummary,
        changed: bool,
    ) -> Result<(), FsError> {
        if changed {
            state.revision = state
                .revision
                .max(s.pending_action_revision)
                .checked_add(1)
                .ok_or_else(|| error("Action revision exhausted"))?;
            state.at = jiff::Timestamp::now().as_millisecond();
            self.put("actions.json", state)?;
        }
        let count = u32::try_from(state.leases.len()).map_err(error)?;
        let kind = state
            .leases
            .iter()
            .find(|lease| lease.kind == ActionKind::Approval)
            .or(state.leases.first())
            .map(|lease| lease.kind);
        if (
            s.pending_action_count,
            s.pending_action_kind,
            s.pending_action_revision,
            s.pending_action_at,
        ) != (count, kind, state.revision, state.at)
        {
            s.pending_action_count = count;
            s.pending_action_kind = kind;
            s.pending_action_revision = state.revision;
            s.pending_action_at = state.at;
            s.source_revision += 1;
            self.put("summary.json", s)?;
        }
        Ok(())
    }

    fn refresh_actions(&self, s: &mut ActivitySummary) -> Result<(), FsError> {
        let mut state = self.actions()?;
        let before = state.leases.len();
        let mut live = Vec::with_capacity(before);
        for lease in state.leases.drain(..) {
            let lock = self.dir.join(format!("actions/{}.lock", lease.key));
            let handle = fs::prepare_lock_exclusive_pinned(
                &self.home,
                &lock,
                0o700,
                0o600,
                Some(&self.pin),
            )?;
            match handle.try_acquire()? {
                fs::RootedFileLockAttempt::Contended(_) => live.push(lease),
                fs::RootedFileLockAttempt::Acquired(guard) => {
                    drop(guard);
                    fs::remove_file_pinned(&self.home, &lock, &self.pin)?;
                }
            }
        }
        let changed = before != live.len() || (!live.is_empty() && state.revision == 0);
        state.leases = live;
        self.publish_actions(&mut state, s, changed)
    }
}

/// Parked question/permission lifetime. A dead process releases its kernel
/// lock, so a later catalog reader can retire the stale action after restart.
pub struct ActivityAction {
    path: PathBuf,
    lock: Option<fs::RootedFileLock>,
}

impl ActivityAction {
    pub fn start(path: &Path, kind: ActionKind) -> Result<Option<Self>, FsError> {
        let Some((home, session)) = catalog_location(path) else {
            return Ok(None);
        };
        let store = Store::open(home, session)?;
        let mut s = store.summary(path)?;
        let mut active = store.actions()?;
        let key = Uuid::new_v4();
        let lock = fs::lock_exclusive_pinned(
            home,
            &store.dir.join(format!("actions/{key}.lock")),
            0o700,
            0o600,
            Some(&store.pin),
        )?;
        active.leases.push(ParkedAction { key, kind });
        store.publish_actions(&mut active, &mut s, true)?;
        Ok(Some(Self {
            path: path.into(),
            lock: Some(lock),
        }))
    }
}

impl Drop for ActivityAction {
    fn drop(&mut self) {
        drop(self.lock.take());
        if let Err(e) = summary(&self.path) {
            tracing::warn!(%e, "Could not refresh settled inbox action");
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Terminal {
    #[serde(default)]
    at: i64,
    blocks: Vec<Candidate>,
    failed: bool,
}

/// Publish one logical turn only after the engine has settled stop hooks,
/// tool continuations and budgets. Uncommitted outer rows cannot participate.
pub fn finish_turn(
    path: &Path,
    logical: Option<&str>,
    outer: Option<&str>,
    failed: bool,
) -> Result<(), FsError> {
    let Some((home, session)) = catalog_location(path) else {
        return Ok(());
    };
    let store = Store::open(home, session)?;
    let mut s = store.summary(path)?;
    let mut blocks = None;
    if let Some(logical) = logical {
        let group = digest(format!("assistant/{logical}").as_bytes());
        blocks = store.get::<Vec<Candidate>>(&format!("staged/{group}.json"))?;
    }
    if blocks.is_none() {
        if let Some(id) = outer.and_then(|outer| Uuid::parse_str(outer).ok()) {
            if let Some(group) = store.get::<String>(&format!("staged-rows/{id}"))? {
                blocks = store.get(&format!("staged/{group}.json"))?;
            }
        }
    }
    let blocks = blocks.unwrap_or_default();
    let failed = failed || blocks.last().is_some_and(|row| row.kind == "failure");
    if blocks.is_empty() && failed == s.failed {
        return Ok(());
    }
    store.put(
        "terminal.json",
        &Terminal {
            blocks,
            failed,
            at: jiff::Timestamp::now().as_millisecond(),
        },
    )?;
    store.recover_terminal(&mut s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonl::durable_writer::DurableTranscriptWriter;
    use crate::jsonl::{JsonlMessage, JsonlWriter};
    use std::io::Write;
    use std::sync::Arc;

    struct Fixture {
        home: tempfile::TempDir,
        session: Uuid,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let session = Uuid::new_v4();
            let path = crate::jsonl::session_path(home.path(), "/work", &session.to_string());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            Self {
                home,
                session,
                path,
            }
        }
        fn row(&self, id: Uuid, group: &str, stop: &str) -> serde_json::Value {
            serde_json::json!({"type":"assistant","uuid":id,"sessionId":self.session,"timestamp":"2026-11-16T16:00:00Z","message":{"id":group,"role":"assistant","stop_reason":stop,"content":[{"type":"text","text":"private text"}] }})
        }
        fn append_bytes(&self, bytes: &[u8]) {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .unwrap();
            f.write_all(bytes).unwrap();
            f.sync_all().unwrap();
        }
        fn stage(&self, row: &serde_json::Value) -> (ActivityAppend, Vec<u8>) {
            let line = format!("{row}\n");
            let offset = std::fs::metadata(&self.path)
                .map(|m| m.len())
                .unwrap_or_default();
            let guard = ActivityAppend::begin(
                &self.path,
                &Row::parse(&line).unwrap(),
                line.as_bytes(),
                || Ok(offset),
            )
            .unwrap()
            .unwrap();
            (guard, line.into_bytes())
        }
        fn write(&self, row: &serde_json::Value) {
            let (guard, bytes) = self.stage(row);
            self.append_bytes(&bytes);
            guard.commit().unwrap();
        }
        fn head(&self) -> ActivitySummary {
            summary(&self.path).unwrap().unwrap()
        }
    }

    #[test]
    fn recover_after_append_crash_and_ignore_unwritten_reservation() {
        let f = Fixture::new();
        let old = f.row(Uuid::new_v4(), "past", "end_turn");
        f.append_bytes(format!("{old}\n").as_bytes());
        let result = serde_json::json!({"type":"attachment","uuid":Uuid::new_v4(),"sessionId":f.session,"attachment":{"commandMode":"task-notification","delivery_id":"current"}});
        let (guard, bytes) = f.stage(&result);
        drop(guard);
        assert_eq!((f.head().baseline_cursor, f.head().cursor), (1, 1));
        let (guard, _) = f.stage(&result);
        f.append_bytes(&bytes);
        drop(guard);
        assert_eq!((f.head().baseline_cursor, f.head().cursor), (1, 2));
        f.write(&result);
        assert_eq!(f.head().cursor, 2);
        assert!(!serde_json::to_string(&f.head())
            .unwrap()
            .contains("private text"));
    }

    #[tokio::test]
    async fn real_writer_counts_terminal_blocks_once_after_gc_and_reimport() {
        let f = Fixture::new();
        let old = f.row(Uuid::new_v4(), "before", "end_turn");
        f.append_bytes(format!("{old}\n").as_bytes());
        let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(
            f.home.path().into(),
        ));
        let state = f.home.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let writer = JsonlWriter::new(f.path.clone(), fs)
            .with_durable_lock(Arc::new(DurableTranscriptWriter::open(state).unwrap()));
        writer
            .bootstrap_session_message_identity_snapshot(&f.path)
            .await
            .unwrap();
        let first = Uuid::new_v4();
        let last = Uuid::new_v4();
        let rows = [
            f.row(Uuid::new_v4(), "tool", "tool_use"),
            f.row(first, "after", "end_turn"),
            f.row(last, "after", "end_turn"),
        ];
        let parsed: Vec<JsonlMessage> = rows
            .iter()
            .map(|r| serde_json::from_value(r.clone()).unwrap())
            .collect();
        writer.append_batch(&parsed).await.unwrap();
        assert_eq!(
            f.head().cursor,
            1,
            "terminal hook may still continue with another model call"
        );
        finish_turn(&f.path, Some("after"), None, false).unwrap();
        let head = f.head();
        assert_eq!((head.baseline_cursor, head.cursor), (1, 2));
        assert_eq!(
            head.latest_result_uuids,
            [first.to_string(), last.to_string()]
        );
        let before = head.source_revision;
        // Compact/import replacement does not own or rewind the activity ledger.
        let filler = serde_json::json!({"type":"user","uuid":Uuid::new_v4(),"sessionId":f.session,"message":{"role":"user","content":"x".repeat(crate::jsonl::MIN_COMPACT_FILE_BYTES as usize)}});
        let boundary = serde_json::json!({"type":"system","subtype":"compact_boundary","uuid":Uuid::new_v4(),"sessionId":f.session,"compactMetadata":{},"parentUuid":null});
        f.append_bytes(format!("{filler}\n{boundary}\n").as_bytes());
        assert!(matches!(
            crate::jsonl::perform_compact_transcript(&f.path),
            crate::jsonl::CompactOutcome::Compacted(_)
        ));
        assert!(!std::fs::read_to_string(&f.path)
            .unwrap()
            .contains(&first.to_string()));
        writer.append_batch(&parsed).await.unwrap();
        assert_eq!(f.head().source_revision, before);
        let page = page(&f.path, 0).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].cursor, 2);
    }

    #[test]
    fn baseline_racer_cannot_absorb_admitted_postbarrier_result() {
        let f = Fixture::new();
        let task = serde_json::json!({"type":"attachment","uuid":Uuid::new_v4(),"sessionId":f.session,"attachment":{"commandMode":"task-notification","delivery_id":"task"}});
        let (guard, bytes) = f.stage(&task);
        let path = f.path.clone();
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            start_tx.send(()).unwrap();
            summary(&path).unwrap().unwrap()
        });
        start_rx.recv().unwrap();
        f.append_bytes(&bytes);
        guard.commit().unwrap();
        let s = reader.join().unwrap();
        assert_eq!((s.baseline_cursor, s.cursor), (0, 1));
    }

    #[test]
    fn exact_preview_excludes_tool_and_thinking_content_and_fails_closed_on_rewrite() {
        let f = Fixture::new();
        let id = Uuid::new_v4();
        let mut row = f.row(id, "preview", "end_turn");
        row["message"]["content"] = serde_json::json!([{"type":"thinking","thinking":"private"},{"type":"tool_use","input":{"key":"private"}},{"type":"text","text":"final answer"}]);
        f.write(&row);
        assert!(preview(&f.path, 1).unwrap().is_empty());
        finish_turn(&f.path, Some("preview"), None, false).unwrap();
        let preview_rows = preview(&f.path, 1).unwrap();
        assert_eq!(preview_rows.len(), 1);
        assert_eq!(preview_rows[0].uuid, id.to_string());
        assert_eq!(
            preview_rows[0].blocks,
            ["final answer".encode_utf16().collect::<Vec<_>>()]
        );
        assert!(preview(&f.path, 2).unwrap().is_empty());
        std::fs::write(&f.path, format!("{}\n", f.row(id, "preview", "end_turn"))).unwrap();
        assert!(preview(&f.path, 1).unwrap().is_empty());
    }

    #[test]
    fn first_and_idle_failures_keep_event_time_and_revision_across_recovery() {
        let f = Fixture::new();
        finish_turn(&f.path, None, None, true).unwrap();
        let first = f.head();
        assert!(first.failed && first.failure_at > 0);
        assert_eq!(
            (
                first.failure_revision,
                first.cursor,
                first.last_meaningful_at
            ),
            (1, 0, 0)
        );
        finish_turn(&f.path, None, None, true).unwrap();
        assert_eq!(first, f.head());
        finish_turn(&f.path, None, None, false).unwrap();
        let clear = f.head();
        assert!(!clear.failed);
        assert_eq!(clear.failure_revision, 2);
        let mut old = f.row(Uuid::new_v4(), "reply", "end_turn");
        old["timestamp"] = serde_json::json!("2000-01-01T00:00:00Z");
        f.write(&old);
        finish_turn(&f.path, Some("reply"), None, false).unwrap();
        let idle = f.head();
        assert_eq!((idle.failure_revision, idle.cursor), (2, 1));
        {
            // Simulate loss of the terminal worker after admission, before
            // summary publication. A later reader uses the source timestamp.
            let store = Store::open(f.home.path(), f.session).unwrap();
            store
                .put(
                    "terminal.json",
                    &Terminal {
                        blocks: Vec::new(),
                        failed: true,
                        at: first.failure_at + 1000,
                    },
                )
                .unwrap();
        }
        let recovered = f.head();
        assert_eq!(recovered.failure_at, first.failure_at + 1000);
        assert_eq!(
            (
                recovered.failure_revision,
                recovered.last_meaningful_at,
                recovered.cursor
            ),
            (3, idle.last_meaningful_at, idle.cursor)
        );
        finish_turn(&f.path, None, None, true).unwrap();
        assert_eq!(f.head(), recovered);
    }

    #[test]
    fn only_current_explicit_user_rename_can_leave_the_source() {
        let f = Fixture::new();
        assert!(f.head().take_user_title(Some("AI or hook title")).is_none());
        record_user_title(&f.path, "My project").unwrap();
        let mut head = f.head();
        assert_eq!(
            head.take_user_title(Some("My project")).as_deref(),
            Some("My project")
        );
        assert!(!serde_json::to_string(&head)
            .unwrap()
            .contains("userTitleDigest"));
        assert!(f
            .head()
            .take_user_title(Some("new generated title"))
            .is_none());
        assert_eq!(head.cursor, 0);
    }

    #[test]
    fn distinct_actions_and_failure_state_are_independent_of_result_ordinals() {
        let f = Fixture::new();
        let a = ActivityAction::start(&f.path, ActionKind::Approval)
            .unwrap()
            .unwrap();
        let first = f.head();
        assert_eq!(
            (
                first.pending_action_kind,
                first.pending_action_revision,
                first.last_meaningful_at
            ),
            (Some(ActionKind::Approval), 1, 0)
        );
        assert!(first.pending_action_at > 0);
        let b = ActivityAction::start(&f.path, ActionKind::Input)
            .unwrap()
            .unwrap();
        let s = f.head();
        assert_eq!((s.pending_action_count, s.cursor), (2, 0));
        finish_turn(&f.path, None, None, true).unwrap();
        assert_eq!(f.head().pending_action_revision, s.pending_action_revision);
        assert_eq!(f.head().pending_action_at, s.pending_action_at);
        assert_eq!(s.pending_action_kind, Some(ActionKind::Approval));
        drop(a);
        let next = f.head();
        assert!(next.failed && next.source_revision > s.source_revision);
        assert_eq!((next.pending_action_count, next.cursor), (1, 0));
        assert_eq!(
            (next.pending_action_kind, next.pending_action_revision),
            (Some(ActionKind::Input), 3)
        );
        drop(b);
        assert_eq!(
            (
                f.head().pending_action_count,
                f.head().pending_action_kind,
                f.head().pending_action_revision
            ),
            (0, None, 4)
        );
        let mut old = f.row(Uuid::new_v4(), "old", "end_turn");
        old["timestamp"] = serde_json::json!("2000-01-01T00:00:00Z");
        f.write(&old);
        finish_turn(&f.path, Some("old"), None, false).unwrap();
        assert_eq!(f.head().pending_action_revision, 4);
        let old_time = f.head().last_meaningful_at;
        let _after_idle = ActivityAction::start(&f.path, ActionKind::Input)
            .unwrap()
            .unwrap();
        assert_eq!(f.head().last_meaningful_at, old_time);
        assert!(f.head().pending_action_at > old_time);
        let id = f.head().source_id;
        assert_eq!(source_id(f.home.path()).unwrap(), id);
    }
}
