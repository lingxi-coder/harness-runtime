//! Native local managed-policy helper execution.
//!
//! Source: Claude Code 2.1.295, next-turn-source/src_188306029.js,
//! SHA-256 93e1595fd1989cb42b1fc443504ba0b7bc426ab6724265d7579b9fdccf5c959d.
//! The process runner is a dedicated administrative boundary and does not
//! use ordinary tool permissions or the user-command sandbox.

use async_trait::async_trait;
use lingxi_core::settings::native_policy::{
    collect_native_policy_semantic_inputs, collect_native_policy_semantic_inputs_with_js,
    collect_native_policy_static_fallback_semantic_inputs,
    collect_native_policy_static_fallback_semantic_inputs_with_js,
    parse_native_policy_helper_output, revalidate_native_policy_static_fallbacks,
    NativePolicyHelperApplyStatus, NativePolicyHelperEntry, NativePolicyHelperOnFailure,
    NativePolicyHelperOutput, NativePolicyHelperPlanValidation, NativePolicyHelperSelectedKind,
    NativePolicyHelperSettingsInput, NativePolicyPlatform, NativePolicySelection,
    NativePolicySemanticEvidence, NativePolicySemanticInputs, NativePolicySource,
    NativePolicyStartupFatal, NativePolicyStaticFallbackValidation,
};
use lingxi_core::settings::native_policy_js_semantics::NativePolicyJsSemanticsRegistry;
use lingxi_core::settings::native_policy_regex::NativePolicyRegexWorkerError;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OUTPUT_LIMIT: usize = 1_048_576;
const PROCESS_BUFFER_LIMIT: usize = OUTPUT_LIMIT + 1;
const HELPER_VERSION: &str = "2.1.295";
const POWERSHELL_MODULE_PATH_ENV: &str = "CLAUDE_CODE_POLICY_HELPER_PSMODULEPATH";
const POWERSHELL_SCRIPT_PATH_ENV: &str = "CLAUDE_CODE_POLICY_HELPER_PS1_PATH";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Interpreter {
    Sh,
    Pwsh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Invocation {
    pub file: Vec<u16>,
    pub args: Vec<Vec<u16>>,
    pub stdin: Option<Vec<u16>>,
    pub cwd: Vec<u16>,
    pub env: BTreeMap<Vec<u16>, Vec<u16>>,
    pub timeout: Duration,
    pub total_timeout: Duration,
    pub routed_interpreter: Option<Interpreter>,
    pub ps1_path: Option<Vec<u16>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureCode {
    BadPath,
    InterpreterUnavailable,
    NotAFile,
    ExitNonzero,
    TimedOut,
    Oversize,
    ParseFailed,
    EnvelopeInvalid,
    SchemaRejected,
}

impl FailureCode {
    fn retryable(self) -> bool {
        matches!(
            self,
            Self::InterpreterUnavailable | Self::NotAFile | Self::ExitNonzero | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProcessFailure {
    pub code: FailureCode,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExecutionFailure {
    pub code: FailureCode,
    pub diagnostic: Option<Utf16JsonProjection>,
    pub field_path: Option<String>,
    pub attempts: u8,
}

#[async_trait]
pub(super) trait ProcessRunner: Send + Sync {
    async fn run(&self, invocation: Invocation) -> Result<ProcessOutput, ProcessFailure>;
}

/// Host-owned sink for source-exact Native policy-helper warning messages.
/// The Runtime preserves UTF-16 until the embedding host's actual output sink.
pub trait NativePolicyWarningLogger: Send + Sync {
    fn warning_enabled(&self) -> bool;
    fn filter_source_units(&self) -> Option<Vec<u16>>;
    fn write_rendered_warning(&self, line_units: &[u16]);
}

type SemanticWorkerState = Arc<
    std::sync::Mutex<
        Option<
            Result<
                Arc<crate::bun_policy_js_semantics_worker::BunPolicyJsSemanticsWorker>,
                NativePolicyRegexWorkerError,
            >,
        >,
    >,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum InitialPolicyHelperError {
    Native(PolicyHelperStartupError),
    SemanticWorker(NativePolicyRegexWorkerError),
}

impl From<PolicyHelperStartupError> for InitialPolicyHelperError {
    fn from(error: PolicyHelperStartupError) -> Self {
        Self::Native(error)
    }
}

/// A Bun semantic-worker capability failure retained independently from a
/// Native helper refusal. It means the host could not validate a required
/// source operation; it is never reported as an invalid Native pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicySemanticWorkerFailure {
    source: NativePolicySource,
    source_paths: Vec<std::path::PathBuf>,
    generation: u64,
    error: NativePolicyRegexWorkerError,
}

impl NativePolicySemanticWorkerFailure {
    #[must_use]
    pub fn source(&self) -> NativePolicySource {
        self.source
    }

    #[must_use]
    pub fn source_paths(&self) -> &[std::path::PathBuf] {
        &self.source_paths
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn worker_error(&self) -> &NativePolicyRegexWorkerError {
        &self.error
    }
}

impl std::fmt::Display for NativePolicySemanticWorkerFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Native policy semantic worker failed: {}",
            self.error
        )
    }
}

impl std::error::Error for NativePolicySemanticWorkerFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Cloneable read-only observation of the current source-scoped semantic
/// worker failure. A successful later validation or a new source generation
/// clears the retained value.
#[derive(Clone)]
pub struct PolicySemanticWorkerProbe {
    receiver: tokio::sync::watch::Receiver<Option<Arc<NativePolicySemanticWorkerFailure>>>,
}

impl PolicySemanticWorkerProbe {
    #[must_use]
    pub fn current(&self) -> Option<Arc<NativePolicySemanticWorkerFailure>> {
        self.receiver.borrow().clone()
    }

    pub async fn wait(&mut self) -> Arc<NativePolicySemanticWorkerFailure> {
        loop {
            if let Some(error) = self.current() {
                return error;
            }
            if self.receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

fn semantic_worker(
    state: &SemanticWorkerState,
) -> Result<
    Arc<crate::bun_policy_js_semantics_worker::BunPolicyJsSemanticsWorker>,
    NativePolicyRegexWorkerError,
> {
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .get_or_insert_with(|| {
            crate::bun_policy_js_semantics_worker::BunPolicyJsSemanticsWorker::new_from_host_environment()
                .map(Arc::new)
        })
        .clone()
}

async fn emit_native_policy_warning_messages(
    state: &SemanticWorkerState,
    logger: &dyn NativePolicyWarningLogger,
    messages: &[Utf16JsonProjection],
) {
    if !logger.warning_enabled() || messages.is_empty() {
        return;
    }
    let Some(message_units) = messages
        .iter()
        .map(|message| message.string_units(""))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let Ok(worker) = semantic_worker(state) else {
        return;
    };
    let filter_units = logger.filter_source_units();
    let Ok(rendered_lines) = worker
        .render_policy_warnings(&message_units, filter_units.as_deref())
        .await
    else {
        // Never emit an unredacted Native warning if the pinned formatter is
        // unavailable. The source warning rows remain in the managed snapshot.
        return;
    };
    for line_units in rendered_lines.into_iter().flatten() {
        logger.write_rendered_warning(&line_units);
    }
}

fn empty_semantic_evidence() -> NativePolicySemanticEvidence {
    NativePolicySemanticEvidence {
        regex_rows: Vec::new(),
        js: NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
            .expect("empty semantic input set is valid"),
    }
}

async fn semantic_evidence_for_inputs(
    state: &SemanticWorkerState,
    inputs: &NativePolicySemanticInputs,
) -> Result<NativePolicySemanticEvidence, NativePolicyRegexWorkerError> {
    if inputs.regex_patterns.is_empty()
        && inputs.url_inputs.is_empty()
        && inputs.display_inputs.is_empty()
    {
        return Ok(empty_semantic_evidence());
    }
    let worker = semantic_worker(state)?;
    let regex_rows = if inputs.regex_patterns.is_empty() {
        Vec::new()
    } else {
        worker.validate_patterns(&inputs.regex_patterns).await?
    };
    let js = if inputs.url_inputs.is_empty() && inputs.display_inputs.is_empty() {
        NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
            .expect("no URL/display input set is valid")
    } else {
        worker
            .observe_semantics(&inputs.url_inputs, &inputs.display_inputs)
            .await?
    };
    let evidence = NativePolicySemanticEvidence { regex_rows, js };
    if !evidence.covers(inputs) {
        return Err(NativePolicyRegexWorkerError::Protocol {
            message: "Bun worker omitted a requested Native policy semantic row".to_owned(),
        });
    }
    Ok(evidence)
}

async fn semantic_evidence_for_projection(
    state: &SemanticWorkerState,
    source: &Utf16JsonProjection,
) -> Result<NativePolicySemanticEvidence, NativePolicyRegexWorkerError> {
    // First obtain all unconditional URL/RegExp/display rows. Some Native
    // paths only reach a nested URL after inspecting one of these rows (for
    // example, managed MCP OAuth metadata after a valid HTTPS server URL).
    let first_inputs = collect_native_policy_semantic_inputs(source);
    let first_evidence = semantic_evidence_for_inputs(state, &first_inputs).await?;
    let reached_inputs =
        collect_native_policy_semantic_inputs_with_js(source, Some(&first_evidence.js));
    if reached_inputs == first_inputs {
        return Ok(first_evidence);
    }
    semantic_evidence_for_inputs(state, &reached_inputs).await
}

async fn revalidated_selection(
    state: &SemanticWorkerState,
    selection: &NativePolicySelection,
) -> Result<NativePolicySelection, NativePolicyRegexWorkerError> {
    let first_inputs = collect_native_policy_static_fallback_semantic_inputs(selection);
    let first_evidence = semantic_evidence_for_inputs(state, &first_inputs).await?;
    let reached_inputs = collect_native_policy_static_fallback_semantic_inputs_with_js(
        selection,
        Some(&first_evidence.js),
    );
    let evidence = if reached_inputs == first_inputs {
        first_evidence
    } else {
        semantic_evidence_for_inputs(state, &reached_inputs).await?
    };
    let mut selection = selection.clone();
    revalidate_native_policy_static_fallbacks(&mut selection, &evidence);
    Ok(selection)
}

/// Source-bearing refusal from Native policy-helper validation or execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyHelperStartupError {
    cause: PolicyHelperStartupErrorCause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PolicyHelperStartupErrorCause {
    Parser(NativePolicyStartupFatal),
    Execution(super::settings_watch::NativePolicyHelperFailure),
}

impl PolicyHelperStartupError {
    pub(crate) fn new(fatal: NativePolicyStartupFatal) -> Self {
        Self {
            cause: PolicyHelperStartupErrorCause::Parser(fatal),
        }
    }

    fn from_execution(failure: super::settings_watch::NativePolicyHelperFailure) -> Self {
        Self {
            cause: PolicyHelperStartupErrorCause::Execution(failure),
        }
    }

    #[must_use]
    pub fn source(&self) -> NativePolicySource {
        match &self.cause {
            PolicyHelperStartupErrorCause::Parser(fatal) => fatal.source,
            PolicyHelperStartupErrorCause::Execution(failure) => failure.source,
        }
    }

    #[must_use]
    pub fn source_paths(&self) -> &[std::path::PathBuf] {
        match &self.cause {
            PolicyHelperStartupErrorCause::Parser(fatal) => &fatal.source_paths,
            PolicyHelperStartupErrorCause::Execution(failure) => &failure.source_paths,
        }
    }

    #[must_use]
    pub fn field_path(&self) -> &str {
        match &self.cause {
            PolicyHelperStartupErrorCause::Parser(fatal) => &fatal.path,
            PolicyHelperStartupErrorCause::Execution(failure) => {
                failure.field_path.as_deref().unwrap_or("")
            }
        }
    }

    /// Exact Native diagnostic source when its Zod first-issue formatter is
    /// implemented. `None` is an explicit source gap; no replacement prose is
    /// synthesized here.
    #[must_use]
    pub fn diagnostic(&self) -> Option<&Utf16JsonProjection> {
        match &self.cause {
            PolicyHelperStartupErrorCause::Parser(fatal) => fatal.diagnostic.as_ref(),
            PolicyHelperStartupErrorCause::Execution(failure) => failure.diagnostic.as_ref(),
        }
    }
}

impl std::fmt::Display for PolicyHelperStartupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(diagnostic) = self.diagnostic() {
            if let Some(units) = diagnostic.string_units("") {
                return formatter.write_str(&String::from_utf16_lossy(&units));
            }
        }
        // The source field path is the only text available when Native's
        // validator message has not been represented. Do not invent prose.
        formatter.write_str(self.field_path())
    }
}

impl std::error::Error for PolicyHelperStartupError {}

/// Cloneable read-only status handle for product hosts that must distinguish
/// a Native policy-helper refusal from an ordinary requested session exit.
#[derive(Clone)]
pub struct PolicyHelperRefusalProbe {
    receiver: tokio::sync::watch::Receiver<Option<Arc<PolicyHelperStartupError>>>,
    #[cfg(any(test, feature = "test-support"))]
    test_sender: tokio::sync::watch::Sender<Option<Arc<PolicyHelperStartupError>>>,
}

impl PolicyHelperRefusalProbe {
    /// Return the retained terminal refusal, if one has already been accepted.
    #[must_use]
    pub fn current(&self) -> Option<Arc<PolicyHelperStartupError>> {
        self.receiver.borrow().clone()
    }

    /// Run a synchronous publication only while no terminal refusal is retained.
    /// The watch borrow remains held through the operation so a refusal cannot
    /// race between the check and an unbounded-channel enqueue.
    pub fn with_not_refused<R>(&self, operation: impl FnOnce() -> R) -> Option<R> {
        let current = self.receiver.borrow();
        if current.is_some() {
            None
        } else {
            let result = operation();
            drop(current);
            Some(result)
        }
    }

    /// Wait until the current process accepts a terminal policy-helper refusal.
    /// The watch value is retained, so callers that subscribe after the refusal
    /// still observe it without relying on a transient notification.
    pub async fn wait(&mut self) -> Arc<PolicyHelperStartupError> {
        loop {
            if let Some(error) = self.current() {
                return error;
            }
            if self.receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn test_probe() -> Self {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        Self {
            receiver,
            test_sender: sender,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn trigger_refusal_for_test(&self) {
        self.trigger_for_test(PolicyHelperStartupError::new(NativePolicyStartupFatal {
            source: NativePolicySource::ManagedFile,
            source_paths: Vec::new(),
            path: "policyHelpers.linux".to_owned(),
            diagnostic: None,
        }));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn trigger_for_test(&self, error: PolicyHelperStartupError) {
        let _ = retain_first_refusal(&self.test_sender, Arc::new(error));
    }
}

fn retain_first_refusal(
    sender: &tokio::sync::watch::Sender<Option<Arc<PolicyHelperStartupError>>>,
    error: Arc<PolicyHelperStartupError>,
) -> bool {
    sender.send_if_modified(|retained| {
        if retained.is_some() {
            false
        } else {
            *retained = Some(error);
            true
        }
    })
}

/// One stable controller owns process-local helper state and its optional
/// refresh task. The shared settings cache carries the current output into
/// both ordinary policy consumers and Managed instruction rows.
pub(super) struct NativePolicyHelperController {
    cache: super::settings_watch::ManagedPolicySnapshotCache,
    runner: Arc<dyn ProcessRunner>,
    semantic_worker: SemanticWorkerState,
    active_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    refresh_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    generation: Arc<AtomicU64>,
    refusal: tokio::sync::watch::Sender<Option<Arc<PolicyHelperStartupError>>>,
    semantic_worker_failure:
        tokio::sync::watch::Sender<Option<Arc<NativePolicySemanticWorkerFailure>>>,
    warning_logger: Option<Arc<dyn NativePolicyWarningLogger>>,
    on_apply: std::sync::RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl NativePolicyHelperController {
    pub(super) fn new(
        cache: super::settings_watch::ManagedPolicySnapshotCache,
        runner: Arc<dyn ProcessRunner>,
        warning_logger: Option<Arc<dyn NativePolicyWarningLogger>>,
    ) -> Self {
        let (refusal, _) = tokio::sync::watch::channel(None);
        let (semantic_worker_failure, _) = tokio::sync::watch::channel(None);
        Self {
            cache,
            runner,
            semantic_worker: Arc::new(std::sync::Mutex::new(None)),
            active_task: std::sync::Mutex::new(None),
            refresh_task: std::sync::Mutex::new(None),
            generation: Arc::new(AtomicU64::new(0)),
            refusal,
            semantic_worker_failure,
            warning_logger,
            on_apply: std::sync::RwLock::new(None),
        }
    }

    pub(super) fn set_on_apply(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        *self
            .on_apply
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(callback);
    }

    pub(super) fn check_startup_fatal(
        selection: &NativePolicySelection,
    ) -> Result<(), PolicyHelperStartupError> {
        if let Some(fatal) = selection.startup_fatal_helper_paths.first() {
            return Err(PolicyHelperStartupError::new(fatal.clone()));
        }
        Ok(())
    }

    pub(super) fn latest_failure(
        &self,
    ) -> Option<super::settings_watch::NativePolicyHelperFailure> {
        self.cache.native_helper_failure()
    }

    pub(super) fn initial_append_system_prompt(&self) -> Option<Utf16JsonProjection> {
        self.cache.native_helper_append_system_prompt()
    }

    pub(super) fn refusal_probe(&self) -> PolicyHelperRefusalProbe {
        PolicyHelperRefusalProbe {
            receiver: self.refusal.subscribe(),
            #[cfg(any(test, feature = "test-support"))]
            test_sender: self.refusal.clone(),
        }
    }

    pub(super) fn semantic_worker_failure_probe(&self) -> PolicySemanticWorkerProbe {
        PolicySemanticWorkerProbe {
            receiver: self.semantic_worker_failure.subscribe(),
        }
    }

    fn retain_semantic_worker_failure(
        &self,
        source: NativePolicySource,
        selection: &NativePolicySelection,
        error: NativePolicyRegexWorkerError,
        generation: u64,
    ) {
        if generation != self.generation.load(Ordering::Acquire) {
            return;
        }
        let source_paths = selection
            .helper_plan
            .as_ref()
            .filter(|plan| plan.source == source)
            .map_or_else(Vec::new, |plan| plan.source_paths.clone());
        let failure = NativePolicySemanticWorkerFailure {
            source,
            source_paths,
            generation,
            error,
        };
        if self.cache.record_semantic_worker_failure(failure.clone()) {
            self.semantic_worker_failure
                .send_replace(Some(Arc::new(failure)));
        }
    }

    fn clear_semantic_worker_failure_on_source_change(&self) {
        self.cache.clear_semantic_worker_failure_on_source_change();
        self.semantic_worker_failure.send_replace(None);
    }

    fn clear_semantic_worker_failure_after_success(
        &self,
        source: NativePolicySource,
        generation: u64,
    ) {
        if generation != self.generation.load(Ordering::Acquire) {
            return;
        }
        let _ = self
            .cache
            .clear_semantic_worker_failure_after_success(source, generation);
        let current = self.cache.semantic_worker_failure().map(Arc::new);
        self.semantic_worker_failure.send_replace(current);
    }

    fn retain_current_refusal(&self) -> bool {
        let generation = self.generation.load(Ordering::Acquire);
        let Some(failure) = self.cache.native_helper_failure().filter(|failure| {
            failure.on_failure == Some(NativePolicyHelperOnFailure::Refuse)
                && failure.generation == generation
                && failure.selected_admin_at_failure == Some(failure.source)
        }) else {
            return false;
        };
        if self
            .cache
            .snapshot()
            .is_none_or(|snapshot| snapshot.selection.selected_admin != Some(failure.source))
        {
            return false;
        }

        retain_first_refusal(
            &self.refusal,
            Arc::new(PolicyHelperStartupError::from_execution(failure)),
        )
    }

    async fn log_first_on_chain_static_warnings(&self, selection: &NativePolicySelection) {
        let Some(logger) = self.warning_logger.as_deref() else {
            return;
        };
        log_first_on_chain_static_warnings(&self.semantic_worker, selection, logger).await;
    }

    /// Apply only a Core-validated Native static fallback during startup or
    /// an admitted source transition. The pre-action source path may also run
    /// an executable helper; this method itself never spawns a process.
    pub(super) fn apply_initial_static_fallback(
        &self,
        selection: &NativePolicySelection,
    ) -> Result<bool, NativePolicyHelperApplyStatus> {
        Self::check_startup_fatal(selection).map_err(|_| {
            NativePolicyHelperApplyStatus::StartupFatal {
                diagnostics: selection.startup_fatal_helper_paths.clone(),
            }
        })?;
        let Some((field, _settings, _behavior)) = selected_static_fallback(selection) else {
            return Ok(false);
        };
        let source = selection
            .helper_plan
            .as_ref()
            .map(|plan| plan.source)
            .ok_or(NativePolicyHelperApplyStatus::SourceMismatch)?;
        if selection
            .helper_plan
            .as_ref()
            .is_none_or(|plan| plan.selected_kind != NativePolicyHelperSelectedKind::StaticFallback)
        {
            return Ok(false);
        }
        self.cache.apply_helper_settings(
            source,
            NativePolicyHelperSettingsInput::StaticFallback {
                source_field: field.to_owned(),
            },
            None,
        )?;
        Ok(true)
    }

    /// Mirror the initial local helper pass in Native's startup pre-action.
    /// This must finish before effective settings and prompt consumers are
    /// constructed. Refuse becomes a typed build error; Continue leaves the
    /// base policy in place.
    pub(super) async fn run_initial_local_helper(
        self: &Arc<Self>,
        selection: &NativePolicySelection,
    ) -> Result<(), InitialPolicyHelperError> {
        Self::check_startup_fatal(selection)?;
        if !matches!(
            selection.selected_admin,
            Some(NativePolicySource::Plist | NativePolicySource::ManagedFile)
        ) {
            return Ok(());
        }
        let checked_selection = revalidated_selection(&self.semantic_worker, selection)
            .await
            .map_err(InitialPolicyHelperError::SemanticWorker)?;
        Self::check_startup_fatal(&checked_selection)?;
        self.log_first_on_chain_static_warnings(&checked_selection)
            .await;
        if let Some(mut snapshot) = self.cache.snapshot() {
            snapshot.selection = checked_selection.clone();
            self.cache.replace(snapshot);
        }
        if let Err(status) = self.apply_initial_static_fallback(&checked_selection) {
            tracing::warn!(
                target: "harness_runtime::desktop::policy_helper",
                ?status,
                "validated Native static policy-helper fallback was not applied"
            );
        }
        let base_selection = self
            .cache
            .snapshot()
            .map_or_else(|| checked_selection.clone(), |snapshot| snapshot.selection);
        let Some(plan) = base_selection.helper_plan.as_ref() else {
            return Ok(());
        };
        if !matches!(
            plan.selected_kind,
            NativePolicyHelperSelectedKind::PerOsHelper
                | NativePolicyHelperSelectedKind::SingularHelper
        ) {
            return Ok(());
        }
        if let Err(fields) = executable_plan(&base_selection) {
            tracing::warn!(
                target: "harness_runtime::desktop::policy_helper",
                ?fields,
                "Native policy helper was not run because source validation is unresolved"
            );
            return Ok(());
        }
        let entry = if plan.selected_kind == NativePolicyHelperSelectedKind::PerOsHelper {
            plan.selected_entry.clone()
        } else {
            plan.singular_entry.clone()
        };
        let Some(entry) = entry else {
            return Ok(());
        };
        let source = plan.source;
        let platform = plan.platform;
        let generation = self.generation.load(Ordering::Acquire);
        let outcome = run_selected_helper(
            &self.cache,
            self.runner.as_ref(),
            &self.semantic_worker,
            self.warning_logger.as_deref(),
            source,
            platform,
            &entry,
            &base_selection,
            &self.generation,
            generation,
        )
        .await
        .map_err(InitialPolicyHelperError::SemanticWorker)?;
        if outcome.uses_refresh_config() && entry.refresh_interval_ms > 0 {
            self.start_refresh_task(
                source,
                platform,
                entry.clone(),
                base_selection.clone(),
                generation,
            );
        }
        if outcome == HelperRunOutcome::Failed
            && entry.on_failure == NativePolicyHelperOnFailure::Refuse
        {
            if let Some(failure) = self.cache.native_helper_failure() {
                return Err(PolicyHelperStartupError::from_execution(failure).into());
            }
        }
        Ok(())
    }

    /// Called only after a settled admin source identity transition in the
    /// settings watcher. The watcher establishes the baseline at construction
    /// and never calls this for an ordinary same-source settings update.
    pub(super) fn on_admin_source_change(
        self: &Arc<Self>,
        snapshot: super::settings_watch::ManagedSettingsSnapshot,
    ) {
        self.cancel_refresh_task();
        self.cancel_active_task();
        // This callback runs only for the watcher's settled source-origin
        // transition. Clear both the authoritative cache row and its public
        // watch value; the cache has already discarded stale source state.
        self.clear_semantic_worker_failure_on_source_change();
        let local_admin = matches!(
            snapshot.selection.selected_admin,
            Some(NativePolicySource::Plist | NativePolicySource::ManagedFile)
        );
        if !local_admin {
            self.notify_applied();
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        let source = snapshot
            .selection
            .selected_admin
            .expect("local admin checked");
        let cache = self.cache.clone();
        let runner = Arc::clone(&self.runner);
        let semantic_worker = Arc::clone(&self.semantic_worker);
        let warning_logger = self.warning_logger.clone();
        let generation_counter = Arc::clone(&self.generation);
        let weak_controller = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let base_selection =
                match revalidated_selection(&semantic_worker, &snapshot.selection).await {
                    Ok(selection) => selection,
                    Err(error) => {
                        if generation_counter.load(Ordering::Acquire) != generation
                            || cache.snapshot().is_none_or(|current| {
                                current.selection.selected_admin != Some(source)
                            })
                        {
                            return;
                        }
                        tracing::error!(
                            target: "harness_runtime::desktop::policy_helper",
                            error = %error,
                            "Native policy semantic worker failed during source refresh"
                        );
                        if let Some(controller) = weak_controller.upgrade() {
                            controller.retain_semantic_worker_failure(
                                source,
                                &snapshot.selection,
                                error,
                                generation,
                            );
                            controller.notify_applied();
                        }
                        return;
                    }
                };
            if generation_counter.load(Ordering::Acquire) != generation
                || cache
                    .snapshot()
                    .is_none_or(|current| current.selection.selected_admin != Some(source))
            {
                return;
            }
            if let Some(mut current) = cache.snapshot() {
                current.selection = base_selection.clone();
                cache.replace(current);
            }
            if let Some(controller) = weak_controller.upgrade() {
                controller.clear_semantic_worker_failure_after_success(source, generation);
            }
            if let Err(error) = Self::check_startup_fatal(&base_selection) {
                let _ = cache.record_native_helper_failure(
                    super::settings_watch::NativePolicyHelperFailure {
                        source: error.source(),
                        source_paths: error.source_paths().to_vec(),
                        diagnostic: error.diagnostic().cloned(),
                        field_path: Some(error.field_path().to_owned()),
                        code: FailureCode::SchemaRejected,
                        on_failure: Some(NativePolicyHelperOnFailure::Refuse),
                        attempts: 0,
                        generation,
                        selected_admin_at_failure: Some(source),
                    },
                );
                tracing::error!(
                    target: "harness_runtime::desktop::policy_helper",
                    source = ?error.source(),
                    path = error.field_path(),
                    diagnostic = %error,
                    "refusing Native policy helper refresh"
                );
                if let Some(controller) = weak_controller.upgrade() {
                    controller.notify_applied();
                }
                return;
            }
            if let Some(logger) = warning_logger.as_deref() {
                log_first_on_chain_static_warnings(&semantic_worker, &base_selection, logger).await;
            }
            if let Err(fields) = executable_plan(&base_selection) {
                tracing::warn!(
                    target: "harness_runtime::desktop::policy_helper",
                    ?fields,
                    "Native policy helper plan is not executable with current source validation"
                );
                if let Some(controller) = weak_controller.upgrade() {
                    controller.notify_applied();
                }
                return;
            }
            let Some(plan) = base_selection.helper_plan.as_ref() else {
                if let Some(controller) = weak_controller.upgrade() {
                    controller.notify_applied();
                }
                return;
            };
            match plan.selected_kind {
                NativePolicyHelperSelectedKind::StaticFallback => {
                    if let Some((field, _settings, _behavior)) =
                        selected_static_fallback(&base_selection)
                    {
                        match cache.apply_helper_settings(
                            source,
                            NativePolicyHelperSettingsInput::StaticFallback {
                                source_field: field.to_owned(),
                            },
                            None,
                        ) {
                            Ok(_) => tracing::info!(
                                target: "harness_runtime::desktop::policy_helper",
                                field,
                                "applied Native static policy-helper fallback"
                            ),
                            Err(status) => tracing::warn!(
                                target: "harness_runtime::desktop::policy_helper",
                                ?status,
                                "could not apply Native static policy-helper fallback"
                            ),
                        }
                    }
                    if let Some(controller) = weak_controller.upgrade() {
                        controller.notify_applied();
                    }
                }
                NativePolicyHelperSelectedKind::PerOsHelper
                | NativePolicyHelperSelectedKind::SingularHelper => {
                    let entry = if plan.selected_kind == NativePolicyHelperSelectedKind::PerOsHelper
                    {
                        plan.selected_entry.clone()
                    } else {
                        plan.singular_entry.clone()
                    };
                    let Some(entry) = entry else {
                        if let Some(controller) = weak_controller.upgrade() {
                            controller.notify_applied();
                        }
                        return;
                    };
                    let platform = plan.platform;
                    let outcome = run_selected_helper(
                        &cache,
                        runner.as_ref(),
                        &semantic_worker,
                        warning_logger.as_deref(),
                        source,
                        platform,
                        &entry,
                        &base_selection,
                        &generation_counter,
                        generation,
                    )
                    .await;
                    if generation_counter.load(Ordering::Acquire) != generation
                        || cache
                            .snapshot()
                            .is_none_or(|current| current.selection.selected_admin != Some(source))
                    {
                        return;
                    }
                    let Some(controller) = weak_controller.upgrade() else {
                        return;
                    };
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            controller.retain_semantic_worker_failure(
                                source,
                                &base_selection,
                                error.clone(),
                                generation,
                            );
                            tracing::error!(
                                target: "harness_runtime::desktop::policy_helper",
                                error = %error,
                                "Native policy semantic worker failed while validating helper output"
                            );
                            controller.notify_applied();
                            return;
                        }
                    };
                    controller.clear_semantic_worker_failure_after_success(source, generation);
                    controller.notify_applied();
                    if outcome.uses_refresh_config() && entry.refresh_interval_ms > 0 {
                        controller.start_refresh_task(
                            source,
                            platform,
                            entry,
                            base_selection.clone(),
                            generation,
                        );
                    }
                }
                NativePolicyHelperSelectedKind::NoEntry => {
                    if let Some(controller) = weak_controller.upgrade() {
                        controller.notify_applied();
                    }
                }
            }
        });
        *self
            .active_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }

    fn start_refresh_task(
        self: &Arc<Self>,
        source: NativePolicySource,
        platform: NativePolicyPlatform,
        entry: NativePolicyHelperEntry,
        base_selection: NativePolicySelection,
        generation: u64,
    ) {
        let cache = self.cache.clone();
        let runner = Arc::clone(&self.runner);
        let semantic_worker = Arc::clone(&self.semantic_worker);
        let warning_logger = self.warning_logger.clone();
        let generation_counter = Arc::clone(&self.generation);
        let weak_controller = Arc::downgrade(self);
        let interval = Duration::from_millis(u64::from(entry.refresh_interval_ms));
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if generation_counter.load(Ordering::Acquire) != generation {
                    return;
                }
                let Some(current) = cache.snapshot() else {
                    return;
                };
                if current.selection.selected_admin != Some(source) {
                    return;
                }
                let outcome = run_selected_helper(
                    &cache,
                    runner.as_ref(),
                    &semantic_worker,
                    warning_logger.as_deref(),
                    source,
                    platform,
                    &entry,
                    &base_selection,
                    &generation_counter,
                    generation,
                )
                .await;
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if generation_counter.load(Ordering::Acquire) != generation
                            || cache.snapshot().is_none_or(|current| {
                                current.selection.selected_admin != Some(source)
                            })
                        {
                            return;
                        }
                        if let Some(controller) = weak_controller.upgrade() {
                            controller.retain_semantic_worker_failure(
                                source,
                                &base_selection,
                                error.clone(),
                                generation,
                            );
                            controller.notify_applied();
                        }
                        tracing::error!(
                            target: "harness_runtime::desktop::policy_helper",
                            error = %error,
                            "Native policy semantic worker failed during helper refresh"
                        );
                        continue;
                    }
                };
                if matches!(outcome, HelperRunOutcome::Stale) {
                    return;
                }
                let Some(controller) = weak_controller.upgrade() else {
                    return;
                };
                controller.clear_semantic_worker_failure_after_success(source, generation);
                let callback = controller
                    .on_apply
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if outcome.uses_refresh_config() {
                    if let Some(callback) = callback.as_ref() {
                        callback();
                    }
                } else if matches!(outcome, HelperRunOutcome::StaticFallbackApplied { .. }) {
                    if let Some(callback) = callback.as_ref() {
                        callback();
                    }
                    return;
                } else if outcome == HelperRunOutcome::Failed
                    && controller.retained_refusal_matches(source, generation)
                {
                    controller.notify_applied();
                    return;
                }
            }
        });
        *self
            .refresh_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }

    fn cancel_refresh_task(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(task) = self
            .refresh_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    fn cancel_active_task(&self) {
        if let Some(task) = self
            .active_task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }

    fn notify_applied(&self) {
        self.retain_current_refusal();
        if let Some(callback) = self
            .on_apply
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            callback();
        }
    }

    fn retained_refusal_matches(&self, source: NativePolicySource, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) == generation
            && self.cache.native_helper_failure().is_some_and(|failure| {
                failure.source == source
                    && failure.generation == generation
                    && failure.on_failure == Some(NativePolicyHelperOnFailure::Refuse)
                    && failure.selected_admin_at_failure == Some(source)
                    && self
                        .cache
                        .snapshot()
                        .is_some_and(|snapshot| snapshot.selection.selected_admin == Some(source))
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperRunOutcome {
    HelperApplied,
    StaticFallbackApplied { retain_refresh: bool },
    Failed,
    Stale,
}

impl HelperRunOutcome {
    fn uses_refresh_config(self) -> bool {
        matches!(
            self,
            Self::HelperApplied
                | Self::StaticFallbackApplied {
                    retain_refresh: true
                }
        )
    }
}

async fn run_selected_helper(
    cache: &super::settings_watch::ManagedPolicySnapshotCache,
    runner: &dyn ProcessRunner,
    semantic_worker: &SemanticWorkerState,
    warning_logger: Option<&dyn NativePolicyWarningLogger>,
    source: NativePolicySource,
    platform: NativePolicyPlatform,
    entry: &NativePolicyHelperEntry,
    base_selection: &NativePolicySelection,
    generation_counter: &AtomicU64,
    expected_generation: u64,
) -> Result<HelperRunOutcome, NativePolicyRegexWorkerError> {
    let helper_result = execute_entry(entry, platform, runner).await;
    if generation_counter.load(Ordering::Acquire) != expected_generation
        || cache
            .snapshot()
            .is_none_or(|snapshot| snapshot.selection.selected_admin != Some(source))
    {
        return Ok(HelperRunOutcome::Stale);
    }
    let failure = match helper_result {
        Ok(output) => {
            let semantic_evidence = if let Some(settings) = output.managed_settings.as_ref() {
                semantic_evidence_for_projection(semantic_worker, settings).await?
            } else {
                empty_semantic_evidence()
            };
            let settings_for_log = output.managed_settings.clone();
            match cache.apply_helper_output(
                source,
                entry.output_behavior,
                output,
                semantic_evidence,
            ) {
                Ok(snapshot) => {
                    if let (Some(logger), Some(settings)) =
                        (warning_logger, settings_for_log.as_ref())
                    {
                        if let Some(messages) =
                            lingxi_core::settings::native_policy::native_policy_qn_log_messages(
                                settings,
                                &snapshot.selection.helper_warnings,
                                "policyHelper",
                            )
                        {
                            emit_native_policy_warning_messages(semantic_worker, logger, &messages)
                                .await;
                        }
                    }
                    return Ok(HelperRunOutcome::HelperApplied);
                }
                Err(status) => {
                    tracing::warn!(
                        target: "harness_runtime::desktop::policy_helper",
                        ?status,
                        "Native helper output could not be admitted"
                    );
                    let (diagnostic, field_path) = apply_status_error(status);
                    ExecutionFailure {
                        code: FailureCode::SchemaRejected,
                        diagnostic,
                        field_path,
                        attempts: 1,
                    }
                }
            }
        }
        Err(failure) => failure,
    };

    if let Some((field, _settings, _candidate_behavior)) = selected_static_fallback(base_selection)
    {
        match cache.apply_helper_settings(
            source,
            NativePolicyHelperSettingsInput::StaticFallback {
                source_field: field.to_owned(),
            },
            None,
        ) {
            Ok(_) => {
                tracing::warn!(
                    target: "harness_runtime::desktop::policy_helper",
                    field,
                    failure = %execution_failure_display(&failure),
                    "Native helper failed; applied its validated static fallback"
                );
                return Ok(HelperRunOutcome::StaticFallbackApplied {
                    retain_refresh: failure.code != FailureCode::BadPath,
                });
            }
            Err(status) => tracing::warn!(
                target: "harness_runtime::desktop::policy_helper",
                field,
                ?status,
                "Native helper static fallback could not be applied"
            ),
        }
    }

    let source_paths = base_selection
        .helper_plan
        .as_ref()
        .filter(|plan| plan.source == source)
        .map_or_else(Vec::new, |plan| plan.source_paths.clone());
    let _ = cache.record_native_helper_failure(super::settings_watch::NativePolicyHelperFailure {
        source,
        source_paths,
        // Native Hn() adds this outer message after It() has already applied
        // Xi()'s truncation and retry prefix.
        diagnostic: failure
            .diagnostic
            .as_ref()
            .and_then(|diagnostic| prefix_source_projection("policyHelper failed: ", diagnostic)),
        field_path: failure.field_path.clone(),
        code: failure.code,
        on_failure: Some(entry.on_failure),
        attempts: failure.attempts,
        generation: expected_generation,
        selected_admin_at_failure: Some(source),
    });

    match entry.on_failure {
        NativePolicyHelperOnFailure::Continue => tracing::warn!(
            target: "harness_runtime::desktop::policy_helper",
            code = ?failure.code,
            attempts = failure.attempts,
            diagnostic = %execution_failure_display(&failure),
            "Native helper failed with onFailure=continue"
        ),
        NativePolicyHelperOnFailure::Refuse => tracing::error!(
            target: "harness_runtime::desktop::policy_helper",
            code = ?failure.code,
            attempts = failure.attempts,
            diagnostic = %execution_failure_display(&failure),
            "Native helper failed with onFailure=refuse"
        ),
    }
    Ok(HelperRunOutcome::Failed)
}

impl Drop for NativePolicyHelperController {
    fn drop(&mut self) {
        if let Some(task) = self
            .active_task
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        if let Some(task) = self
            .refresh_task
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct DirectProcessRunner;

#[async_trait]
impl ProcessRunner for DirectProcessRunner {
    async fn run(&self, invocation: Invocation) -> Result<ProcessOutput, ProcessFailure> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let drop_guard = CancelOnDrop(Arc::clone(&cancelled));
        let timeout = invocation.timeout;
        let total_timeout = invocation.total_timeout;
        let task_cancelled = Arc::clone(&cancelled);
        let task = tokio::task::spawn_blocking(move || run_blocking(invocation, task_cancelled));
        let result = match tokio::time::timeout(timeout, task).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(ProcessFailure {
                code: FailureCode::ExitNonzero,
                detail: error.to_string(),
            }),
            Err(_) => Err(ProcessFailure {
                code: FailureCode::TimedOut,
                detail: format!("timed out after {}ms", total_timeout.as_millis()),
            }),
        };
        drop(drop_guard);
        result
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn run_blocking(
    invocation: Invocation,
    cancelled: Arc<AtomicBool>,
) -> Result<ProcessOutput, ProcessFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(ProcessFailure {
            code: FailureCode::TimedOut,
            detail: format!("timed out after {}ms", invocation.total_timeout.as_millis()),
        });
    }
    let mut command = Command::new(os_string(&invocation.file));
    command
        .args(invocation.args.iter().map(|arg| os_string(arg)))
        .current_dir(os_string(&invocation.cwd))
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if invocation.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    for (key, value) in &invocation.env {
        command.env(os_string(key), os_string(value));
    }
    let mut child = command.spawn().map_err(|_error| ProcessFailure {
        // Native's caught spawn-failure result has code 1 and no error field;
        // Zi() formats it as `exited with code 1: `.
        code: FailureCode::ExitNonzero,
        detail: "exited with code 1: ".into(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| ProcessFailure {
        code: FailureCode::ExitNonzero,
        detail: "stdout pipe was not created".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ProcessFailure {
        code: FailureCode::ExitNonzero,
        detail: "stderr pipe was not created".into(),
    })?;
    let stdout_utf16_len = Arc::new(AtomicUsize::new(0));
    let stderr_utf16_len = Arc::new(AtomicUsize::new(0));
    let stdout_overflow = Arc::new(AtomicBool::new(false));
    let stderr_overflow = Arc::new(AtomicBool::new(false));
    let (reader_tx, reader_rx) = mpsc::channel();
    let _stdout_reader = pipe_reader(
        stdout,
        Arc::clone(&stdout_utf16_len),
        Arc::clone(&stdout_overflow),
        reader_tx.clone(),
        true,
    );
    let _stderr_reader = pipe_reader(
        stderr,
        Arc::clone(&stderr_utf16_len),
        Arc::clone(&stderr_overflow),
        reader_tx.clone(),
        false,
    );
    drop(reader_tx);
    if let Some(units) = invocation.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            std::thread::spawn(move || {
                let _ = stdin.write_all(utf16_to_native_string(&units).as_bytes());
            });
        }
    }

    let started = Instant::now();
    let mut status = None;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    loop {
        loop {
            match reader_rx.try_recv() {
                Ok((is_stdout, bytes)) => {
                    if is_stdout {
                        stdout_bytes = Some(bytes);
                    } else {
                        stderr_bytes = Some(bytes);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected)
                    if stdout_bytes.is_some() && stderr_bytes.is_some() =>
                {
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcessFailure {
                        code: FailureCode::ExitNonzero,
                        detail: "stdout or stderr reader stopped before EOF".into(),
                    });
                }
            }
        }
        let stdout_exceeded = stdout_overflow.load(Ordering::Acquire);
        let stderr_exceeded = stderr_overflow.load(Ordering::Acquire);
        if stdout_exceeded || stderr_exceeded {
            let _ = child.kill();
            let _ = child.wait();
            // Execa caps both decoded streams independently at maxBuffer. If
            // both overflow, both retained strings reach the same cap and
            // Native's strict `stderr.length > stdout.length` check selects
            // stdout on the tie.
            let stream = if stderr_exceeded && !stdout_exceeded {
                "stderr"
            } else {
                "stdout"
            };
            return Err(ProcessFailure {
                code: FailureCode::Oversize,
                detail: format!("{stream} exceeded {OUTPUT_LIMIT} bytes"),
            });
        }
        if cancelled.load(Ordering::Acquire) || started.elapsed() >= invocation.timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProcessFailure {
                code: FailureCode::TimedOut,
                detail: format!("timed out after {}ms", invocation.total_timeout.as_millis()),
            });
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcessFailure {
                        code: FailureCode::ExitNonzero,
                        detail: error.to_string(),
                    });
                }
            }
        }
        if status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = status.expect("loop exits only after child exit");
    let stdout = stdout_bytes.expect("loop exits only after stdout EOF");
    let stderr = stderr_bytes.expect("loop exits only after stderr EOF");
    if !status.success() {
        let detail = if stderr.is_empty() { &stdout } else { &stderr };
        return Err(ProcessFailure {
            code: FailureCode::ExitNonzero,
            detail: format!(
                "exited with code {}: {}",
                status
                    .code()
                    .map_or_else(|| "unknown".into(), |code| code.to_string()),
                String::from_utf8_lossy(detail)
            ),
        });
    }
    Ok(ProcessOutput {
        stdout,
        stderr,
        exit_code: status.code(),
    })
}

fn pipe_reader<R: Read + Send + 'static>(
    mut reader: R,
    utf16_unit_count: Arc<AtomicUsize>,
    stream_overflow: Arc<AtomicBool>,
    sender: mpsc::Sender<(bool, Vec<u8>)>,
    is_stdout: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut output = Vec::with_capacity(16 * 1024);
        // Valid UTF-8 uses at most four bytes per UTF-16 code unit. Retain
        // enough bytes for Native's per-stream maxBuffer in JS string units.
        let maximum_bytes = PROCESS_BUFFER_LIMIT.saturating_mul(4).saturating_add(4);
        let mut buffer = [0_u8; 8192];
        let mut incomplete_utf8 = Vec::new();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => {
                    if !incomplete_utf8.is_empty() {
                        let units = utf16_unit_count.fetch_add(1, Ordering::Relaxed) + 1;
                        if units > PROCESS_BUFFER_LIMIT {
                            stream_overflow.store(true, Ordering::Release);
                        }
                    }
                    break;
                }
                Ok(read) => {
                    let decoded_units =
                        count_utf16_lossy_incremental(&buffer[..read], &mut incomplete_utf8);
                    let units = utf16_unit_count.fetch_add(decoded_units, Ordering::Relaxed)
                        + decoded_units;
                    let remaining = maximum_bytes.saturating_sub(output.len());
                    output.extend_from_slice(&buffer[..read.min(remaining)]);
                    if units > PROCESS_BUFFER_LIMIT {
                        stream_overflow.store(true, Ordering::Release);
                    }
                }
            }
        }
        let _ = sender.send((is_stdout, output));
    })
}

