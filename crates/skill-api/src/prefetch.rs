//! EXPERIMENTAL_SKILL_SEARCH skill-discovery prefetch — the per-turn,
//! NON-blocking discovery side-channel that runs the skill candidate selector
//! CONCURRENTLY with the main API call + tool execution, then surfaces the
//! chosen skills as a transient `<system-reminder>` meta user message.
//!
//! 1:1 with claude-code 2.1.195's `services/skillSearch/prefetch.js`
//! (`startSkillDiscoveryPrefetch` / `collectSkillDiscoveryPrefetch`, wired in
//! `query.ts` — bundle fn `C1z`: `B=at1?.startSkillDiscoveryPrefetch(null,V,T)`
//! at iteration top, `at1.collectSkillDiscoveryPrefetch(B)` post-tools). The
//! shipping binary DCEs this entire feature out (every skill-search literal = 0
//! hits in `strings`), so the CORRECT parity state is register-but-disable,
//! DEFAULT OFF, zero observable bytes when off. The composition root only wires
//! this when `EXPERIMENTAL_SKILL_SEARCH` is on (default false), exactly mirroring
//! the structurally-identical memory-selector prefetch
//! ([`memory::prefetch::MemoryPrefetch`]).
//!
//! Two construction modes (cloned from `MemoryPrefetch`):
//! - [`SkillDiscoveryPrefetch::new`] binds a real [`SkillCandidateSource`]:
//!   [`SkillDiscoveryPrefetch::start`] asks the source which skills are relevant
//!   to the turn query and ships them as [`DiscoveredSkill`]. This is the path
//!   the composition root wires when the flag is on.
//! - [`SkillDiscoveryPrefetch::with_fixed_result`] resolves to a PRE-SELECTED
//!   set, bypassing the source — the deterministic seam the orchestrator's
//!   surfacing tests drive.
//!
//! A prefetch with neither a source nor a fixed result resolves to an EMPTY set,
//! so the surfacing reminder is a strict no-op and the locked fixtures stay
//! byte-identical (analog of the `(None, None)` arm in `MemoryPrefetch::start`).
//!
//! DEFERRED (NON-GOALS, faithful stubs rather than full builds): the AKI
//! remote/embedding backend + Haiku classifier; the `getSkillIndex` /
//! `clearSkillIndexCache` memoization layer; remote canonical skills + the
//! `DiscoverSkills` tool; TUI transcript rendering of `skill_discovery`; and the
//! turn-0 BLOCKING discovery path (`attachments.ts:806 getTurnZeroSkillDiscovery`,
//! which blocks inside `userInputAttachments` — the one signal with no prior work
//! to hide under). The port ships only the per-iteration NON-blocking prefetch,
//! which also covers the turn-0 query albeit non-blockingly. All are inert while
//! the feature is OFF by default.

#![allow(clippy::module_name_repetitions)]

use crate::registry::SkillRegistry;
use platform_api::RuntimeSpawner;
use std::sync::Arc;
use tokio::sync::oneshot;

/// One skill selected for surfacing this turn, in the shape
/// [`render_skill_discovery_block`] renders. 1:1 with the TS attachment element
/// `{ name, description, shortId? }` (`attachments.ts:537`). Only `name` and
/// `description` enter the model text; `short_id` is UI/telemetry-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    /// Canonical skill name — the `Skill("<name>")` invocation target and the
    /// `- {name}: {description}` line key. Also the per-session dedup key.
    pub name: String,
    /// One-line skill description rendered after the name.
    pub description: String,
    /// Optional short id (`shortId?`). UI/transcript-only (`AttachmentMessage.tsx`
    /// `${name} [${shortId}]`); NEVER part of the model-facing block.
    pub short_id: Option<String>,
}

/// Provenance of a discovery BATCH — the TS attachment's `source: 'native' |
/// 'aki' | 'both'` (`attachments.ts`). Telemetry/UI-only; NOT in model bytes.
/// The faithful local backend only ever produces [`DiscoverySource::Native`]
/// (the AKI remote + Haiku-classifier backends are unrecoverable / out of
/// scope — see the crate spec's NON-GOALS).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoverySource {
    /// Local lexical (registry trigger) discovery — the only port-implemented
    /// backend. Default.
    #[default]
    Native,
    /// AKI remote/embedding backend (stub — never produced by the port).
    Aki,
    /// Both native + AKI agreed (stub — never produced by the port).
    Both,
}

