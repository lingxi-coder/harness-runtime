//! Orchestrator-side [`hooks::FileChangedFirer`] implementation.
//!
//! Sibling of [`OrchestratorCwdChangedFirer`](crate::OrchestratorCwdChangedFirer):
//! both back a firer trait defined in `hooks` over the engine's shared
//! `Arc<hooks::HookExecutorImpl>` (`orch.hooks`). The desktop file-changed
//! watcher (`engine-desktop::file_changed_watch`) owns the watch loop but has
//! no hook executor; this adapter closes the seam from the orchestrator side:
//! it owns the `Arc<HookExecutorImpl>` + the engine cwd, translates a
//! [`hooks::FileChangedFire`] into a [`hooks::HookEvent::FileChanged`], and
//! fires the registry best-effort.
//!
//! Parity: reproduces claude-code's `handleFileEvent`
//! (`fileChangedWatcher.ts:80`) → `executeFileChangedHooks(path, event)`
//! (`utils/hooks.ts:4278-4294`). The wire payload (`FileChangedHookInput`,
//! `coreSchemas.ts:737-745`) carries `file_path` + `event`
//! (`change` / `add` / `unlink`).
//!
//! Best-effort: [`HookExecutorImpl::execute`] never errors out, so a failing or
//! absent `FileChanged` hook degrades to a no-op and never breaks the watch
//! loop — matching the `CwdChanged` / task firers.
//!
//! Wiring: the composition root (`engine-desktop`) builds an
//! [`OrchestratorFileChangedFirer`] over the SAME `Arc<HookExecutorImpl>` it
//! hands the orchestrator, then injects it into the file-changed watcher.
//! `engine-mobile` never starts the watcher, so the mobile path is unaffected.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use hooks::events::HookEvent;
use hooks::file_changed_firer::{FileChangedFire, FileChangedFirer};
use hooks::registry::HookContext;
use hooks::HookExecutorImpl;

/// Adapts the engine's hook executor to the file-changed watcher's
/// `FileChanged` firer seam.
pub struct OrchestratorFileChangedFirer {
    /// The SAME executor the orchestrator fires its other hooks through.
    hooks: Arc<HookExecutorImpl>,
    /// Engine cwd, threaded into the `FileChanged` hook payload `cwd` field (the
    /// session base dir) and the per-hook Command-arm `LINGXI_PROJECT_DIR`
    /// fallback. Distinct from the fire's `path`, which becomes the wire
    /// `file_path`.
    cwd: PathBuf,
    /// The MAIN orchestrator session's transcript path
    /// (`<config_home>/projects/<sanitize(cwd)>/<uuid>.jsonl`, claude-code
    /// `getTranscriptPathForSession`), stamped on the `FileChanged` hook payload's
    /// `transcript_path` (FIX B). Empty for builds wiring neither.
    transcript_path: PathBuf,
}

impl OrchestratorFileChangedFirer {
    /// Build a firer over the shared hook executor, engine cwd, and the main
    /// session's transcript path. Pass the SAME `Arc<HookExecutorImpl>` handed to
    /// the orchestrator so the `FileChanged` hook rides the identical registry /
    /// async / sandbox plumbing.
    #[must_use]
    pub fn new(hooks: Arc<HookExecutorImpl>, cwd: PathBuf, transcript_path: PathBuf) -> Self {
        Self {
            hooks,
            cwd,
            transcript_path,
        }
    }
}