fn count_utf16_lossy_incremental(bytes: &[u8], pending: &mut Vec<u8>) -> usize {
    let mut combined = std::mem::take(pending);
    combined.extend_from_slice(bytes);
    let mut offset = 0;
    let mut units = 0;
    while offset < combined.len() {
        match std::str::from_utf8(&combined[offset..]) {
            Ok(text) => {
                units += text.encode_utf16().count();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    units += std::str::from_utf8(&combined[offset..offset + valid])
                        .expect("valid UTF-8 prefix")
                        .encode_utf16()
                        .count();
                    offset += valid;
                }
                match error.error_len() {
                    Some(error_len) => {
                        units += 1;
                        offset = offset.saturating_add(error_len);
                    }
                    None => {
                        pending.extend_from_slice(&combined[offset..]);
                        break;
                    }
                }
            }
        }
    }
    units
}

/// Build the Native-selected direct path or inline-script process plan.
pub(super) fn invocation_for_entry(
    entry: &NativePolicyHelperEntry,
    platform: NativePolicyPlatform,
) -> Result<Invocation, ProcessFailure> {
    let script = entry.config.string_units("/script");
    let path = entry.config.string_units("/path");
    let interpreter = entry.config.string_at("/interpreter");
    let (file, args, stdin, routed_interpreter, cwd, ps1_path) = match (script, interpreter) {
        (Some(script), Some("sh")) if platform != NativePolicyPlatform::Windows => (
            ascii_units("/bin/sh"),
            vec![ascii_units("-s")],
            Some(script),
            Some(Interpreter::Sh),
            ascii_units("/"),
            None,
        ),
        (Some(script), Some("pwsh")) if platform == NativePolicyPlatform::Windows => {
            let Some(powershell) = find_native_powershell() else {
                return Err(ProcessFailure {
                    code: FailureCode::InterpreterUnavailable,
                    detail: "PowerShell not found at any of its stock install locations on C: (fixed absolute candidates only; PATH is never consulted)".into(),
                });
            };
            let mut args = powershell_arguments();
            args.push(ascii_units("-Command"));
            args.push(ascii_units(POWERSHELL_INLINE_SCRIPT));
            let cwd = parent_units(&powershell);
            (
                powershell,
                args,
                Some(script),
                Some(Interpreter::Pwsh),
                cwd,
                None,
            )
        }
        (Some(_), _) => {
            return Err(ProcessFailure {
                code: FailureCode::BadPath,
                detail: "inline helper interpreter does not match the selected platform".into(),
            });
        }
        (None, _) => {
            let Some(path) = path else {
                return Err(ProcessFailure {
                    code: FailureCode::BadPath,
                    detail: "no path or script configured".into(),
                });
            };
            if platform == NativePolicyPlatform::Windows && windows_path_is_ps1(&path) {
                let Some(powershell) = find_native_powershell() else {
                    return Err(ProcessFailure {
                        code: FailureCode::InterpreterUnavailable,
                        detail: "PowerShell not found at any of its stock install locations on C: (fixed absolute candidates only; PATH is never consulted)".into(),
                    });
                };
                let mut args = powershell_arguments();
                args.push(ascii_units("-Command"));
                args.push(ascii_units(POWERSHELL_FILE_SCRIPT));
                let cwd = parent_units(&powershell);
                (
                    powershell,
                    args,
                    None,
                    Some(Interpreter::Pwsh),
                    cwd,
                    Some(path),
                )
            } else {
                let cwd = if platform == NativePolicyPlatform::Windows {
                    parent_units(&path)
                } else {
                    ascii_units("/")
                };
                (path, Vec::new(), None, None, cwd, None)
            }
        }
    };
    let mut invocation = Invocation {
        file,
        args,
        stdin,
        cwd,
        env: current_environment(),
        timeout: Duration::from_millis(u64::from(entry.timeout_ms)),
        total_timeout: Duration::from_millis(u64::from(entry.timeout_ms)),
        routed_interpreter,
        ps1_path,
    };
    set_interpreter_environment(&mut invocation);
    Ok(invocation)
}