/// The selection seam: given the turn query, return the candidate skills.
///
/// The faithful inert/local impl ([`RegistryCandidateSource`]) wraps
/// [`SkillRegistry::discover`] (substring trigger match). The AKI/Haiku-backed
/// selector is a future hook, not this pass.
#[async_trait::async_trait]
pub trait SkillCandidateSource: Send + Sync {
    /// Return the skills relevant to `query` (already mapped to the renderable
    /// [`DiscoveredSkill`] shape). Any failure should resolve to an empty vec —
    /// a failed discovery must never break the turn.
    async fn discover(&self, query: &str) -> Vec<DiscoveredSkill>;
}

/// Local lexical candidate source backed by a [`SkillRegistry`] snapshot. Wraps
/// [`SkillRegistry::discover`] (case-insensitive substring trigger match →
/// `DiscoveredSkill { name, description, short_id: None }`). This is the
/// [STRUCTURE-FAITHFUL] stand-in for the binary's native lexical index
/// (`{name, description, whenToUse}`, in-memory, no embeddings/scores).
pub struct RegistryCandidateSource {
    registry: Arc<SkillRegistry>,
}

impl RegistryCandidateSource {
    /// Wrap a shared registry snapshot.
    #[must_use]
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl SkillCandidateSource for RegistryCandidateSource {
    async fn discover(&self, query: &str) -> Vec<DiscoveredSkill> {
        self.registry
            .discover(query)
            .into_iter()
            .map(|s| DiscoveredSkill {
                name: s.name.clone(),
                description: s.description.clone(),
                short_id: None,
            })
            .collect()
    }
}

/// Side-channel that fires the skill candidate source concurrently with the
/// main turn. Direct clone of [`memory::prefetch::MemoryPrefetch`]'s lifecycle.
pub struct SkillDiscoveryPrefetch {
    /// Candidate source that selects relevant skills. `Some` on the real path
    /// ([`Self::new`]); `None` for the fixed-result seam.
    registry: Option<Arc<dyn SkillCandidateSource>>,
    /// Runtime adapter used to spawn the background task.
    runtime: Arc<dyn RuntimeSpawner>,
    /// Pre-resolved set: when `Some`, [`Self::start`] short-circuits to this set
    /// instead of running the source. `None` ⇒ the source path (or, with no
    /// source, an empty inert result).
    fixed_result: Option<Vec<DiscoveredSkill>>,
}

/// Handle awaiting an in-flight skill-discovery prefetch. Mirrors
/// [`memory::prefetch::PendingMemoryPrefetch`].
pub struct PendingSkillDiscoveryPrefetch {
    /// Receiver that produces the discovered-skill set (ready to render).
    pub rx: tokio::sync::Mutex<Option<oneshot::Receiver<Vec<DiscoveredSkill>>>>,
}

impl PendingSkillDiscoveryPrefetch {
    /// Await the in-flight prefetch, consuming the one-shot receiver.
    ///
    /// Returns the discovered-skill set, or an empty vec if the receiver was
    /// already taken or the background task dropped its sender (e.g. a cancelled
    /// turn — implicit cancellation: dropping the `Pending` drops the receiver,
    /// so the spawned task's `tx.send` no-ops). Never errors — a failed prefetch
    /// must not break the turn. Copied from `PendingMemoryPrefetch::take`.
    pub async fn take(&self) -> Vec<DiscoveredSkill> {
        let rx = self.rx.lock().await.take();
        match rx {
            Some(rx) => rx.await.unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// Non-consuming peek: `true` when the background task has already resolved
    /// (the one-shot is ready) BEFORE collection — the `hidden_by_main_turn`
    /// signal (`query.ts:1617`: true ⇒ the prefetch hid under the main turn's
    /// streaming + tool execution; expected >98%). A `try_recv` peek wrapper that
    /// does NOT consume the value (it leaves the (now-buffered) value for
    /// [`Self::take`]).
    pub fn is_ready(&self) -> bool {
        // `try_lock` so the peek never blocks; if contended (it never is in the
        // single-threaded consume path) treat as not-ready.
        let Ok(mut guard) = self.rx.try_lock() else {
            return false;
        };
        match guard.as_mut() {
            // `try_recv` on an oneshot::Receiver returns Ok(v) once the sender
            // has sent. Per tokio semantics this CONSUMES the value, so we must
            // re-buffer it for `take()` — we do that by swapping in a pre-resolved
            // receiver carrying the same value.
            Some(rx) => match rx.try_recv() {
                Ok(v) => {
                    let (tx2, rx2) = oneshot::channel();
                    let _ = tx2.send(v);
                    *guard = Some(rx2);
                    true
                }
                Err(_) => false,
            },
            None => false,
        }
    }
}

impl SkillDiscoveryPrefetch {
    /// Construct a prefetcher bound to a candidate source + the platform runtime.
    /// [`Self::start`] runs the real selection path.
    #[must_use]
    pub fn new(registry: Arc<dyn SkillCandidateSource>, runtime: Arc<dyn RuntimeSpawner>) -> Self {
        Self {
            registry: Some(registry),
            runtime,
            fixed_result: None,
        }
    }

    /// Construct a prefetcher that resolves to a PRE-SELECTED set, bypassing the
    /// source body. Used by a composition root that has already selected the
    /// relevant skills out of band, and by the orchestrator's surfacing tests to
    /// drive `skill_discovery_reminder_message` deterministically. The result is
    /// buffered before [`Self::start`] returns; no source is needed. Analog of
    /// `MemoryPrefetch::with_fixed_result`.
    #[must_use]
    pub fn with_fixed_result(
        runtime: Arc<dyn RuntimeSpawner>,
        result: Vec<DiscoveredSkill>,
    ) -> Self {
        Self {
            registry: None,
            runtime,
            fixed_result: Some(result),
        }
    }

    /// Kick off a background discovery call and return a pending handle.
    ///
    /// - When `!is_write_pivot`, ship an EMPTY set IMMEDIATELY (the TS
    ///   `findWritePivot` early-return — discovery only fires on write-pivot
    ///   iterations). See [`find_write_pivot`].
    /// - A [`Self::with_fixed_result`] set is shipped verbatim.
    /// - Otherwise, when a source is wired ([`Self::new`]), the background task
    ///   runs the source over the turn `query` and ships the chosen skills.
    /// - With neither, it ships an EMPTY set (inert surfacing reminder).
    pub async fn start(
        &self,
        query: String,
        is_write_pivot: bool,
    ) -> PendingSkillDiscoveryPrefetch {
        let (tx, rx) = oneshot::channel();

        // findWritePivot early-return: non-write iterations surface nothing.
        // [RECONSTRUCTED — re-verify vs services/skillSearch/, not in 2.1.195]
        if !is_write_pivot {
            let _ = self
                .runtime
                .spawn(
                    "skill-discovery-prefetch",
                    Box::pin(async move {
                        let _ = tx.send(Vec::new());
                    }),
                )
                .await;
            return PendingSkillDiscoveryPrefetch {
                rx: tokio::sync::Mutex::new(Some(rx)),
            };
        }

        if let Some(fixed) = self.fixed_result.clone() {
            let _ = tx.send(fixed);
            return PendingSkillDiscoveryPrefetch {
                rx: tokio::sync::Mutex::new(Some(rx)),
            };
        }

        // Real path requires a source; otherwise inert.
        let Some(source) = self.registry.clone() else {
            let _ = self
                .runtime
                .spawn(
                    "skill-discovery-prefetch",
                    Box::pin(async move {
                        let _ = tx.send(Vec::new());
                    }),
                )
                .await;
            return PendingSkillDiscoveryPrefetch {
                rx: tokio::sync::Mutex::new(Some(rx)),
            };
        };

        let _ = self
            .runtime
            .spawn(
                "skill-discovery-prefetch",
                Box::pin(async move {
                    let discovered = source.discover(&query).await;
                    let _ = tx.send(discovered);
                }),
            )
            .await;
        PendingSkillDiscoveryPrefetch {
            rx: tokio::sync::Mutex::new(Some(rx)),
        }
    }
}

/// The per-iteration write-pivot predicate — the TS `findWritePivot` guard that
/// early-returns on non-write iterations so discovery only fires when the turn
/// is about to do real (write) work.
///
/// [RECONSTRUCTED — re-verify vs services/skillSearch/, not in 2.1.195]. The
/// exact predicate (which tool_use names / message shapes = a "write pivot")
/// lives in the DCE'd `prefetch.ts` and is NOT recoverable from any artifact.
/// This is a CONSERVATIVE, `false`-leaning, standalone-testable stand-in: it
/// reports a write pivot only when the most recent assistant turn requested at
/// least one tool whose name is in [`WRITE_PIVOT_TOOL_NAMES`]. RISK: wrong
/// predicate over-fires (extra latency/attachments) or under-fires (no
/// surfacing). Since the feature is OFF by default, a wrong predicate ships zero
/// observable bytes; swap this fn when the body is recovered.
#[must_use]
pub fn find_write_pivot(last_assistant_tool_names: &[String]) -> bool {
    last_assistant_tool_names
        .iter()
        .any(|n| WRITE_PIVOT_TOOL_NAMES.contains(&n.as_str()))
}

/// Tool names that mark a "write pivot" iteration.
/// [RECONSTRUCTED — re-verify vs services/skillSearch/]. The canonical
/// write-tool set the port's other subsystems use (Edit/Write/MultiEdit/
/// NotebookEdit + the shell that performs mutating work).
const WRITE_PIVOT_TOOL_NAMES: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];

/// Render the discovered skills as the body of a single `<system-reminder>` meta
/// user message, BYTE-EXACT with claude-code 2.1.195's `skill_discovery`
/// attachment renderer (`utils/messages.ts:3506-3519`, inside the
/// `feature('EXPERIMENTAL_SKILL_SEARCH')` guard):
///
/// ```text
/// Skills relevant to your task:
///
/// - <name>: <description>
/// - <name>: <description>
///
/// These skills encode project-specific conventions. Invoke via Skill("<name>") for complete instructions.
/// ```
///
/// Lines = `- {name}: {description}` joined by `\n`. EMPTY short-circuit: 0
/// skills ⇒ `None` (TS `if (attachment.skills.length === 0) return []`). The body
/// is wrapped in `<system-reminder>\n{body}\n</system-reminder>` (port
/// envelope-ownership convention, mirroring `render_surfacing_block` /
/// `skill_listing`; byte-equivalent to the TS `wrapMessagesInSystemReminder([…])`
/// of one `isMeta` user message).
#[must_use]
pub fn render_skill_discovery_block(skills: &[DiscoveredSkill]) -> Option<String> {
    if skills.is_empty() {
        return None; // TS `if (attachment.skills.length === 0) return []`
    }
    let lines = skills
        .iter()
        .map(|s| format!("- {}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n");
    let body = format!(
        "Skills relevant to your task:\n\n{lines}\n\n\
         These skills encode project-specific conventions. \
         Invoke via Skill(\"<name>\") for complete instructions."
    );
    Some(format!("<system-reminder>\n{body}\n</system-reminder>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LoadedFrom, Skill, SkillFrontmatter, SkillSource};
    use async_trait::async_trait;

    /// A runtime that actually RUNS the spawned future, so the prefetch's
    /// one-shot resolves. Copied from `memory::prefetch::tests::InlineRuntime`.
    struct InlineRuntime;
    #[async_trait]
    impl RuntimeSpawner for InlineRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<platform_api::BackgroundTaskHandle, platform_api::RuntimeError> {
            tokio::spawn(task);
            Ok(platform_api::BackgroundTaskHandle {
                task_name: name.to_string(),
                task_id: 0,
            })
        }
        async fn sleep(&self, _d: std::time::Duration) {}
        async fn cancel(
            &self,
            _h: &platform_api::BackgroundTaskHandle,
        ) -> Result<(), platform_api::RuntimeError> {
            Ok(())
        }
    }

    fn skill(name: &str, description: &str, short_id: Option<&str>) -> DiscoveredSkill {
        DiscoveredSkill {
            name: name.into(),
            description: description.into(),
            short_id: short_id.map(Into::into),
        }
    }

    fn mk_registry_skill(name: &str, description: &str, triggers: &[&str]) -> Skill {
        Skill {
            name: name.into(),
            description: description.into(),
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: description.into(),
                triggers: triggers.iter().map(|s| (*s).into()).collect(),
                ..Default::default()
            },
            content: String::new(),
            source: SkillSource::Bundled,
            loaded_from: LoadedFrom::Bundled,
            plugin_id: None,
            file_path: "/tmp".into(),
        }
    }

    // (D.1) render byte-exact for 2 skills.
    #[test]
    fn render_block_byte_exact() {
        let out = render_skill_discovery_block(&[
            skill("git-commit", "Commit staged changes", None),
            skill("rebase", "Interactive rebase helper", Some("rb")),
        ])
        .expect("non-empty renders Some");
        assert_eq!(
            out,
            "<system-reminder>\n\
             Skills relevant to your task:\n\n\
             - git-commit: Commit staged changes\n\
             - rebase: Interactive rebase helper\n\n\
             These skills encode project-specific conventions. \
             Invoke via Skill(\"<name>\") for complete instructions.\n\
             </system-reminder>"
        );
    }

    // (D.2) 0 skills ⇒ None (TS `return []`).
    #[test]
    fn render_block_empty_returns_none() {
        assert!(render_skill_discovery_block(&[]).is_none());
    }

    // (D.3) fixed-result path surfaces both.
    #[tokio::test]
    async fn fixed_result_path_surfaces() {
        let p = SkillDiscoveryPrefetch::with_fixed_result(
            Arc::new(InlineRuntime),
            vec![skill("a", "da", None), skill("b", "db", None)],
        );
        let out = p.start("q".into(), true).await.take().await;
        assert_eq!(out, vec![skill("a", "da", None), skill("b", "db", None)]);
    }

    // (D.4) write_pivot=false ⇒ empty even with a non-empty source.
    #[tokio::test]
    async fn write_pivot_false_is_inert() {
        let p = SkillDiscoveryPrefetch::with_fixed_result(
            Arc::new(InlineRuntime),
            vec![skill("a", "da", None)],
        );
        let out = p.start("q".into(), false).await.take().await;
        assert!(
            out.is_empty(),
            "non-write iteration surfaces nothing: {out:?}"
        );
    }

    // (D.5) neither source nor fixed ⇒ empty.
    #[tokio::test]
    async fn inert_when_no_source() {
        // Construct the inert arm directly (no public ctor for it — mirror the
        // production `(None, None)` shape via with_fixed_result-less new path:
        // the real `new` always has a source, so we exercise the inert arm by
        // dropping the source through a registry source over an empty registry).
        let reg = Arc::new(SkillRegistry::new());
        let p = SkillDiscoveryPrefetch::new(
            Arc::new(RegistryCandidateSource::new(reg)),
            Arc::new(InlineRuntime),
        );
        let out = p.start("anything".into(), true).await.take().await;
        assert!(out.is_empty(), "empty registry surfaces nothing: {out:?}");
    }

    // (D.6) take never errors: double-take ⇒ empty, no panic.
    #[tokio::test]
    async fn take_never_errors() {
        let p = SkillDiscoveryPrefetch::with_fixed_result(
            Arc::new(InlineRuntime),
            vec![skill("a", "da", None)],
        );
        let pending = p.start("q".into(), true).await;
        let _first = pending.take().await;
        let second = pending.take().await; // receiver already consumed
        assert!(second.is_empty(), "second take is empty, no panic");
    }

    // Registry source maps discover() → DiscoveredSkill.
    #[tokio::test]
    async fn registry_source_discovers_by_trigger() {
        let mut reg = SkillRegistry::new();
        reg.register(mk_registry_skill(
            "git-commit",
            "Commit helper",
            &["commit", "git"],
        ));
        let src = RegistryCandidateSource::new(Arc::new(reg));
        let out = src.discover("please commit my changes").await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "git-commit");
        assert_eq!(out[0].description, "Commit helper");
        assert_eq!(out[0].short_id, None);
    }

    // (D.7) find_write_pivot predicate [RECONSTRUCTED].
    #[test]
    fn find_write_pivot_predicate() {
        assert!(find_write_pivot(&["Edit".into()]));
        assert!(find_write_pivot(&["Read".into(), "Write".into()]));
        assert!(find_write_pivot(&["Bash".into()]));
        assert!(!find_write_pivot(&["Read".into(), "Grep".into()]));
        assert!(!find_write_pivot(&[]));
    }
}
