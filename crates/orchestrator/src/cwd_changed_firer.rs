//! Orchestrator-side [`hooks::CwdChangedFirer`] implementation.
//!
//! Counterpart to [`OrchestratorTaskCreatedFirer`](crate::OrchestratorTaskCreatedFirer)
//! and [`OrchestratorTaskCompletedFirer`](crate::OrchestratorTaskCompletedFirer).
//! The `tool-shell` crate is a LEAF: [`BashTool`](../../tools/shell/src/bash.rs)
//! owns the persistent shell cwd but has no access to a live hook executor, and
//! the orchestrator depends on `hooks` but NOT on `tool-shell`. The firer trait
//! therefore lives in `hooks` (both crates can name it); this adapter closes the
//! seam from the orchestrator side: it owns the engine's
//! `Arc<hooks::HookExecutorImpl>` (`orch.hooks`) plus the engine cwd, translates
//! a [`hooks::CwdChangedFire`] into a [`hooks::HookEvent::CwdChanged`], and fires
//! the registry best-effort.
//!
//! Parity: reproduces claude-code's `onCwdChangedForHooks(oldCwd, newCwd)`
//! (`utils/Shell.ts:409`, fired from the `pwd -P` readback) → `executeCwdChangedHooks`
//! (`utils/hooks.ts:4260`) — the wire payload (`CwdChangedHookInput`,
//! `types/hooks.ts:146`) carries `old_cwd` / `new_cwd`.
//!
//! Best-effort: [`HookExecutorImpl::execute`] never errors out, so a failing or
//! absent `CwdChanged` hook degrades to a no-op and never breaks the cwd update
//! — matching the `TaskCreated` / `TaskCompleted` arms.
//!
//! Wiring: the composition root (`engine-desktop`) builds an
//! [`OrchestratorCwdChangedFirer`] over the SAME `Arc<HookExecutorImpl>` it hands
//! the orchestrator, then injects it via `BashTool::with_cwd_changed_firer` where
//! the desktop shell tools are registered. `engine-mobile` never registers the
//! shell tools (`tool_shell::register_all` is desktop-only), so the mobile path
//! keeps the plain `BashTool::new(ctx)` with the firer `None`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::cwd_changed_firer::{CwdChangedFire, CwdChangedFirer};
use hooks::events::HookEvent;
use hooks::registry::HookContext;
use hooks::HookExecutorImpl;

/// Adapts the engine's hook executor to the `tool-shell` crate's `CwdChanged`
/// firer seam.
pub struct OrchestratorCwdChangedFirer {
    /// The SAME executor the orchestrator fires its other hooks through.
    hooks: Arc<HookExecutorImpl>,
    /// Engine cwd, threaded into the `CwdChanged` hook payload `cwd` field (the
    /// session base dir) and the per-hook Command-arm `LINGXI_PROJECT_DIR`
    /// fallback. This is distinct from the fire's `old`/`new` shell cwd, which
    /// becomes the `old_cwd` / `new_cwd` payload fields.
    cwd: PathBuf,
    /// The MAIN orchestrator session's transcript path
    /// (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
    /// `getTranscriptPathForSession`), stamped on the `CwdChanged` hook payload's
    /// `transcript_path` (FIX B). Empty for builds wiring neither.
    transcript_path: PathBuf,
    /// The orchestrator's shared mutable-cwd cell. On every fire (each Bash `cd`
    /// that moved the persistent shell cwd) `fire.new` is written here BEFORE the
    /// hook fires, so the orchestrator's subsequent PreToolUse/PostToolUse/
    /// lifecycle hook payloads read the post-`cd` directory — 1:1 with
    /// claude-code, where `setCwdState(new)` (`Shell.ts:409`) updates the single
    /// global cwd every later `getCwd()` reads.
    current_cwd: Arc<std::sync::Mutex<PathBuf>>,
    /// Optional file-changed-watcher rebinder — the watcher-rebind half of
    /// claude-code's `onCwdChanged` (function `g`, `fileChangedWatcher.ts`).
    /// After the `CwdChanged` hooks fire, the watcher re-resolves its
    /// `FileChanged` matchers against the new cwd (REPLACING its watch set) and
    /// restarts. Default `None` (mobile, or a desktop session with no
    /// `FileChanged` hooks — the watcher only exists when those hooks do), which
    /// makes the rebind a strict no-op. The `CwdChanged` hooks themselves are
    /// fired ABOVE, so this MUST NOT re-fire them (no double-fire).
    watcher_rebinder: hooks::OptionalWatcherRebinder,
}

impl OrchestratorCwdChangedFirer {
    /// Build a firer over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator so the `CwdChanged` hook rides the identical registry /
    /// async / sandbox plumbing.
    #[must_use]
    pub fn new(
        hooks: Arc<HookExecutorImpl>,
        cwd: PathBuf,
        transcript_path: PathBuf,
        current_cwd: Arc<std::sync::Mutex<PathBuf>>,
    ) -> Self {
        Self {
            hooks,
            cwd,
            transcript_path,
            current_cwd,
            watcher_rebinder: None,
        }
    }