const POWERSHELL_INLINE_SCRIPT: &str = "$env:PSModulePath = $env:CLAUDE_CODE_POLICY_HELPER_PSMODULEPATH; if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') { try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new() } catch {} }; $LASTEXITCODE = 0; try { Invoke-Expression (@($input) -join [char]10) } catch { Write-Error $_; exit 1 }; exit $LASTEXITCODE";
const POWERSHELL_FILE_SCRIPT: &str = "$env:PSModulePath = $env:CLAUDE_CODE_POLICY_HELPER_PSMODULEPATH; if ($ExecutionContext.SessionState.LanguageMode -eq 'FullLanguage') { try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new() } catch {} }; $LASTEXITCODE = 0; try { & ($env:CLAUDE_CODE_POLICY_HELPER_PS1_PATH) } catch { Write-Error $_; exit 1 }; exit $LASTEXITCODE";

fn powershell_arguments() -> Vec<Vec<u16>> {
    [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
    ]
    .into_iter()
    .map(ascii_units)
    .collect()
}

fn windows_path_is_ps1(path: &[u16]) -> bool {
    let suffix = ".ps1".encode_utf16().collect::<Vec<_>>();
    path.len() >= suffix.len()
        && path[path.len() - suffix.len()..]
            .iter()
            .zip(suffix.iter())
            .all(|(actual, expected)| ascii_lower_unit(*actual) == *expected)
}

