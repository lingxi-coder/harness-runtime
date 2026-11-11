//! Shared search guards, bounded result collection and cooperative stop
//! controls, alongside binary detection, BOM stripping and UTF-8 decoding.

use once_cell::sync::Lazy;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// JavaScript strings become well-formed UTF-8 at the regex/argv boundary.
/// Keep ordinary input borrowed and repair only unpaired UTF-16 units there;
/// callers must not use this coercion for filesystem authority/path resolution.
pub(crate) fn search_argv_string<'a>(
    input: &'a crate::Utf16JsonProjection,
    field: &str,
) -> Option<std::borrow::Cow<'a, str>> {
    crate::input_string(input, field)
        .map(std::borrow::Cow::Borrowed)
        .or_else(|| {
            input
                .string_units(&format!("/{field}"))
                .map(|units| std::borrow::Cow::Owned(String::from_utf16_lossy(&units)))
        })
}

/// Keep the first `limit` values in the comparator's stable sorted order.
/// Encounter order breaks ties exactly as a stable sort would. A missing
/// limit retains every value, for explicitly unlimited search requests.
pub(crate) struct StableTopK<T> {
    values: BinaryHeap<RankedSearchValue<T>>,
    unlimited: Vec<RankedSearchValue<T>>,
    limit: Option<usize>,
    compare: fn(&T, &T) -> Ordering,
    total: usize,
}

struct RankedSearchValue<T> {
    value: T,
    encounter: usize,
    compare: fn(&T, &T) -> Ordering,
}

impl<T> PartialEq for RankedSearchValue<T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<T> Eq for RankedSearchValue<T> {}

impl<T> PartialOrd for RankedSearchValue<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for RankedSearchValue<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.compare)(&self.value, &other.value).then_with(|| self.encounter.cmp(&other.encounter))
    }
}

impl<T> StableTopK<T> {
    pub(crate) fn new(limit: Option<usize>, compare: fn(&T, &T) -> Ordering) -> Self {
        Self {
            values: BinaryHeap::new(),
            unlimited: Vec::new(),
            limit,
            compare,
            total: 0,
        }
    }

    pub(crate) fn push(&mut self, value: T) {
        let value = RankedSearchValue {
            value,
            encounter: self.total,
            compare: self.compare,
        };
        self.total += 1;
        match self.limit {
            None => self.unlimited.push(value),
            Some(limit) if self.values.len() < limit => self.values.push(value),
            Some(_) => {
                if let Some(mut worst) = self.values.peek_mut() {
                    if value < *worst {
                        *worst = value;
                    }
                }
            }
        }
    }

    pub(crate) fn total(&self) -> usize {
        self.total
    }

