use super::*;
use lingxi_core::host::{FileContent, FileEventKind, FlockGuard, FsError};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

type WatchInput = (mpsc::UnboundedReceiver<FileEvent>, Arc<Notify>);
struct WatchFs(Mutex<HashMap<PathBuf, WatchInput>>);
#[async_trait]
impl FileSystem for WatchFs {
    async fn read_file(
        &self,
        _: &str,
        _: Option<u64>,
        _: Option<u64>,
    ) -> Result<FileContent, FsError> {
        unreachable!()
    }
    async fn write_file(&self, _: &str, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        dir: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
        let input = self.0.lock().unwrap().remove(&PathBuf::from(dir));
        Ok(match input {
            Some((rx, seen)) => Box::pin(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx).map(move |event| {
                    seen.notify_one();
                    event
                }),
            ),
            None => Box::pin(tokio_stream::empty()),
        })
    }
    async fn append_file(&self, _: &str, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
        unreachable!()
    }
    async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
        unreachable!()
    }
    async fn file_size(&self, _: &str) -> Result<u64, FsError> {
        unreachable!()
    }
    async fn delete_file(&self, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        unreachable!()
    }
    async fn fsync(&self, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
}

#[derive(Default)]
struct Publications {
    old_entered: Notify,
    release_old: Notify,
    new_committed: Notify,
    models: Mutex<Vec<Option<String>>>,
    prompt: Mutex<(Option<String>, Option<String>)>,
    hooks: Mutex<Vec<ConfigChangeSource>>,
}
#[async_trait]
impl ConfigChangeFirer for Publications {
    async fn fire_config_change(&self, source: ConfigChangeSource, _: Option<PathBuf>) {
        self.hooks.lock().unwrap().push(source);
    }
}
#[async_trait]
impl MainModelSelectionRefresher for Publications {
    async fn refresh_main_model_selection(
        &self,
        model: Option<String>,
        _: Option<Utf16JsonProjection>,
        _: lingxi_core::host::ModelProvenance,
        _: Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
        _: agent::model_resolution::NativeModelSettingsSnapshot,
    ) {
        let old = model.as_deref() == Some("old-setting");
        if old {
            self.old_entered.notify_one();
            self.release_old.notified().await;
        }
        self.models.lock().unwrap().push(model);
        if !old {
            self.new_committed.notify_one();
        }
    }
}
#[async_trait]
impl NativeSettingsRefresher for Publications {
    async fn refresh_native_settings(
        &self,
        output: Option<Utf16JsonProjection>,
        language: Option<Utf16JsonProjection>,
    ) {
        *self.prompt.lock().unwrap() = (
            output.and_then(|value| value.string_at("").map(str::to_owned)),
            language.and_then(|value| value.string_at("").map(str::to_owned)),
        );
    }
}

async fn reached(notify: &Notify) {
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn cross_directory_refresh_serializes_capture_publication_and_cancelled_restart() {
    let _env = crate::desktop::tests::MANAGED_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("user");
    let cwd = root.path().join("project");
    let project = cwd.join(branding::DOT_DIR);
    let managed = root.path().join("managed");
    for dir in [&home, &project, &managed] {
        std::fs::create_dir_all(dir).unwrap();
    }
    struct RestoreEnv(Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(MANAGED_DIR_ENV, value),
                None => std::env::remove_var(MANAGED_DIR_ENV),
            }
        }
    }
    let _restore = RestoreEnv(std::env::var_os(MANAGED_DIR_ENV));
    std::env::set_var(MANAGED_DIR_ENV, &managed);
    let user_file = home.join("settings.json");
    let project_file = project.join("settings.json");
    let mut config = crate::desktop::DesktopConfig::default();
    config.cwd = cwd.clone();
    config.lingxi_home = home.clone();
    config.setting_source_scope = (true, true);
    for cancel in [false, true] {
        let _ = std::fs::remove_file(&project_file);
        std::fs::write(
            &user_file,
            r#"{"model":"old-setting","outputStyle":"old-style","language":"old-language"}"#,
        )
        .unwrap();
        let (user_tx, user_rx) = mpsc::unbounded_channel();
        let (project_tx, project_rx) = mpsc::unbounded_channel();
        let project_seen = Arc::new(Notify::new());
        let fs = Arc::new(WatchFs(Mutex::new(HashMap::from([
            (home.clone(), (user_rx, Arc::new(Notify::new()))),
            (project.clone(), (project_rx, project_seen.clone())),
        ]))));
        let publications = Arc::new(Publications::default());
        let make = || {
            SettingsWatcher::new(&home, &cwd, publications.clone())
                .with_main_model_selection_refresh(
                    MainModelSelectionRefresh::from_config(&config, &[]),
                    publications.clone(),
                )
                .with_native_settings_refresher(publications.clone())
        };
        let handle = make().spawn(fs).await;
        user_tx
            .send(FileEvent {
                path: user_file.clone(),
                kind: FileEventKind::Modified,
            })
            .unwrap();
        reached(&publications.old_entered).await;
        std::fs::write(
            &project_file,
            r#"{"model":"new-setting","outputStyle":"new-style","language":"new-language"}"#,
        )
        .unwrap();
        project_tx
            .send(FileEvent {
                path: project_file.clone(),
                kind: FileEventKind::Modified,
            })
            .unwrap();
        reached(&project_seen).await;
        assert!(publications.models.lock().unwrap().is_empty());
        assert_eq!(
            publications.prompt.lock().unwrap().0.as_deref(),
            Some("old-style"),
            "a consumed second event still waits before capturing/publishing"
        );
        assert_eq!(
            *publications.hooks.lock().unwrap(),
            vec![ConfigChangeSource::UserSettings]
        );
        if cancel {
            handle.shutdown_and_drain().await;
            let (tx, rx) = mpsc::unbounded_channel();
            let fs = Arc::new(WatchFs(Mutex::new(HashMap::from([(
                home.clone(),
                (rx, Arc::new(Notify::new())),
            )]))));
            let restarted = make().spawn(fs).await;
            tx.send(FileEvent {
                path: user_file.clone(),
                kind: FileEventKind::Modified,
            })
            .unwrap();
            reached(&publications.new_committed).await;
            restarted.shutdown_and_drain().await;
            publications.release_old.notify_one();
            assert_eq!(
                publications.models.lock().unwrap().len(),
                1,
                "retired publishers cannot resume"
            );
        } else {
            publications.release_old.notify_one();
            reached(&publications.new_committed).await;
            handle.shutdown_and_drain().await;
            assert_eq!(
                *publications.models.lock().unwrap(),
                vec![Some("old-setting".into()), Some("new-setting".into())]
            );
        }
        let effective = crate::desktop::load_effective_settings_for_config(
            &config,
            Some(&managed_settings_snapshot().await),
        )
        .unwrap();
        assert_eq!(
            publications
                .models
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .as_deref(),
            effective.settings.model.as_deref()
        );
        assert_eq!(
            *publications.prompt.lock().unwrap(),
            (Some("new-style".into()), Some("new-language".into()))
        );
    }
}