fn ascii_lower_unit(value: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&value) {
        value + (u16::from(b'a') - u16::from(b'A'))
    } else {
        value
    }
}

fn find_native_powershell() -> Option<Vec<u16>> {
    const CANDIDATES: &[&str] = &[
        r"C:\Program Files\PowerShell\7\pwsh.exe",
        r"C:\Program Files\PowerShell\7-preview\pwsh.exe",
        r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
    ];
    for candidate in CANDIDATES {
        let candidate_path = PathBuf::from(candidate);
        match std::fs::canonicalize(&candidate_path) {
            Ok(canonical) => {
                if canonical
                    .to_str()
                    .map(native_windows_realpath_text)
                    .is_some_and(|resolved| resolved.eq_ignore_ascii_case(candidate))
                {
                    return Some(ascii_units(candidate));
                }
            }
            Err(_) if candidate_path.exists() => return None,
            Err(_) => continue,
        }
    }
    None
}

/// Native `zse` applies the `tC` Node path-namespace projection to
/// `realpathSync.native` before `po` compares the fixed PowerShell candidate.
/// Source: `insights-prepared/metadata-source/native-bundle/
/// src_186068928.mjs`, SHA-256
/// `88e71b90c23c57422aad6e86ca189e1fa0db0ded507932db63c8455889a77de4`.
fn native_windows_realpath_text(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if path.starts_with(r"\\?\") && path.as_bytes().get(5) == Some(&b':') {
        return path[4..].to_owned();
    }
    path.to_owned()
}