    pub(crate) fn into_sorted(self) -> Vec<T> {
        let values = if self.limit.is_some() {
            self.values.into_sorted_vec()
        } else {
            let mut values = self.unlimited;
            values.sort_unstable();
            values
        };
        values.into_iter().map(|value| value.value).collect()
    }

    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.values.len() + self.unlimited.len()
    }

    #[cfg(test)]
    fn retained_capacity(&self) -> usize {
        self.values.capacity() + self.unlimited.capacity()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchStop {
    Cancelled,
    TimedOut,
}

/// Shared by the blocking walker, reader and sink. Dropping the owning async
/// call signals the same stop path as explicit context cancellation.
#[derive(Clone)]
pub(crate) struct SearchControl {
    deadline: Instant,
    cancel: Option<CancellationToken>,
    owner_dropped: Arc<AtomicBool>,
    #[cfg(test)]
    test_stop: Option<Arc<dyn Fn() -> Option<SearchStop> + Send + Sync>>,
}

pub(crate) struct SearchOwner(Arc<AtomicBool>);

impl Drop for SearchOwner {
    fn drop(&mut self) {
        self.0.store(true, AtomicOrdering::Release);
    }
}

impl SearchControl {
    pub(crate) fn new(deadline: Instant, cancel: Option<CancellationToken>) -> (Self, SearchOwner) {
        let owner_dropped = Arc::new(AtomicBool::new(false));
        (
            Self {
                deadline,
                cancel,
                owner_dropped: owner_dropped.clone(),
                #[cfg(test)]
                test_stop: None,
            },
            SearchOwner(owner_dropped),
        )
    }

    pub(crate) fn check_cancellation(&self) -> Result<(), SearchStop> {
        if self.owner_dropped.load(AtomicOrdering::Acquire)
            || self
                .cancel
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(SearchStop::Cancelled);
        }
        Ok(())
    }

    pub(crate) fn check(&self) -> Result<(), SearchStop> {
        self.check_cancellation()?;
        #[cfg(test)]
        if let Some(reason) = self.test_stop.as_ref().and_then(|probe| probe()) {
            return Err(reason);
        }
        if Instant::now() >= self.deadline {
            return Err(SearchStop::TimedOut);
        }
        Ok(())
    }

    pub(crate) fn check_io(&self) -> io::Result<()> {
        self.check().map_err(|reason| {
            // Interrupted is retried by read_to_end and decoders, so use a
            // terminal reader error and inspect the shared stop reason instead.
            io::Error::other(match reason {
                SearchStop::Cancelled => "search cancelled",
                SearchStop::TimedOut => "search timed out",
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_stop(
        mut self,
        probe: impl Fn() -> Option<SearchStop> + Send + Sync + 'static,
    ) -> Self {
        self.test_stop = Some(Arc::new(probe));
        self
    }
}

/// Share the blocking-file search budget across Glob and Grep. The permit is
/// moved into the actual worker, so dropping an async waiter cannot admit
/// replacement work while its previous worker is still unwinding.
const MAX_BLOCKING_SEARCH_JOBS: usize = 4;
static BLOCKING_SEARCH_JOBS: Lazy<Arc<Semaphore>> =
    Lazy::new(|| Arc::new(Semaphore::new(MAX_BLOCKING_SEARCH_JOBS)));

pub(crate) async fn acquire_search_job(
    control: &SearchControl,
) -> Result<OwnedSemaphorePermit, SearchStop> {
    acquire_search_job_from(control, BLOCKING_SEARCH_JOBS.clone()).await
}

async fn acquire_search_job_from(
    control: &SearchControl,
    jobs: Arc<Semaphore>,
) -> Result<OwnedSemaphorePermit, SearchStop> {
    control.check()?;
    let cancelled = async {
        if let Some(cancel) = &control.cancel {
            cancel.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let permit = tokio::select! {
        biased;
        _ = cancelled => return Err(SearchStop::Cancelled),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(control.deadline)) => return Err(SearchStop::TimedOut),
        permit = jobs.acquire_owned() => permit.map_err(|_| SearchStop::Cancelled)?,
    };
    control.check()?;
    Ok(permit)
}

/// Bound even read_to_end's growing requests to one check per 64 KiB. A
/// blocked OS read itself cannot be interrupted by this cooperative adapter.
pub(crate) struct SearchReader<'a, R> {
    pub(crate) inner: &'a mut R,
    pub(crate) control: &'a SearchControl,
}

impl<R: Read> Read for SearchReader<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.control.check_io()?;
        let length = output.len().min(64 * 1024);
        let read = self.inner.read(&mut output[..length])?;
        self.control.check_io()?;
        Ok(read)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SearchWalkError {
    Engine(String),
    InvalidInput(String),
    Resolution(SearchResolutionError),
    Cancelled,
}

impl From<SearchResolutionError> for SearchWalkError {
    fn from(error: SearchResolutionError) -> Self {
        Self::Resolution(error)
    }
}

pub(crate) struct ReadDenyPolicy {
    snapshot: Arc<dyn lingxi_core::host::permission_gate::ReadPathPolicySnapshot>,
    requested_root: PathBuf,
    approved_root: PathBuf,
}

/// Keep exact Read checks alongside coarse glob exclusions. Both the walk's
/// canonical path and the caller's lexical spelling must pass the snapshot.
pub(crate) fn prepare_search_read_policy(
    ctx: &tool_api::BuiltinToolContext,
    requested_root: &Path,
    approved_root: &Path,
) -> (Vec<String>, Option<ReadDenyPolicy>) {
    let (exclude_globs, snapshot) = ctx
        .permission_gate
        .as_ref()
        .and_then(|gate| gate.prepare_read_search_policy(approved_root))
        .map(|prepared| (prepared.exclude_globs, prepared.path_policy))
        .unwrap_or_else(|| {
            (
                ctx.read_deny_exclude_globs.clone(),
                permission::read_deny_globs::read_deny_search_snapshot(
                    ctx.permission_policy.clone(),
                    Some(approved_root),
                ),
            )
        });
    let policy = snapshot
        .facts()
        .read_deny_rules_active
        .then_some(ReadDenyPolicy {
            snapshot,
            requested_root: requested_root.into(),
            approved_root: approved_root.into(),
        });
    (exclude_globs, policy)
}

pub(crate) fn search_path_is_read_denied(policy: &Option<ReadDenyPolicy>, path: &Path) -> bool {
    use lingxi_core::host::permission_gate::ReadPathPolicyMatch;
    policy.as_ref().is_some_and(|policy| {
        policy.snapshot.denied_search_path(path) != ReadPathPolicyMatch::NoMatch
            || (policy.requested_root != policy.approved_root
                && path
                    .strip_prefix(&policy.approved_root)
                    .ok()
                    .is_some_and(|relative| {
                        policy
                            .snapshot
                            .denied_search_path(&policy.requested_root.join(relative))
                            != ReadPathPolicyMatch::NoMatch
                    }))
    })
}

/// Search preparation failures that must be surfaced before a walk can expose
/// any result.  The search tools first resolve their root and install their
/// Read-deny overrides; this snapshot lets them confirm that neither changed
/// before (and while) the walk runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchResolutionError {
    /// The model-supplied search root no longer resolves to the approved path.
    SearchRootChanged,
    /// A path used by one of the active Read-deny rules no longer resolves to
    /// the path observed while the search was being prepared.
    ReadDenyPathChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PathResolution {
    resolved: Option<PathBuf>,
    parent: Option<PathBuf>,
}

/// Resolution state captured after permission canonicalization and deny-glob
/// construction.  The approved root itself is used for the walk, while the
/// original spelling is retained for the TOCTOU re-check.
#[derive(Debug, Clone)]
pub(crate) struct SearchResolutionSnapshot {
    requested_root: PathBuf,
    approved_root: PathBuf,
    root_state: PathResolution,
    deny_paths: Vec<(PathBuf, PathResolution)>,
}

impl SearchResolutionSnapshot {
    /// Capture the root and the static path prefixes represented by deny globs.
    /// A glob's wildcard suffix cannot be canonicalized, so its longest static
    /// prefix is the path whose symlink chain can be checked deterministically.
    pub(crate) fn capture(
        requested_root: &Path,
        approved_root: &Path,
        deny_globs: &[String],
    ) -> Self {
        let deny_paths = deny_globs
            .iter()
            .filter_map(|glob| static_deny_path(approved_root, glob))
            .map(|path| {
                let state = path_resolution(&path);
                (path, state)
            })
            .collect();
        Self {
            requested_root: requested_root.to_path_buf(),
            approved_root: approved_root.to_path_buf(),
            root_state: path_resolution(requested_root),
            deny_paths,
        }
    }

    /// Re-check the original root and each deny-rule path.  A changed root is
    /// checked first because it determines the entire search tree; deny paths
    /// are checked next so no search result from a changed protected path is
    /// returned.
    pub(crate) fn verify(&self) -> Result<(), SearchResolutionError> {
        let current_root = path_resolution(&self.requested_root);
        if current_root != self.root_state
            || current_root.resolved.as_deref() != Some(self.approved_root.as_path())
        {
            return Err(SearchResolutionError::SearchRootChanged);
        }
        for (path, approved) in &self.deny_paths {
            if path_resolution(path) != *approved {
                return Err(SearchResolutionError::ReadDenyPathChanged);
            }
        }
        Ok(())
    }
}

fn path_resolution(path: &Path) -> PathResolution {
    let resolved = std::fs::canonicalize(path).ok();
    let parent = path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok());
    PathResolution { resolved, parent }
}

/// Return the longest non-wildcard path represented by a deny glob.  Rooted
/// entries are relative to the canonical search root (the same anchor used by
/// `ignore::OverrideBuilder`); unrooted entries use that root as their stable
/// check point as well.  If a pattern begins with a wildcard there is no single
/// path to pin, and the search root re-check remains the applicable guard.
fn static_deny_path(search_root: &Path, glob: &str) -> Option<PathBuf> {
    let without_root = glob.strip_prefix('/').unwrap_or(glob);
    let wildcard = without_root.find(['*', '?', '[', '{']);
    let static_prefix = match wildcard {
        Some(index) => &without_root[..index],
        None => without_root,
    }
    .trim_end_matches('/');
    if static_prefix.is_empty() {
        return None;
    }
    let path = search_root.join(static_prefix);
    if path == search_root {
        None
    } else {
        Some(path)
    }
}

#[cfg(test)]
mod search_test_hook {
    use once_cell::sync::Lazy;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static HOOKS: Lazy<Mutex<Vec<(PathBuf, Box<dyn FnOnce() + Send + 'static>)>>> =
        Lazy::new(|| Mutex::new(Vec::new()));
    static CANDIDATE_HOOKS: Lazy<Mutex<Vec<(PathBuf, Box<dyn FnOnce() + Send + 'static>)>>> =
        Lazy::new(|| Mutex::new(Vec::new()));

    pub(crate) fn install(path: &Path, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = HOOKS.lock().expect("search hook mutex poisoned");
        hooks.retain(|(registered_path, _)| registered_path != path);
        hooks.push((path.to_path_buf(), Box::new(hook)));
    }

    pub(crate) fn run(path: &Path) {
        let mut hooks = HOOKS.lock().expect("search hook mutex poisoned");
        let hook = hooks
            .iter()
            .position(|(registered_path, _)| registered_path == path)
            .map(|index| hooks.remove(index).1);
        drop(hooks);
        if let Some(hook) = hook {
            hook();
        }
    }

    pub(crate) fn install_candidate(path: &Path, hook: impl FnOnce() + Send + 'static) {
        let mut hooks = CANDIDATE_HOOKS
            .lock()
            .expect("search candidate hook mutex poisoned");
        hooks.retain(|(registered_path, _)| registered_path != path);
        hooks.push((path.to_path_buf(), Box::new(hook)));
    }

    pub(crate) fn run_candidate(path: &Path) {
        let mut hooks = CANDIDATE_HOOKS
            .lock()
            .expect("search candidate hook mutex poisoned");
        let hook = hooks
            .iter()
            .position(|(registered_path, _)| registered_path == path)
            .map(|index| hooks.remove(index).1);
        drop(hooks);
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Deterministic test seam for the gap between search preparation and the
/// first filesystem walk. Production builds compile this to a no-op.
#[cfg(test)]
pub(crate) fn run_search_preparation_hook(path: &Path) {
    search_test_hook::run(path);
}

#[cfg(not(test))]
pub(crate) fn run_search_preparation_hook(_path: &Path) {}

#[cfg(test)]
pub(crate) fn install_search_preparation_hook(path: &Path, hook: impl FnOnce() + Send + 'static) {
    search_test_hook::install(path, hook);
}

/// Deterministic test seam for the gap after a walk entry is classified as a
/// regular file and before its stable rooted handle is opened.
#[cfg(test)]
pub(crate) fn run_search_candidate_hook(path: &Path) {
    search_test_hook::run_candidate(path);
}

#[cfg(not(test))]
pub(crate) fn run_search_candidate_hook(_path: &Path) {}

#[cfg(test)]
pub(crate) fn install_search_candidate_hook(path: &Path, hook: impl FnOnce() + Send + 'static) {
    search_test_hook::install_candidate(path, hook);
}

/// Open a walked candidate through a rooted, no-follow handle chain and verify
/// that its resolved path is still exactly the path yielded under the search
/// root. Any candidate drift fails the whole search closed, discarding partial
/// matches rather than exposing data from a swapped leaf or ancestor.
pub(crate) fn open_rooted_search_file(
    search_root: &Path,
    candidate: &Path,
) -> Result<std::fs::File, SearchResolutionError> {
    if !candidate.starts_with(search_root) {
        return Err(SearchResolutionError::SearchRootChanged);
    }
    // A direct-file Grep has no relative components below its search root.
    // Pin its parent directory and open the approved leaf without following it.
    if candidate == search_root {
        let parent = candidate
            .parent()
            .ok_or(SearchResolutionError::SearchRootChanged)?;
        let filename = candidate
            .file_name()
            .ok_or(SearchResolutionError::SearchRootChanged)?;
        return lingxi_core::host::rooted_fs::open_file_after_permission(
            parent,
            Path::new(filename),
            candidate,
            candidate,
        )
        .map_err(|_| SearchResolutionError::SearchRootChanged);
    }
    let relative = candidate
        .strip_prefix(search_root)
        .map_err(|_| SearchResolutionError::SearchRootChanged)?;
    lingxi_core::host::rooted_fs::open_file_after_permission(
        search_root,
        relative,
        candidate,
        candidate,
    )
    .map_err(|_| SearchResolutionError::SearchRootChanged)
}

/// Map an approved canonical path back to a canonical trusted root and a
/// root-relative path. Rooted I/O uses this pair to open fixed directory
/// handles instead of reopening the canonical pathname. The longest matching
/// root wins when a session has nested trusted directories.
pub(crate) fn rooted_location(
    approved: &Path,
    trusted_dirs: &[PathBuf],
) -> Option<(PathBuf, PathBuf)> {
    trusted_dirs
        .iter()
        .filter_map(|root| {
            let root = std::fs::canonicalize(root).ok()?;
            if !approved.starts_with(&root) {
                return None;
            }
            let relative = approved.strip_prefix(&root).ok()?.to_path_buf();
            Some((root, relative))
        })
        .max_by_key(|(root, _)| root.components().count())
}

/// Native 2.1.322's decimal 8 KB window for the first-pass NUL scan.
pub const NUL_SCAN_WINDOW: usize = 8000;

/// True iff `prefix` (capped at [`NUL_SCAN_WINDOW`]) contains a NUL byte.
#[must_use]
pub fn looks_binary(prefix: &[u8]) -> bool {
    let window = &prefix[..prefix.len().min(NUL_SCAN_WINDOW)];
    window.contains(&0)
}

/// Return `bytes` with the UTF-8 BOM (`EF BB BF`) stripped if present.
#[must_use]
pub fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    if bytes.starts_with(b"\xEF\xBB\xBF") {
        &bytes[3..]
    } else {
        bytes
    }
}

/// Decode `bytes` as UTF-8, stripping the BOM if present. Rejects non-UTF-8
/// (no fallback to latin-1 / cp1252 in M4-01 per spec §7 "Default encoding:
/// UTF-8 (BOM-aware); reject non-UTF-8").
///
/// # Errors
/// Returns `Utf8Error` if the bytes are not valid UTF-8.
pub fn decode_utf8_strict(bytes: &[u8]) -> Result<String, std::str::Utf8Error> {
    let trimmed = strip_utf8_bom(bytes);
    std::str::from_utf8(trimmed).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_optimizer_top_k_matches_stable_sort_and_bounds_retention() {
        use std::sync::atomic::AtomicUsize;
        static COMPARISONS: AtomicUsize = AtomicUsize::new(0);
        fn compare(a: &(usize, usize), b: &(usize, usize)) -> Ordering {
            COMPARISONS.fetch_add(1, AtomicOrdering::Relaxed);
            a.0.cmp(&b.0)
        }
        let count = 50_000;
        let mut top = StableTopK::new(Some(100), compare);
        let mut reference = Vec::new();
        let mut peak_capacity = 0;
        for encounter in 0..count {
            let value = ((count - encounter) / 7, encounter);
            reference.push(value);
            top.push(value);
            assert!(top.retained_len() <= 100);
            peak_capacity = peak_capacity.max(top.retained_capacity());
        }
        assert_eq!(top.total(), count);
        let comparisons = COMPARISONS.load(AtomicOrdering::Relaxed);
        assert!(comparisons < count * 30, "bounded heap comparison budget");
        reference.sort_by_key(|value| value.0);
        reference.truncate(100);
        assert_eq!(top.into_sorted(), reference);
        println!("search_top_k_probe: candidates={count} retained=100 capacity={peak_capacity} comparisons={comparisons}");
    }

    #[test]
    fn search_optimizer_top_k_unlimited_and_zero() {
        let mut all = StableTopK::new(None, usize::cmp);
        let mut zero = StableTopK::new(Some(0), usize::cmp);
        for value in [4, 1, 4, 2] {
            all.push(value);
            zero.push(value);
        }
        assert_eq!(all.into_sorted(), vec![1, 2, 4, 4]);
        assert_eq!(zero.total(), 4);
        assert!(zero.into_sorted().is_empty());
    }

    #[test]
    fn search_optimizer_owner_drop_and_context_cancel_are_terminal() {
        let token = CancellationToken::new();
        let (control, owner) = SearchControl::new(
            Instant::now() + std::time::Duration::from_secs(60),
            Some(token.clone()),
        );
        assert_eq!(control.check(), Ok(()));
        token.cancel();
        assert_eq!(control.check(), Err(SearchStop::Cancelled));
        drop(owner);
        let (control, owner) =
            SearchControl::new(Instant::now() + std::time::Duration::from_secs(60), None);
        drop(owner);
        assert_eq!(control.check(), Err(SearchStop::Cancelled));
    }

    #[tokio::test]
    async fn search_optimizer_dropping_async_owner_stops_detached_blocking_work() {
        let (control, owner) =
            SearchControl::new(Instant::now() + std::time::Duration::from_secs(60), None);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let call = tokio::spawn(async move {
            let _owner = owner;
            tokio::task::spawn_blocking(move || {
                entered_tx.send(()).unwrap();
                released_rx.recv().unwrap();
                result_tx.send(control.check()).unwrap();
            })
            .await
            .unwrap();
        });
        entered_rx.await.unwrap();
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        released_tx.send(()).unwrap();
        assert_eq!(result_rx.await.unwrap(), Err(SearchStop::Cancelled));
    }

    #[test]
    fn search_optimizer_reader_checks_each_bounded_chunk_and_after_io() {
        struct ProbeReader {
            token: CancellationToken,
            calls: usize,
        }
        impl Read for ProbeReader {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                assert!(output.len() <= 64 * 1024);
                output.fill(b'a');
                self.token.cancel();
                Ok(output.len())
            }
        }
        let token = CancellationToken::new();
        let (control, _owner) = SearchControl::new(
            Instant::now() + std::time::Duration::from_secs(60),
            Some(token.clone()),
        );
        let mut input = ProbeReader { token, calls: 0 };
        let error = SearchReader {
            inner: &mut input,
            control: &control,
        }
        .read(&mut vec![0; 1024 * 1024])
        .unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(input.calls, 1);
        assert_eq!(control.check(), Err(SearchStop::Cancelled));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_job_admission_preserves_queued_deadline_and_cancellation() {
        let jobs = Arc::new(Semaphore::new(MAX_BLOCKING_SEARCH_JOBS));
        let mut held = Vec::new();
        for _ in 0..MAX_BLOCKING_SEARCH_JOBS {
            held.push(jobs.clone().acquire_owned().await.unwrap());
        }
        let (control, _owner) =
            SearchControl::new(Instant::now() + std::time::Duration::from_millis(20), None);
        let heartbeat = async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            1
        };
        let (waited, beat) =
            tokio::join!(acquire_search_job_from(&control, jobs.clone()), heartbeat);
        assert!(matches!(waited, Err(SearchStop::TimedOut)));
        assert_eq!(beat, 1);
        let token = CancellationToken::new();
        let (control, _owner) = SearchControl::new(
            Instant::now() + std::time::Duration::from_secs(30),
            Some(token.clone()),
        );
        let cancel = async {
            tokio::task::yield_now().await;
            token.cancel();
        };
        let (waited, ()) = tokio::join!(acquire_search_job_from(&control, jobs.clone()), cancel);
        assert!(matches!(waited, Err(SearchStop::Cancelled)));
        assert_eq!(jobs.available_permits(), 0);
        drop(held);
        assert_eq!(jobs.available_permits(), MAX_BLOCKING_SEARCH_JOBS);
    }

    #[test]
    fn search_optimizer_reader_deadline_stops_read_to_end_without_retrying() {
        use std::sync::atomic::AtomicUsize;
        let checks = Arc::new(AtomicUsize::new(0));
        let (control, _owner) =
            SearchControl::new(Instant::now() + std::time::Duration::from_secs(60), None);
        let control = control.with_test_stop(move || {
            (checks.fetch_add(1, AtomicOrdering::Relaxed) >= 3).then_some(SearchStop::TimedOut)
        });
        let mut input = io::repeat(b'a');
        let mut output = Vec::new();
        let error = SearchReader {
            inner: &mut input,
            control: &control,
        }
        .read_to_end(&mut output)
        .unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(control.check(), Err(SearchStop::TimedOut));
        assert!(output.len() <= 64 * 1024);
        let (expired, _owner) = SearchControl::new(Instant::now(), None);
        assert_eq!(expired.check(), Err(SearchStop::TimedOut));
    }

    #[test]
    fn nul_scan_window_is_decimal_8kb() {
        assert_eq!(NUL_SCAN_WINDOW, 8000);
    }

    #[test]
    fn looks_binary_finds_nul_in_window() {
        let mut buf = vec![b'A'; NUL_SCAN_WINDOW];
        buf[100] = 0;
        assert!(looks_binary(&buf));
    }

    #[test]
    fn looks_binary_ignores_nul_outside_window() {
        let mut buf = vec![b'A'; NUL_SCAN_WINDOW + 1];
        buf[NUL_SCAN_WINDOW] = 0;
        assert!(!looks_binary(&buf));
    }

    #[test]
    fn looks_binary_empty_input_is_text() {
        assert!(!looks_binary(b""));
    }

    #[test]
    fn looks_binary_pure_text_is_text() {
        assert!(!looks_binary(b"hello world\nline 2\n"));
    }

    #[test]
    fn strip_utf8_bom_removes_bom() {
        let with_bom = b"\xEF\xBB\xBFhello";
        assert_eq!(strip_utf8_bom(with_bom), b"hello");
    }

    #[test]
    fn strip_utf8_bom_preserves_non_bom() {
        let no_bom = b"hello";
        assert_eq!(strip_utf8_bom(no_bom), b"hello");
    }

    #[test]
    fn decode_utf8_strict_handles_bom() {
        let s = decode_utf8_strict(b"\xEF\xBB\xBFhello \xE4\xB8\x96\xE7\x95\x8C").unwrap();
        assert_eq!(s, "hello 世界");
    }

    #[test]
    fn decode_utf8_strict_rejects_invalid() {
        // 0xFF is never a valid UTF-8 starter
        assert!(decode_utf8_strict(&[0xFF, 0xFE, b'a']).is_err());
    }
}

#[cfg(test)]
pub(crate) struct TaskOutputTestRegistry(pub std::path::PathBuf);

#[cfg(test)]
#[async_trait::async_trait]
impl lingxi_core::host::task_registry::TaskRegistryHandle for TaskOutputTestRegistry {
    async fn task_output_directory(&self) -> Option<String> {
        Some(self.0.to_string_lossy().into_owned())
    }
    async fn create(
        &self,
        _: lingxi_core::host::task_registry::TaskCreateInput,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn get(
        &self,
        _: &str,
    ) -> Result<
        Option<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(None)
    }
    async fn list(
        &self,
        _: lingxi_core::host::task_registry::TaskListFilter,
    ) -> Result<
        Vec<lingxi_core::host::task_registry::TaskRecord>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        Ok(vec![])
    }
    async fn update(
        &self,
        _: &str,
        _: lingxi_core::host::task_registry::TaskUpdatePatch,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn set_status(
        &self,
        _: &str,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn kill(
        &self,
        _: &str,
    ) -> Result<
        lingxi_core::host::task_registry::TaskRecord,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
    async fn output(
        &self,
        _: &str,
        _: Option<u64>,
    ) -> Result<
        lingxi_core::host::task_registry::TaskOutputChunk,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        unreachable!()
    }
}