    /// Attach the file-changed-watcher rebinder (the watcher-rebind half of
    /// claude-code's `onCwdChanged`). The composition root injects this so that,
    /// after a `cd` fires the `CwdChanged` hooks, the watcher re-resolves its
    /// `FileChanged` matchers against the new cwd and restarts. Left unset
    /// (mobile / no `FileChanged` hooks), the rebind is a strict no-op.
    #[must_use]
    pub fn with_watcher_rebinder(mut self, rebinder: Arc<dyn hooks::WatcherRebinder>) -> Self {
        self.watcher_rebinder = Some(rebinder);
        self
    }
}

#[async_trait]
impl CwdChangedFirer for OrchestratorCwdChangedFirer {
    async fn fire(&self, fire: CwdChangedFire) {
        // Update the orchestrator's shared current-cwd FIRST (mirrors
        // `setCwdState(new)` firing BEFORE `executeCwdChangedHooks` in
        // Shell.ts:409), so any hook that fires AFTER this `cd` reads the new dir.
        if let Ok(mut g) = self.current_cwd.lock() {
            g.clone_from(&fire.new);
        }
        // Retained for the watcher-rebind step below (the hook `event` moves
        // `fire.new`); this is the cwd the file-changed watcher re-resolves its
        // `FileChanged` matchers against.
        let new_cwd = fire.new.clone();
        // Map the fire's old/new shell cwd onto the `HookEvent::CwdChanged`
        // variant (`old` / `new`); the envelope builder serializes these as the
        // wire `old_cwd` / `new_cwd` (executor.rs build_lifecycle_envelope_body).
        let event = HookEvent::CwdChanged {
            old: fire.old,
            new: fire.new,
        };
        // Context-light: a cwd change has no live per-turn session here, so we
        // thread the engine cwd (also the LINGXI_PROJECT_DIR fallback) and the main
        // session's `transcript_path` (FIX B). Everything else defaults — matching
        // the `OrchestratorTaskCreatedFirer`.
        let ctx = HookContext {
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            ..Default::default()
        };
        // Best-effort: the executor never errors out of `execute`, so a
        // misbehaving / absent hook degrades to a no-op and never breaks the
        // caller's cwd update. The aggregate result is intentionally dropped —
        // `CwdChanged` is observational.
        let _ = self.hooks.execute(event, ctx).await;
        // Watcher-rebind half of claude-code's `onCwdChanged` (function `g`):
        // AFTER the `CwdChanged` hooks fire, ask the file-changed watcher to
        // re-resolve its `FileChanged` matchers against the new cwd and restart
        // (`t=S ... let x=await E3r(_,S); r=x.watchPaths ... if(o)m()`). The
        // watcher itself guards an unchanged cwd (`if(_===S)return`), so we fire
        // unconditionally. No-op when unset (mobile / no `FileChanged` hooks).
        if let Some(rebinder) = &self.watcher_rebinder {
            rebinder.rebind(new_cwd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::noop_hook_executor;
    use std::sync::Mutex;

    use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor};
    use hooks::events::HookEventType;
    use hooks::registry::HookRegistry;
    use hooks::response::{HookOutcome, HookResult};
    use hooks::{BuiltinHookHandler, HookSource};

    #[tokio::test]
    async fn no_matching_hook_is_a_noop_fire() {
        // An executor with an empty registry never intervenes => the fire is a
        // silent no-op (the "no hook registered" contract). Must not panic/hang.
        let firer = OrchestratorCwdChangedFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
            Arc::new(Mutex::new(PathBuf::from("/work"))),
        );
        firer
            .fire(CwdChangedFire {
                old: PathBuf::from("/work"),
                new: PathBuf::from("/work/sub"),
            })
            .await;
    }

    /// Records every `new_cwd` a rebind is asked to forward — proves the firer
    /// signals the file-changed watcher's rebind seam with the post-`cd` dir.
    struct RecordingRebinder {
        seen: Arc<Mutex<Vec<PathBuf>>>,
    }
    impl hooks::WatcherRebinder for RecordingRebinder {
        fn rebind(&self, new_cwd: PathBuf) {
            self.seen.lock().unwrap().push(new_cwd);
        }
    }

    #[tokio::test]
    async fn fire_forwards_new_cwd_to_watcher_rebinder() {
        // The watcher-rebind half of `onCwdChanged`: an injected rebinder must
        // receive `fire.new` so the file-changed watcher re-resolves its
        // matchers against the new cwd. (The `CwdChanged` hooks are fired
        // separately, above the rebind — this seam must NOT re-fire them.)
        let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
        let rebinder = Arc::new(RecordingRebinder { seen: seen.clone() });
        let firer = OrchestratorCwdChangedFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
            Arc::new(Mutex::new(PathBuf::from("/work"))),
        )
        .with_watcher_rebinder(rebinder);
        firer
            .fire(CwdChangedFire {
                old: PathBuf::from("/work"),
                new: PathBuf::from("/work/sub"),
            })
            .await;
        assert_eq!(
            *seen.lock().unwrap(),
            vec![PathBuf::from("/work/sub")],
            "fire must forward fire.new to the watcher rebinder"
        );
    }