fn set_interpreter_environment(invocation: &mut Invocation) {
    if invocation.routed_interpreter == Some(Interpreter::Sh) {
        const DROP_KEYS: &[&str] = &[
            "ENV",
            "BASH_ENV",
            "SHELLOPTS",
            "PS4",
            "GCONV_PATH",
            "IFS",
            "PWD",
            "CDPATH",
            "OLDPWD",
            "TMOUT",
            "POSIXLY_CORRECT",
            "BASHOPTS",
            "BASH_COMPAT",
            "EXECIGNORE",
            "BASH_SOURCE_PATH",
            "BASH_LOADABLES_PATH",
            "GLOBIGNORE",
            "GLOBSORT",
            "LOCPATH",
            "PATH_LOCALE",
            "NLSPATH",
            "LANG",
            "TMPDIR",
            "TMP",
            "TEMP",
        ];
        const DROP_PREFIXES: &[&str] = &["LD_", "DYLD_", "BASH_FUNC_", "__BASH_FUNC<", "LC_"];
        invocation.env.retain(|key, _| {
            let key = String::from_utf16_lossy(key);
            !DROP_KEYS.contains(&key.as_str())
                && !DROP_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        });
        set_env(&mut invocation.env, "PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        set_env(&mut invocation.env, "LC_ALL", "C");
    } else if invocation.routed_interpreter == Some(Interpreter::Pwsh) {
        invocation.env = windows_interpreter_environment(invocation);
    }
    set_env(&mut invocation.env, "CLAUDE_CODE_VERSION", HELPER_VERSION);
}

fn windows_interpreter_environment(invocation: &Invocation) -> BTreeMap<Vec<u16>, Vec<u16>> {
    const DROP_KEYS: &[&str] = &[
        "DEVPATH",
        "__PSLOCKDOWNPOLICY",
        "PSMODULEANALYSISCACHEPATH",
        "PSMODULEPATH",
        "PATH",
        "PATHEXT",
        "COMSPEC",
        "TERM",
        POWERSHELL_MODULE_PATH_ENV,
        POWERSHELL_SCRIPT_PATH_ENV,
    ];
    const DROP_PREFIXES: &[&str] = &[
        "DOTNET_",
        "COMPLUS_",
        "COR_",
        "CORECLR_",
        "APPDOMAIN_MANAGER_",
    ];
    let mut environment = invocation.env.clone();
    environment.retain(|key, _| {
        let key = String::from_utf16_lossy(key).to_uppercase();
        !DROP_KEYS.contains(&key.as_str())
            && !DROP_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
    });

    let executable_dir = parent_units(&invocation.file);
    let system_root = ascii_units(r"C:\Windows");
    let system32 = ascii_units(r"C:\Windows\System32");
    let module_paths = join_windows_paths(&[
        append_units(&executable_dir, r"\Modules"),
        ascii_units(r"C:\Program Files\PowerShell\Modules"),
        ascii_units(r"C:\Program Files\WindowsPowerShell\Modules"),
        append_units(&system32, r"\WindowsPowerShell\v1.0\Modules"),
    ]);
    let path = join_windows_paths(&[
        executable_dir,
        system32.clone(),
        system_root,
        append_units(&system32, r"\Wbem"),
        append_units(&system32, r"\WindowsPowerShell\v1.0"),
    ]);
    set_env_units(&mut environment, "PSModulePath", module_paths.clone());
    set_env_units(&mut environment, POWERSHELL_MODULE_PATH_ENV, module_paths);
    set_env_units(&mut environment, "PATH", path);
    set_env(
        &mut environment,
        "PATHEXT",
        ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC",
    );
    set_env(&mut environment, "COMSPEC", r"C:\Windows\System32\cmd.exe");
    set_env(&mut environment, "TERM", "dumb");
    if let Some(path) = invocation.ps1_path.as_ref() {
        set_env_units(&mut environment, POWERSHELL_SCRIPT_PATH_ENV, path.clone());
    }
    environment
}

fn append_units(prefix: &[u16], suffix: &str) -> Vec<u16> {
    let mut result = prefix.to_vec();
    result.extend(suffix.encode_utf16());
    result
}

fn join_windows_paths(paths: &[Vec<u16>]) -> Vec<u16> {
    let mut joined = Vec::new();
    for path in paths {
        if !joined.is_empty() {
            joined.push(u16::from(b';'));
        }
        joined.extend(path);
    }
    joined
}

fn set_env_units(environment: &mut BTreeMap<Vec<u16>, Vec<u16>>, key: &str, value: Vec<u16>) {
    environment.insert(ascii_units(key), value);
}

fn current_environment() -> BTreeMap<Vec<u16>, Vec<u16>> {
    std::env::vars_os()
        .map(|(key, value)| (os_str_units(&key), os_str_units(&value)))
        .collect()
}

fn set_env(environment: &mut BTreeMap<Vec<u16>, Vec<u16>>, key: &str, value: &str) {
    environment.insert(ascii_units(key), value.encode_utf16().collect());
}

fn parent_units(path: &[u16]) -> Vec<u16> {
    let Some(separator) = path
        .iter()
        .rposition(|unit| *unit == u16::from(b'\\') || *unit == u16::from(b'/'))
    else {
        return ascii_units(".");
    };
    let end = if separator == 2 && path.get(1) == Some(&u16::from(b':')) {
        separator + 1
    } else if separator == 0 {
        1
    } else {
        separator
    };
    path[..end].to_vec()
}

fn ascii_units(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

fn os_string(units: &[u16]) -> OsString {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(units)
    }
    #[cfg(not(windows))]
    {
        OsString::from(utf16_to_native_string(units))
    }
}

fn os_str_units(value: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        value.encode_wide().collect()
    }
    #[cfg(not(windows))]
    {
        value.to_string_lossy().encode_utf16().collect()
    }
}