#[async_trait]
impl FileChangedFirer for OrchestratorFileChangedFirer {
    async fn fire(&self, fire: FileChangedFire) -> Vec<PathBuf> {
        // Map the fire's path/kind onto the `HookEvent::FileChanged` variant;
        // the envelope builder serializes these as the wire `file_path` /
        // `event` (executor.rs build_lifecycle_envelope_body).
        let event = HookEvent::FileChanged {
            path: fire.path,
            kind: fire.kind,
        };
        // Context-light: a file change has no live per-turn session here, so we
        // thread the engine cwd (also the LINGXI_PROJECT_DIR fallback) and the main
        // session's `transcript_path` (FIX B). Everything else defaults — matching
        // `OrchestratorCwdChangedFirer`.
        let ctx = HookContext {
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            ..Default::default()
        };
        // Best-effort: the executor never errors out of `execute`, so a
        // misbehaving / absent hook degrades to a no-op and never breaks the
        // watch loop. `FileChanged` is observational for its decision, but the
        // aggregated `watchPaths` drive the "dynamic watch paths" feedback loop
        // (`fileChangedWatcher.ts:108-131`) — resolve each against the engine
        // cwd (a relative matcher is anchored to cwd, mirroring
        // `resolveWatchPaths`) and return the deduped set for the watcher to
        // fold in and restart.
        let agg = self.hooks.execute(event, ctx).await;
        let mut out: Vec<PathBuf> = Vec::new();
        let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for raw in agg.watch_paths {
            let p = std::path::Path::new(&raw);
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.cwd.join(p)
            };
            if seen.insert(resolved.clone()) {
                out.push(resolved);
            }
        }
        out
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
        let firer = OrchestratorFileChangedFirer::new(
            noop_hook_executor(),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        firer
            .fire(FileChangedFire {
                path: PathBuf::from("/work/.envrc"),
                kind: "change".into(),
            })
            .await;
    }

    // Inert HTTP / runtime stubs: the Builtin hook arm dispatches in-process,
    // so neither is ever called. Mirrors `test_support::noop_hook_executor`.
    struct UnusedHttp;
    #[async_trait]
    impl platform_api::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
    }
    struct UnusedRuntime;
    #[async_trait]
    impl platform_api::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<platform_api::BackgroundTaskHandle, platform_api::RuntimeError> {
            Err(platform_api::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _duration: std::time::Duration) {}
        async fn cancel(
            &self,
            _handle: &platform_api::BackgroundTaskHandle,
        ) -> Result<(), platform_api::RuntimeError> {
            Ok(())
        }
    }

    /// Records the `(path, kind)` it is dispatched, proving the firer's
    /// `FileChangedFire` reaches the executor as a `HookEvent::FileChanged`.
    struct RecordingBuiltin {
        seen: Arc<Mutex<Option<(PathBuf, String)>>>,
    }
    #[async_trait]
    impl BuiltinHookHandler for RecordingBuiltin {
        async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
            if let HookEvent::FileChanged { path, kind } = event {
                *self.seen.lock().unwrap() = Some((path.clone(), kind.clone()));
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
            "file-changed-recorder"
        }
    }

    #[tokio::test]
    async fn fire_maps_to_delivered_file_changed_event() {
        // A Builtin hook subscribed to `FileChanged` routed to a recording
        // handler: firing the orchestrator firer must deliver a
        // `HookEvent::FileChanged { path, kind }` carrying the fire's values.
        let seen: Arc<Mutex<Option<(PathBuf, String)>>> = Arc::new(Mutex::new(None));
        let handler = Arc::new(RecordingBuiltin { seen: seen.clone() });

        let mut registry = HookRegistry::new();
        registry.register(HookDefinition {
            id: protocol::HookId::new(),
            name: "file-changed".into(),
            events: vec![HookEventType::FileChanged],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "file-changed-recorder".into(),
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

        let reg = Arc::new(tokio::sync::RwLock::new(registry));
        let mut exec = HookExecutorImpl::new(
            reg,
            Arc::new(UnusedHttp) as Arc<dyn platform_api::HttpTransport>,
            Arc::new(UnusedRuntime) as Arc<dyn platform_api::RuntimeSpawner>,
        );
        exec.register_builtin(handler);

        let firer = OrchestratorFileChangedFirer::new(
            Arc::new(exec),
            PathBuf::from("/work"),
            PathBuf::from("/work/.t.jsonl"),
        );
        firer
            .fire(FileChangedFire {
                path: PathBuf::from("/work/.env"),
                kind: "unlink".into(),
            })
            .await;

        let got = seen.lock().unwrap().clone();
        assert_eq!(
            got,
            Some((PathBuf::from("/work/.env"), "unlink".to_string())),
            "OrchestratorFileChangedFirer must deliver HookEvent::FileChanged(path,kind)"
        );
    }
}