    #[tokio::test]
    async fn fire_without_rebinder_never_rebinds() {
        // Default firer (no rebinder wired — mobile / no FileChanged hooks): the
        // fire is a clean no-op and never panics for the missing rebind seam.
        // (The `CwdChanged` hook still fires — covered by
        // `fire_maps_to_delivered_cwd_changed_event`.)
        let firer = OrchestratorCwdChangedFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
            Arc::new(Mutex::new(PathBuf::from("/work"))),
        );
        assert!(
            firer.watcher_rebinder.is_none(),
            "default firer has no rebinder"
        );
        firer
            .fire(CwdChangedFire {
                old: PathBuf::from("/work"),
                new: PathBuf::from("/work/sub"),
            })
            .await;
    }

    // Inert HTTP / runtime stubs: the Builtin hook arm dispatches in-process,
    // so neither is ever called. Mirrors `test_support::noop_hook_executor`.
    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }
    struct UnusedRuntime;
    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: std::time::Duration) {}
        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    /// Records the `(old, new)` it is dispatched, proving the firer's
    /// `CwdChangedFire` reaches the executor as a `HookEvent::CwdChanged`. Also
    /// captures the delivered `ctx.transcript_path` (FIX B) so the test can assert
    /// the firer stamped the main session's transcript path on the hook context.
    struct RecordingBuiltin {
        seen: Arc<Mutex<Option<(PathBuf, PathBuf)>>>,
        seen_transcript: Arc<Mutex<Option<PathBuf>>>,
    }
    #[async_trait]
    impl BuiltinHookHandler for RecordingBuiltin {
        async fn handle(&self, event: &HookEvent, ctx: &HookContext) -> HookResult {
            if let HookEvent::CwdChanged { old, new } = event {
                *self.seen.lock().unwrap() = Some((old.clone(), new.clone()));
                *self.seen_transcript.lock().unwrap() = Some(ctx.transcript_path.clone());
            }
            HookResult {
                outcome: HookOutcome::Success,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                response: None,
            }
        }
        fn id(&self) -> &str {
            "cwd-changed-recorder"
        }
    }

    #[tokio::test]
    async fn fire_maps_to_delivered_cwd_changed_event() {
        // A Builtin hook subscribed to `CwdChanged` routed to a recording
        // handler: firing the orchestrator firer must deliver a
        // `HookEvent::CwdChanged { old, new }` carrying the fire's paths.
        let seen: Arc<Mutex<Option<(PathBuf, PathBuf)>>> = Arc::new(Mutex::new(None));
        let seen_transcript: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let handler = Arc::new(RecordingBuiltin {
            seen: seen.clone(),
            seen_transcript: seen_transcript.clone(),
        });

        let mut registry = HookRegistry::new();
        registry.register(HookDefinition {
            id: lingxi_core::types::HookId::new(),
            name: "cwd-changed".into(),
            events: vec![HookEventType::CwdChanged],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "cwd-changed-recorder".into(),
            },
            source: HookSource::Skill,
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });

        // Build a fresh executor over the populated registry. The Builtin arm
        // dispatches in-process, so the http/runtime stubs are inert.
        let reg = Arc::new(tokio::sync::RwLock::new(registry));
        let mut exec = HookExecutorImpl::new(
            reg,
            Arc::new(UnusedHttp) as Arc<dyn lingxi_core::host::HttpTransport>,
            Arc::new(UnusedRuntime) as Arc<dyn lingxi_core::host::RuntimeSpawner>,
        );
        exec.register_builtin(handler);

        let current_cwd = Arc::new(Mutex::new(PathBuf::from("/work")));
        let firer = OrchestratorCwdChangedFirer::new(
            Arc::new(exec),
            PathBuf::from("/work"),
            PathBuf::from("/home/.lingxi/projects/-work/abc.jsonl"),
            current_cwd.clone(),
        );
        firer
            .fire(CwdChangedFire {
                old: PathBuf::from("/work/old"),
                new: PathBuf::from("/work/new"),
            })
            .await;

        // The shared current-cwd cell is advanced to the new dir, so later hooks
        // read the post-`cd` directory.
        assert_eq!(
            *current_cwd.lock().unwrap(),
            PathBuf::from("/work/new"),
            "the firer must advance the shared current_cwd to fire.new"
        );

        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            Some((PathBuf::from("/work/old"), PathBuf::from("/work/new"))),
            "OrchestratorCwdChangedFirer must deliver HookEvent::CwdChanged(old,new)"
        );
        // FIX B: the firer must stamp the main session's transcript_path on the
        // delivered hook context (claude-code `createBaseHookInput` always sets it).
        let got_transcript = seen_transcript.lock().unwrap().clone();
        assert_eq!(
            got_transcript,
            Some(PathBuf::from("/home/.lingxi/projects/-work/abc.jsonl")),
            "OrchestratorCwdChangedFirer must carry the constructor's transcript_path \
             into the HookContext (non-empty)"
        );
    }
}