fn utf16_to_native_string(units: &[u16]) -> String {
    // The pinned Bun 1.3.11 probe receipt
    // `policy-helper-utf16-boundary-probe-bun-1.3.11/receipt.json` observed
    // U+D800 becoming UTF-8 `ef bf bd` in spawn path, argv, and stdin bytes.
    String::from_utf16_lossy(&units)
}

/// Run one helper with Native's retry count and retryable-code predicate.
pub(super) async fn execute_entry(
    entry: &NativePolicyHelperEntry,
    platform: NativePolicyPlatform,
    runner: &dyn ProcessRunner,
) -> Result<NativePolicyHelperOutput, ExecutionFailure> {
    let max_retries = if entry.from_per_os {
        entry.retries.min(5)
    } else {
        0
    };
    for attempt in 1..=max_retries.saturating_add(1) {
        let invocation = match invocation_for_entry(entry, platform) {
            Ok(invocation) => invocation,
            Err(failure) => {
                if attempt <= max_retries && failure.code.retryable() {
                    tokio::time::sleep(retry_delay(attempt - 1)).await;
                    continue;
                }
                return Err(ExecutionFailure {
                    code: failure.code,
                    diagnostic: native_failure_diagnostic(&failure, attempt),
                    field_path: Some(helper_field_path(entry)),
                    attempts: attempt,
                });
            }
        };
        let mut invocation = invocation;
        if invocation.routed_interpreter.is_none() {
            if let Err(failure) = preflight_direct_helper(&mut invocation).await {
                if attempt <= max_retries && failure.code.retryable() {
                    tokio::time::sleep(retry_delay(attempt - 1)).await;
                    continue;
                }
                return Err(ExecutionFailure {
                    code: failure.code,
                    diagnostic: native_failure_diagnostic(&failure, attempt),
                    field_path: Some(helper_field_path(entry)),
                    attempts: attempt,
                });
            }
        }
        let output = match runner.run(invocation).await {
            Ok(output) => output,
            Err(failure) => {
                if attempt > max_retries || !failure.code.retryable() {
                    return Err(ExecutionFailure {
                        code: failure.code,
                        diagnostic: native_failure_diagnostic(&failure, attempt),
                        field_path: Some(helper_field_path(entry)),
                        attempts: attempt,
                    });
                }
                tokio::time::sleep(retry_delay(attempt - 1)).await;
                continue;
            }
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.encode_utf16().count() > OUTPUT_LIMIT {
            return Err(ExecutionFailure {
                code: FailureCode::Oversize,
                diagnostic: Some(native_helper_error_projection(
                    attempt,
                    &format!("stdout exceeded {OUTPUT_LIMIT} bytes"),
                )),
                field_path: Some(helper_field_path(entry)),
                attempts: attempt,
            });
        }
        if output.exit_code != Some(0) {
            let detail = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            let message = format!(
                "exited with code {}: {}",
                output
                    .exit_code
                    .map_or_else(|| "1".into(), |code| code.to_string()),
                String::from_utf8_lossy(detail)
            );
            if attempt <= max_retries {
                tokio::time::sleep(retry_delay(attempt - 1)).await;
                continue;
            }
            return Err(ExecutionFailure {
                code: FailureCode::ExitNonzero,
                diagnostic: Some(native_helper_error_projection(attempt, &message)),
                field_path: Some(helper_field_path(entry)),
                attempts: attempt,
            });
        }
        let raw = Utf16JsonProjection::parse(&stdout).map_err(|_| ExecutionFailure {
            code: FailureCode::ParseFailed,
            diagnostic: Some(native_helper_error_projection(
                attempt,
                "stdout is not a JSON object",
            )),
            field_path: Some(helper_field_path(entry)),
            attempts: attempt,
        })?;
        return parse_native_policy_helper_output(&raw).map_err(|error| {
            let (code, diagnostic, field_path) = match error {
                lingxi_core::settings::NativePolicyHelperOutputError::NonObjectRoot {
                    diagnostic,
                } => {
                    let detail = diagnostic.string_at("").map(str::to_owned);
                    (
                        FailureCode::ParseFailed,
                        detail.map(|detail| native_helper_error_projection(attempt, &detail)),
                        Some(helper_field_path(entry)),
                    )
                }
                lingxi_core::settings::NativePolicyHelperOutputError::InvalidEnvelope {
                    diagnostic,
                } => {
                    let detail = diagnostic
                        .string_at("")
                        .map(|message| format!("invalid envelope: {message}"));
                    (
                        FailureCode::EnvelopeInvalid,
                        detail.map(|detail| native_helper_error_projection(attempt, &detail)),
                        Some(helper_field_path(entry)),
                    )
                }
                lingxi_core::settings::NativePolicyHelperOutputError::ProjectionUnavailable {
                    field,
                } => (FailureCode::EnvelopeInvalid, None, Some(field.to_owned())),
            };
            ExecutionFailure {
                code,
                diagnostic,
                field_path,
                attempts: attempt,
            }
        });
    }
    unreachable!("at least one helper attempt is always performed")
}

async fn preflight_direct_helper(invocation: &mut Invocation) -> Result<(), ProcessFailure> {
    let started = Instant::now();
    let file = os_string(&invocation.file);
    let result =
        tokio::time::timeout(invocation.timeout, tokio::fs::metadata(PathBuf::from(file))).await;
    match result {
        Err(_) => {
            return Err(ProcessFailure {
                code: FailureCode::NotAFile,
                detail: format!(
                    "cannot stat path (timed out after {} ms): {}",
                    invocation.total_timeout.as_millis(),
                    utf16_to_native_string(&invocation.file)
                ),
            });
        }
        Ok(Err(error)) => {
            return Err(ProcessFailure {
                code: FailureCode::NotAFile,
                detail: format!(
                    "cannot stat path ({}): {}",
                    native_fs_error_code(&error).unwrap_or("unknown error"),
                    utf16_to_native_string(&invocation.file)
                ),
            });
        }
        Ok(Ok(metadata)) if !metadata.is_file() => {
            return Err(ProcessFailure {
                code: FailureCode::NotAFile,
                detail: format!(
                    "path is not a regular file: {}",
                    utf16_to_native_string(&invocation.file)
                ),
            });
        }
        Ok(Ok(_)) => {}
    }
    invocation.timeout = invocation.timeout.saturating_sub(started.elapsed());
    if invocation.timeout.is_zero() {
        return Err(ProcessFailure {
            code: FailureCode::NotAFile,
            detail: format!(
                "cannot stat path (timed out after {} ms): {}",
                invocation.total_timeout.as_millis(),
                utf16_to_native_string(&invocation.file)
            ),
        });
    }
    Ok(())
}

fn native_fs_error_code(error: &std::io::Error) -> Option<&'static str> {
    use std::io::ErrorKind;
    Some(match error.kind() {
        ErrorKind::NotFound => "ENOENT",
        ErrorKind::PermissionDenied => "EACCES",
        ErrorKind::AlreadyExists => "EEXIST",
        ErrorKind::InvalidInput | ErrorKind::InvalidData => "EINVAL",
        ErrorKind::TimedOut => "ETIMEDOUT",
        ErrorKind::Interrupted => "EINTR",
        ErrorKind::WouldBlock => "EAGAIN",
        ErrorKind::BrokenPipe => "EPIPE",
        ErrorKind::UnexpectedEof => "EIO",
        _ => return None,
    })
}

fn retry_delay(attempt: u8) -> Duration {
    let upper = 500_u64
        .saturating_mul(
            1_u64
                .checked_shl(u32::from(attempt.min(31)))
                .unwrap_or(u64::MAX),
        )
        .min(4_000);
    let lower = 250_u64.min(upper);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    let unit = f64::from(nanos) / 1_000_000_000.0;
    Duration::from_millis(lower + (unit * (upper - lower) as f64).floor() as u64)
}

fn diagnostic(message: &str) -> Utf16JsonProjection {
    let units = message.encode_utf16().collect::<Vec<_>>();
    Utf16JsonProjection::root_string(message.to_owned(), units)
        .expect("process diagnostic is valid UTF-16")
}

fn prefix_source_projection(
    prefix: &str,
    source: &Utf16JsonProjection,
) -> Option<Utf16JsonProjection> {
    let source_units = source.string_units("")?;
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(source_units);
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units).ok()
}

fn helper_field_path(entry: &NativePolicyHelperEntry) -> String {
    if entry.from_per_os {
        format!("policyHelpers.{}", entry.entry_key)
    } else {
        "policyHelper".to_owned()
    }
}

fn apply_status_error(
    status: NativePolicyHelperApplyStatus,
) -> (Option<Utf16JsonProjection>, Option<String>) {
    match status {
        NativePolicyHelperApplyStatus::Rejected { diagnostic, .. } => {
            (diagnostic, Some("policyHelper".to_owned()))
        }
        NativePolicyHelperApplyStatus::Unresolved { fields } => (
            None,
            fields
                .first()
                .cloned()
                .or_else(|| Some("policyHelper".to_owned())),
        ),
        NativePolicyHelperApplyStatus::StartupFatal { diagnostics } => diagnostics
            .first()
            .map(|fatal| (fatal.diagnostic.clone(), Some(fatal.path.clone())))
            .unwrap_or_else(|| (None, Some("policyHelper".to_owned()))),
        NativePolicyHelperApplyStatus::SourceMismatch
        | NativePolicyHelperApplyStatus::Applied
        | NativePolicyHelperApplyStatus::NoManagedSettings => {
            (None, Some("policyHelper".to_owned()))
        }
    }
}

fn native_failure_diagnostic(
    failure: &ProcessFailure,
    attempts: u8,
) -> Option<Utf16JsonProjection> {
    // The direct runner can also surface its own pipe/OS failure details. The
    // Native message is bound for process-result failures, but those local
    // runtime diagnostics have no source text and must remain path-only.
    if failure.code == FailureCode::ExitNonzero && !failure.detail.starts_with("exited with code ")
    {
        return None;
    }
    Some(native_helper_error_projection(attempts, &failure.detail))
}

/// Native `Xi()` over `zd(message)`, followed by its attempts prefix.
/// Source: `src_188306029.js` Xi / It; imported `zd` uniquely binds to
/// `src_188304227.js` (SHA `7faa42e8fa26439d0f59d1f4b54a1a02c59a0ab4858f3d1378b49f64fea98213`),
/// and `ne` to `src_186231195.js` (SHA
/// `125f57566b99b67ce958d77cd787163124654cbbb3bdd6f3a92dfd38955a2317`).
fn native_helper_error_projection(attempts: u8, message: &str) -> Utf16JsonProjection {
    let sanitized = native_helper_error_text(message);
    let units = sanitized.encode_utf16().collect::<Vec<_>>();
    let rendered = if units.len() <= 2_048 {
        sanitized
    } else {
        let mut end = 2_048;
        if units
            .get(end - 1)
            .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
        {
            end -= 1;
        }
        let prefix = String::from_utf16(&units[..end])
            .expect("Native ne() never leaves a trailing high surrogate");
        format!(
            "{prefix}… (+{} chars not shown; the full output is in the debug log)",
            units.len() - end
        )
    };
    let rendered = if attempts > 1 {
        format!("after {attempts} attempts: {rendered}")
    } else {
        rendered
    };
    diagnostic(&rendered)
}

fn native_helper_error_text(message: &str) -> String {
    // `zd` first removes VT control sequences and then removes Cc/Cf except
    // TAB/LF, plus U+2028/U+2029. Rust strings cannot contain unpaired UTF-16
    // surrogates, so the source's preceding Lw() is already satisfied.
    lingxi_core::settings::native_policy::sanitize_native_policy_helper_error_text(message)
}

fn execution_failure_display(failure: &ExecutionFailure) -> String {
    failure
        .diagnostic
        .as_ref()
        .and_then(|diagnostic| diagnostic.string_units(""))
        .map(|units| String::from_utf16_lossy(&units))
        .or_else(|| failure.field_path.clone())
        .unwrap_or_default()
}

/// Refuse all helper execution while a Native startup-fatal diagnostic exists
/// or any static fallback schema remains unresolved. Native preflights these
/// candidates before it invokes a process.
pub(super) fn executable_plan(selection: &NativePolicySelection) -> Result<(), Vec<String>> {
    if !selection.startup_fatal_helper_paths.is_empty() {
        return Err(selection
            .startup_fatal_helper_paths
            .iter()
            .map(|fatal| fatal.path.clone())
            .collect());
    }
    let Some(plan) = selection.helper_plan.as_ref() else {
        return Err(vec!["helper plan is absent".into()]);
    };
    if let NativePolicyHelperPlanValidation::Unresolved { fields } = &plan.validation {
        return Err(fields.clone());
    }
    if let Some(candidate) = plan
        .static_fallback_candidates
        .iter()
        .find(|candidate| candidate.validation != NativePolicyStaticFallbackValidation::Validated)
    {
        return Err(vec![candidate.source_field.clone()]);
    }
    Ok(())
}

/// Return the Native-selected static fallback only after full source-backed
/// validation has been represented by Core.
pub(super) async fn log_first_on_chain_static_warnings(
    semantic_worker: &SemanticWorkerState,
    selection: &NativePolicySelection,
    logger: &dyn NativePolicyWarningLogger,
) {
    let Some(candidate) = selection.helper_plan.as_ref().and_then(|plan| {
        plan.static_fallback_candidates
            .iter()
            .find(|candidate| candidate.on_chain)
    }) else {
        return;
    };
    if candidate.validation != NativePolicyStaticFallbackValidation::Validated
        || !candidate.warning_unresolved_fields.is_empty()
    {
        return;
    }
    let Some(messages) = lingxi_core::settings::native_policy::native_policy_qn_log_messages(
        &candidate.settings,
        &candidate.warnings,
        &candidate.source_field,
    ) else {
        return;
    };
    emit_native_policy_warning_messages(semantic_worker, logger, &messages).await;
}

pub(super) fn selected_static_fallback(
    selection: &NativePolicySelection,
) -> Option<(
    &str,
    &Utf16JsonProjection,
    lingxi_core::settings::NativePolicyHelperOutputBehavior,
)> {
    let plan = selection.helper_plan.as_ref()?;
    if matches!(
        plan.validation,
        NativePolicyHelperPlanValidation::Unresolved { .. }
    ) || plan
        .static_fallback_candidates
        .iter()
        .any(|candidate| candidate.validation != NativePolicyStaticFallbackValidation::Validated)
    {
        return None;
    }
    let field = plan.selected_static_fallback_field.as_deref()?;
    let candidate = plan
        .static_fallback_candidates
        .iter()
        .find(|candidate| candidate.source_field == field)?;
    let normalized = candidate.normalized_settings.as_ref()?;
    let behavior = if candidate.merges_output {
        lingxi_core::settings::NativePolicyHelperOutputBehavior::Merge
    } else {
        lingxi_core::settings::NativePolicyHelperOutputBehavior::Replace
    };
    Some((&candidate.source_field, normalized, behavior))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::settings::native_policy::{
        NativePolicyHelperOnFailure, NativePolicyHelperOutputBehavior,
    };
    use std::sync::atomic::AtomicUsize;

    struct CountingRunner(Arc<AtomicUsize>);

    #[tokio::test]
    async fn source_without_js_semantics_does_not_require_bun_worker() {
        let state: SemanticWorkerState = Arc::new(std::sync::Mutex::new(None));
        let evidence = semantic_evidence_for_inputs(&state, &NativePolicySemanticInputs::default())
            .await
            .expect("empty source has no JavaScript operation to preflight");
        assert!(evidence.regex_rows.is_empty());
        assert!(evidence.js.url_rows().is_empty());
        assert!(evidence.js.display_rows().is_empty());
        assert!(state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none());
    }

    fn parser_error(path: &str) -> Arc<PolicyHelperStartupError> {
        Arc::new(PolicyHelperStartupError::new(NativePolicyStartupFatal {
            source: NativePolicySource::ManagedFile,
            source_paths: Vec::new(),
            path: path.to_owned(),
            diagnostic: None,
        }))
    }

    #[derive(Default)]
    struct RecordingWarningLogger(std::sync::Mutex<Vec<Vec<u16>>>);

    impl NativePolicyWarningLogger for RecordingWarningLogger {
        fn warning_enabled(&self) -> bool {
            true
        }

        fn filter_source_units(&self) -> Option<Vec<u16>> {
            None
        }

        fn write_rendered_warning(&self, line_units: &[u16]) {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(line_units.to_vec());
        }
    }

    #[tokio::test]
    async fn initial_static_fallback_sends_exact_on_chain_qn_messages_to_host_sink() {
        let root = tempfile::tempdir().unwrap();
        let managed_dir = root.path().join("managed");
        std::fs::create_dir_all(&managed_dir).unwrap();
        let os_key = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "linux"
        };
        let mut policy_helpers = serde_json::Map::new();
        policy_helpers.insert(
            os_key.to_owned(),
            serde_json::json!({
                "defaultSettings": {
                    "allowedMarketplaces": [],
                    "strictKnownMarketplaces": []
                }
            }),
        );
        let source = serde_json::json!({
            "model": "base-model",
            "policyHelpers": policy_helpers
        });
        std::fs::write(
            managed_dir.join("managed-settings.json"),
            serde_json::to_vec(&source).unwrap(),
        )
        .unwrap();

        let cache = super::super::settings_watch::ManagedPolicySnapshotCache::default();
        let initial =
            super::super::settings_watch::read_managed_settings_snapshot(&managed_dir).await;
        assert_eq!(
            initial.selection.selected_admin,
            Some(NativePolicySource::ManagedFile)
        );
        let selection = initial.selection.clone();
        cache.replace(initial);
        let logger = Arc::new(RecordingWarningLogger::default());
        let controller = Arc::new(NativePolicyHelperController::new(
            cache,
            Arc::new(DirectProcessRunner),
            Some(logger.clone()),
        ));

        controller
            .run_initial_local_helper(&selection)
            .await
            .expect("the source-only static fallback needs no process runner");

        let messages = logger
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(messages.len(), 1);
        let message = String::from_utf16(&messages[0])
            .expect("formatted warning is valid UTF-16 at the logger boundary");
        let (timestamp, _) = message
            .split_once(" [WARN] ")
            .expect("Native warning includes its UTC timestamp and level");
        assert_eq!(timestamp.len(), 24);
        assert!(timestamp.ends_with('Z'));
        chrono::DateTime::parse_from_rfc3339(timestamp)
            .expect("Native timestamp is a valid ISO UTC instant");
        assert!(message.contains(&format!("policyHelpers.{os_key}.defaultSettings:")));
        assert!(message.contains("allowedMarketplaces"));
        assert!(message.ends_with('\n'));
    }

    #[tokio::test]
    async fn semantic_worker_failure_is_source_scoped_and_clears_on_source_change() {
        let root = tempfile::tempdir().unwrap();
        let managed_dir = root.path().join("managed");
        let empty_dir = root.path().join("empty");
        std::fs::create_dir_all(&managed_dir).unwrap();
        std::fs::create_dir_all(&empty_dir).unwrap();
        std::fs::write(
            managed_dir.join("managed-settings.json"),
            r#"{"model":"managed"}"#,
        )
        .unwrap();

        let cache = super::super::settings_watch::ManagedPolicySnapshotCache::default();
        let initial =
            super::super::settings_watch::read_managed_settings_snapshot(&managed_dir).await;
        assert_eq!(
            initial.selection.selected_admin,
            Some(NativePolicySource::ManagedFile)
        );
        cache.replace(initial);
        let controller = Arc::new(NativePolicyHelperController::new(
            cache.clone(),
            Arc::new(DirectProcessRunner),
            None,
        ));
        let snapshot = cache.snapshot().unwrap();
        let mut probe = controller.semantic_worker_failure_probe();
        let waiter = tokio::spawn(async move { probe.wait().await });
        controller.retain_semantic_worker_failure(
            NativePolicySource::ManagedFile,
            &snapshot.selection,
            NativePolicyRegexWorkerError::VersionMismatch {
                expected: "1.4.3@expected".to_owned(),
                actual: "1.3.11@actual".to_owned(),
            },
            0,
        );
        let retained = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("worker capability failure must wake subscribers")
            .unwrap();
        assert_eq!(retained.source(), NativePolicySource::ManagedFile);
        assert_eq!(retained.generation(), 0);
        assert!(matches!(
            retained.worker_error(),
            NativePolicyRegexWorkerError::VersionMismatch { .. }
        ));
        assert_eq!(cache.semantic_worker_failure(), Some((*retained).clone()));

        controller.clear_semantic_worker_failure_after_success(NativePolicySource::ManagedFile, 0);
        assert!(cache.semantic_worker_failure().is_none());
        assert!(controller
            .semantic_worker_failure_probe()
            .current()
            .is_none());

        controller.retain_semantic_worker_failure(
            NativePolicySource::ManagedFile,
            &snapshot.selection,
            NativePolicyRegexWorkerError::VersionMismatch {
                expected: "1.4.3@expected".to_owned(),
                actual: "1.3.11@actual".to_owned(),
            },
            0,
        );
        controller
            .generation
            .store(1, std::sync::atomic::Ordering::Release);
        controller.retain_semantic_worker_failure(
            NativePolicySource::ManagedFile,
            &snapshot.selection,
            NativePolicyRegexWorkerError::VersionMismatch {
                expected: "1.4.3@expected".to_owned(),
                actual: "1.3.11@actual".to_owned(),
            },
            1,
        );
        assert_eq!(
            cache
                .semantic_worker_failure()
                .map(|failure| failure.generation()),
            Some(1)
        );
        controller.clear_semantic_worker_failure_after_success(NativePolicySource::ManagedFile, 0);
        assert_eq!(
            cache
                .semantic_worker_failure()
                .map(|failure| failure.generation()),
            Some(1),
            "an older successful validation cannot clear a newer generation's failure"
        );
        controller.clear_semantic_worker_failure_after_success(NativePolicySource::ManagedFile, 1);
        assert!(cache.semantic_worker_failure().is_none());

        let changed =
            super::super::settings_watch::read_managed_settings_snapshot(&empty_dir).await;
        let (changed, source_changed) = cache.replace_from_watch(changed);
        assert!(source_changed);
        controller.on_admin_source_change(changed);
        assert!(cache.semantic_worker_failure().is_none());
        assert!(controller
            .semantic_worker_failure_probe()
            .current()
            .is_none());
    }

    #[tokio::test]
    async fn refusal_probe_wakes_and_keeps_the_first_terminal_cause() {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let probe = PolicyHelperRefusalProbe {
            receiver,
            #[cfg(any(test, feature = "test-support"))]
            test_sender: sender.clone(),
        };
        let mut waiter = probe.clone();
        let waiting = tokio::spawn(async move { waiter.wait().await });
        tokio::task::yield_now().await;

        let first = parser_error("policyHelpers.linux");
        assert!(retain_first_refusal(&sender, Arc::clone(&first)));
        let woken = tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .expect("terminal refusal must wake an existing waiter")
            .unwrap();
        assert!(Arc::ptr_eq(&first, &woken));
        assert!(Arc::ptr_eq(&first, &probe.current().unwrap()));

        let second = parser_error("policyHelpers.windows");
        assert!(!retain_first_refusal(&sender, second));
        assert!(Arc::ptr_eq(&first, &probe.current().unwrap()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn selected_live_refusal_wakes_the_controller_probe_and_stale_refusal_is_ignored() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let baseline_dir = root.path().join("baseline");
        let managed_dir = root.path().join("managed");
        std::fs::create_dir_all(&baseline_dir).unwrap();
        std::fs::create_dir_all(&managed_dir).unwrap();

        let helper_path = root.path().join("refuse.sh");
        std::fs::write(&helper_path, "#!/bin/sh\necho denied >&2\nexit 9\n").unwrap();
        let mut permissions = std::fs::metadata(&helper_path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&helper_path, permissions).unwrap();
        let helper = serde_json::json!({
            "path": helper_path.to_string_lossy(),
            "onFailure": "refuse"
        });
        std::fs::write(
            managed_dir.join("managed-settings.json"),
            serde_json::to_vec(&serde_json::json!({
                "model": "base",
                "policyHelpers": {
                    "macos": helper.clone(),
                    "linux": helper.clone(),
                    "windows": helper.clone(),
                    "wsl": helper
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let cache = super::super::settings_watch::ManagedPolicySnapshotCache::default();
        cache.replace(
            super::super::settings_watch::read_managed_settings_snapshot(&baseline_dir).await,
        );
        let changed =
            super::super::settings_watch::read_managed_settings_snapshot(&managed_dir).await;
        let (changed, source_changed) = cache.replace_from_watch(changed);
        assert!(source_changed);
        assert_eq!(
            changed.selection.selected_admin,
            Some(NativePolicySource::ManagedFile)
        );

        let controller = Arc::new(NativePolicyHelperController::new(
            cache.clone(),
            Arc::new(DirectProcessRunner),
            None,
        ));
        let mut probe = controller.refusal_probe();
        controller.on_admin_source_change(changed);
        let refusal = tokio::time::timeout(Duration::from_secs(5), probe.wait())
            .await
            .expect("selected live refusal must wake controller subscribers");
        assert_eq!(refusal.source(), NativePolicySource::ManagedFile);
        assert_eq!(
            cache
                .native_helper_failure()
                .and_then(|failure| failure.on_failure),
            Some(NativePolicyHelperOnFailure::Refuse)
        );

        let stale_dir = root.path().join("stale");
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(
            stale_dir.join("managed-settings.json"),
            r#"{"model":"base"}"#,
        )
        .unwrap();
        let stale_cache = super::super::settings_watch::ManagedPolicySnapshotCache::default();
        stale_cache.replace(
            super::super::settings_watch::read_managed_settings_snapshot(&stale_dir).await,
        );
        assert_eq!(
            stale_cache
                .snapshot()
                .and_then(|snapshot| snapshot.selection.selected_admin),
            Some(NativePolicySource::ManagedFile)
        );
        let stale_controller = NativePolicyHelperController::new(
            stale_cache.clone(),
            Arc::new(DirectProcessRunner),
            None,
        );
        let stale_probe = stale_controller.refusal_probe();
        let stale_failure = super::super::settings_watch::NativePolicyHelperFailure {
            source: NativePolicySource::ManagedFile,
            source_paths: vec![stale_dir.join("managed-settings.json")],
            diagnostic: None,
            field_path: Some("policyHelpers.linux".into()),
            code: FailureCode::ExitNonzero,
            on_failure: Some(NativePolicyHelperOnFailure::Refuse),
            attempts: 1,
            generation: 7,
            selected_admin_at_failure: Some(NativePolicySource::ManagedFile),
        };
        assert!(stale_cache.record_native_helper_failure(stale_failure));
        stale_controller.notify_applied();
        assert!(stale_probe.current().is_none());
    }

    #[async_trait]
    impl ProcessRunner for CountingRunner {
        async fn run(&self, _invocation: Invocation) -> Result<ProcessOutput, ProcessFailure> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(ProcessOutput {
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
            })
        }
    }

    #[test]
    fn inline_sh_route_preserves_script_and_uses_native_environment_filter() {
        let config =
            Utf16JsonProjection::parse(r#"{"script":"printf '{}'\n","interpreter":"sh"}"#).unwrap();
        let entry = NativePolicyHelperEntry {
            entry_key: "linux".into(),
            config,
            from_per_os: true,
            on_failure: NativePolicyHelperOnFailure::Refuse,
            timeout_ms: 10_000,
            retries: 0,
            refresh_interval_ms: 0,
            output_behavior: NativePolicyHelperOutputBehavior::Replace,
        };
        let invocation = invocation_for_entry(&entry, NativePolicyPlatform::Linux).unwrap();
        assert_eq!(invocation.file, ascii_units("/bin/sh"));
        assert_eq!(invocation.args, vec![ascii_units("-s")]);
        assert_eq!(
            invocation.stdin,
            Some("printf '{}'\n".encode_utf16().collect())
        );
        assert_eq!(invocation.cwd, ascii_units("/"));
        assert_eq!(
            invocation.env.get(&ascii_units("PATH")),
            Some(&ascii_units("/usr/bin:/bin:/usr/sbin:/sbin"))
        );
        assert_eq!(
            invocation.env.get(&ascii_units("LC_ALL")),
            Some(&ascii_units("C"))
        );
        assert_eq!(
            invocation.env.get(&ascii_units("CLAUDE_CODE_VERSION")),
            Some(&ascii_units(HELPER_VERSION))
        );
    }

    #[test]
    fn retry_delay_stays_within_native_range() {
        for attempt in 0..=8 {
            assert!((250..=4_000).contains(&(retry_delay(attempt).as_millis() as u64)));
        }
    }

    #[cfg(unix)]
    fn shell_process(script: &str) -> Invocation {
        Invocation {
            file: ascii_units("/bin/sh"),
            args: vec![ascii_units("-c"), script.encode_utf16().collect()],
            stdin: None,
            cwd: ascii_units("/"),
            env: BTreeMap::from([(ascii_units("PATH"), ascii_units("/usr/bin:/bin"))]),
            timeout: Duration::from_secs(10),
            total_timeout: Duration::from_secs(10),
            routed_interpreter: None,
            ps1_path: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn process_buffer_limits_each_stream_instead_of_combining_bytes() {
        let invocation = shell_process(
            r"/usr/bin/head -c 600000 /dev/zero | /usr/bin/tr '\000' a; /usr/bin/head -c 600000 /dev/zero | /usr/bin/tr '\000' b >&2",
        );
        let output = run_blocking(invocation, Arc::new(AtomicBool::new(false))).unwrap();

        assert_eq!(output.stdout, vec![b'a'; 600_000]);
        assert_eq!(output.stderr, vec![b'b'; 600_000]);
    }

    #[cfg(unix)]
    #[test]
    fn process_buffer_limit_counts_decoded_utf16_units_not_utf8_bytes() {
        let invocation = shell_process("/usr/bin/yes '😀' | /usr/bin/head -c 1200000");
        let output = run_blocking(invocation, Arc::new(AtomicBool::new(false))).unwrap();
        let decoded = String::from_utf8_lossy(&output.stdout);

        assert!(output.stdout.len() > OUTPUT_LIMIT);
        assert!(decoded.encode_utf16().count() <= OUTPUT_LIMIT);
    }

    #[cfg(unix)]
    #[test]
    fn process_buffer_limit_rejects_a_single_stream_over_the_native_unit_cap() {
        let invocation =
            shell_process(r"/usr/bin/head -c 1100000 /dev/zero | /usr/bin/tr '\000' a");
        let failure = run_blocking(invocation, Arc::new(AtomicBool::new(false))).unwrap_err();

        assert_eq!(failure.code, FailureCode::Oversize);
        assert_eq!(
            failure.detail,
            format!("stdout exceeded {OUTPUT_LIMIT} bytes")
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_deadline_covers_pipe_eof_after_leader_exit() {
        let invocation = Invocation {
            file: ascii_units("/bin/sh"),
            args: vec![ascii_units("-c"), ascii_units("sleep 0.3 & exit 0")],
            stdin: None,
            cwd: ascii_units("/"),
            env: BTreeMap::from([(ascii_units("PATH"), ascii_units("/usr/bin:/bin"))]),
            timeout: Duration::from_millis(40),
            total_timeout: Duration::from_millis(40),
            routed_interpreter: None,
            ps1_path: None,
        };
        let failure = run_blocking(invocation, Arc::new(AtomicBool::new(false))).unwrap_err();

        assert_eq!(failure.code, FailureCode::TimedOut);
    }

    #[test]
    fn helper_output_parser_keeps_lone_surrogate_units() {
        let raw = Utf16JsonProjection::parse(r#"{"claudeMd":"\ud800"}"#).unwrap();
        let parsed = parse_native_policy_helper_output(&raw).unwrap();
        assert_eq!(
            parsed.claude_md.unwrap().string_units(""),
            Some(vec![0xd800])
        );
    }

    #[test]
    fn native_error_wrapper_uses_utf16_limit_and_attempt_prefix() {
        let raw = format!("{}😀z", "a".repeat(2_047));
        let projected = native_helper_error_projection(2, &raw);
        assert_eq!(
            projected.string_at(""),
            Some(
                format!(
                    "after 2 attempts: {}… (+3 chars not shown; the full output is in the debug log)",
                    "a".repeat(2_047)
                )
                .as_str()
            )
        );
    }

    #[test]
    fn native_error_cleaning_removes_vt_and_format_controls() {
        assert_eq!(
            native_helper_error_text("\u{1b}[31mred\u{1b}[0m\u{200b}ok\u{2028}end"),
            "redokend"
        );
        assert_eq!(
            native_fs_error_code(&std::io::Error::from(std::io::ErrorKind::NotFound)),
            Some("ENOENT")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_process_path_and_argv_projection_match_bun_lone_surrogate_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let units = [0xd800];
        let projected = os_string(&units);
        assert_eq!(projected.as_os_str().as_bytes(), b"\xef\xbf\xbd");
        assert_eq!(utf16_to_native_string(&units).as_bytes(), b"\xef\xbf\xbd");
    }

    #[tokio::test]
    async fn direct_helper_stat_failure_precedes_the_process_runner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing-policy-helper");
        let projection = Utf16JsonProjection::object(vec![(
            "path".into(),
            Utf16JsonProjection::root_string(
                path.to_string_lossy().into_owned(),
                path.to_string_lossy().encode_utf16().collect(),
            )
            .unwrap(),
        )])
        .unwrap();
        let entry = NativePolicyHelperEntry {
            entry_key: "linux".into(),
            config: projection,
            from_per_os: true,
            on_failure: NativePolicyHelperOnFailure::Refuse,
            timeout_ms: 1_000,
            retries: 0,
            refresh_interval_ms: 0,
            output_behavior: NativePolicyHelperOutputBehavior::Replace,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let result = execute_entry(
            &entry,
            NativePolicyPlatform::Linux,
            &CountingRunner(Arc::clone(&calls)),
        )
        .await;
        let failure = result.unwrap_err();
        assert_eq!(failure.code, FailureCode::NotAFile);
        assert_eq!(failure.attempts, 1);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
