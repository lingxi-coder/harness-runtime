//! Explicit, process-once media initialization without environment mutation or
//! global teardown. The host must isolate GStreamer's environment before
//! startup: disabling registry updates does not prevent an initial scan.
use gstreamer::{self as gst, glib};
use std::{
    ffi::CString,
    path::{Path, PathBuf},
    sync::OnceLock,
};
#[derive(Debug)]
enum InitializedRuntime {
    Dynamic(PathBuf),
    #[cfg(feature = "static-plugins")]
    Static,
}

static INITIALIZED: OnceLock<Result<InitializedRuntime, String>> = OnceLock::new();
#[cfg(not(feature = "static-plugins"))]
static CONFIGURED_VIDEO: OnceLock<Result<(), String>> = OnceLock::new();

/// Initialize the host-selected distribution without embedding a release SDK path.
/// The product launcher supplies its bundled plugin directory on dynamic hosts.
pub fn initialize_configured(video: bool) -> Result<(), String> {
    #[cfg(feature = "static-plugins")]
    {
        let _ = video;
        initialize_static()
    }
    #[cfg(not(feature = "static-plugins"))]
    {
        let directory = configured_plugin_directory()?;
        initialize(&directory)?;
        if video {
            CONFIGURED_VIDEO
                .get_or_init(|| load_video_plugins(&directory))
                .clone()?;
        }
        Ok(())
    }
}

#[cfg(not(feature = "static-plugins"))]
fn configured_plugin_directory() -> Result<PathBuf, String> {
    match std::env::var_os("LINGXI_GSTREAMER_PLUGIN_DIR") {
        Some(directory) if !directory.is_empty() => Ok(PathBuf::from(directory)),
        Some(_) => Err("configured media plugin directory is empty".into()),
        None => {
            let development = env!("LINGXI_DEV_GST_PLUGIN_DIR");
            if development.is_empty() {
                Err("the host must configure its media plugin directory".into())
            } else {
                Ok(PathBuf::from(development))
            }
        }
    }
}
/// Bind the process to one canonical plugin directory for its entire lifetime.
pub fn initialize(plugin_directory: &Path) -> Result<(), String> {
    initialize_dynamic(&INITIALIZED, plugin_directory, initialize_inner)
}

fn initialize_dynamic(
    state: &OnceLock<Result<InitializedRuntime, String>>,
    directory: &Path,
    initialize: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    state.get_or_init(|| {
        let directory = directory
            .canonicalize()
            .map_err(|_| "media plugin directory unavailable".to_owned())?;
        initialize(&directory)?;
        Ok(InitializedRuntime::Dynamic(directory))
    });
    require_dynamic_directory(state, directory).map(|_| ())
}

fn require_dynamic_directory(
    state: &OnceLock<Result<InitializedRuntime, String>>,
    directory: &Path,
) -> Result<PathBuf, String> {
    match state.get() {
        Some(Ok(InitializedRuntime::Dynamic(initial))) => {
            let requested = directory
                .canonicalize()
                .map_err(|_| "media plugin directory unavailable".to_owned())?;
            if &requested != initial {
                return Err(
                    "media runtime is already configured for another plugin directory".into(),
                );
            }
            Ok(requested)
        }
        #[cfg(feature = "static-plugins")]
        Some(Ok(InitializedRuntime::Static)) => {
            Err("media runtime is already configured for static plugins".into())
        }
        Some(Err(error)) => Err(error.clone()),
        None => Err("initialize media runtime before opening devices".into()),
    }
}

fn initialize_inner(directory: &Path) -> Result<(), String> {
    initialize_engine()?;
    for name in [
        "app",
        "audioconvert",
        "audioresample",
        "coreelements",
        "opus",
        "rtp",
        "rtpmanager",
    ] {
        gst::Plugin::load_file(plugin_file(directory, name))
            .map_err(|_| format!("required media plugin unavailable: {name}"))?;
    }
    Ok(())
}
fn initialize_engine() -> Result<(), String> {
    let args = [
        "lingxi-media",
        "--gst-disable-registry-update",
        "--gst-disable-registry-fork",
    ];
    let args: Vec<_> = args.iter().map(|s| CString::new(*s).unwrap()).collect();
    let mut argv: Vec<_> = args
        .iter()
        .map(|s| s.as_ptr().cast_mut())
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect();
    let mut argv_ptr = argv.as_mut_ptr();
    let mut argc = args.len() as i32;
    let mut error = std::ptr::null_mut();
    // SAFETY: Strings and mutable, null-terminated argv storage outlive this
    // synchronous call. GStreamer parses only our fixed arguments, not host argv.
    let ok = unsafe { gst::ffi::gst_init_check(&mut argc, &mut argv_ptr, &mut error) };
    if ok == glib::ffi::GFALSE {
        if !error.is_null() {
            unsafe { glib::ffi::g_error_free(error) };
        }
        return Err("media runtime initialization failed".into());
    }
    gst::init().map_err(|_| "media Rust bindings initialization failed")?;
    Ok(())
}

/// Mobile applications link only the selected static plugins. No dlopen,
/// registry-scanning child process or per-session global teardown is needed.
#[cfg(feature = "static-plugins")]
pub fn initialize_static() -> Result<(), String> {
    unsafe extern "C" {
        fn gst_plugin_app_register();
        fn gst_plugin_audioconvert_register();
        fn gst_plugin_audioresample_register();
        fn gst_plugin_coreelements_register();
        fn gst_plugin_opus_register();
        fn gst_plugin_rtp_register();
        fn gst_plugin_rtpmanager_register();
        fn gst_plugin_vpx_register();
        fn gst_plugin_videoconvertscale_register();
    }
    match INITIALIZED.get_or_init(|| {
        initialize_engine()?;
        // SAFETY: These fixed ABI functions belong to the linked SDK archives.
        // Each plugin is registered once, after process-wide GStreamer init.
        unsafe {
            gst_plugin_app_register();
            gst_plugin_audioconvert_register();
            gst_plugin_audioresample_register();
            gst_plugin_coreelements_register();
            gst_plugin_opus_register();
            gst_plugin_rtp_register();
            gst_plugin_rtpmanager_register();
            gst_plugin_vpx_register();
            gst_plugin_videoconvertscale_register();
        }
        for factory in [
            "appsrc",
            "appsink",
            "opusdec",
            "vp8enc",
            "vp8dec",
            "rtpjitterbuffer",
            "rtpopusdepay",
            "rtpvp8depay",
            "videoconvert",
        ] {
            if gst::ElementFactory::find(factory).is_none() {
                return Err(format!(
                    "required static media element unavailable: {factory}"
                ));
            }
        }
        Ok(InitializedRuntime::Static)
    }) {
        Ok(InitializedRuntime::Static) => Ok(()),
        Ok(InitializedRuntime::Dynamic(_)) => {
            Err("media runtime is already configured for dynamic plugins".into())
        }
        Err(error) => Err(error.clone()),
    }
}
pub(crate) fn ensure_initialized() -> Result<(), &'static str> {
    match INITIALIZED.get() {
        Some(Ok(_)) => Ok(()),
        _ => Err("initialize media runtime before opening devices"),
    }
}
pub fn load_video_plugins(directory: &Path) -> Result<(), String> {
    let directory = require_dynamic_directory(&INITIALIZED, directory)?;
    for name in ["vpx", "videoconvertscale"] {
        gst::Plugin::load_file(plugin_file(&directory, name))
            .map_err(|_| format!("required video plugin unavailable: {name}"))?;
    }
    Ok(())
}
fn plugin_file(directory: &Path, name: &str) -> std::path::PathBuf {
    directory.join(if cfg!(target_os = "macos") {
        format!("libgst{name}.dylib")
    } else if cfg!(windows) {
        format!("gst{name}.dll")
    } else {
        format!("libgst{name}.so")
    })
}
#[cfg(test)]
pub(crate) fn test_init() {
    initialize(Path::new(env!("LINGXI_DEV_GST_PLUGIN_DIR"))).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TestDirectories {
        root: PathBuf,
        first: PathBuf,
        second: PathBuf,
    }

    impl TestDirectories {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "lingxi-media-directory-{}-{nanos}-{unique}",
                std::process::id()
            ));
            let first = root.join("first");
            let second = root.join("second");
            fs::create_dir_all(&first).unwrap();
            fs::create_dir(&second).unwrap();
            Self {
                root,
                first,
                second,
            }
        }
    }

    impl Drop for TestDirectories {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn initialization_binds_canonical_directory_for_audio_and_video() {
        let dirs = TestDirectories::new();
        let state = OnceLock::new();
        let calls = AtomicUsize::new(0);
        initialize_dynamic(&state, &dirs.first, |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
        initialize_dynamic(&state, &dirs.first.join("."), |_| {
            panic!("a second initializer must not run")
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(
            initialize_dynamic(&state, &dirs.second, |_| {
                panic!("a different directory must not initialize")
            })
            .is_err()
        );
        assert!(require_dynamic_directory(&state, &dirs.second).is_err());
        assert_eq!(
            require_dynamic_directory(&state, &dirs.first).unwrap(),
            dirs.first.canonicalize().unwrap()
        );
    }

    #[test]
    fn competing_initializations_cannot_claim_different_directories() {
        let dirs = TestDirectories::new();
        let state = Arc::new(OnceLock::new());
        let barrier = Arc::new(Barrier::new(12));
        let calls = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..12)
            .map(|index| {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                let calls = Arc::clone(&calls);
                let directory = if index % 2 == 0 {
                    dirs.first.clone()
                } else {
                    dirs.second.clone()
                };
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = initialize_dynamic(&state, &directory, |_| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    });
                    (directory, result)
                })
            })
            .collect();
        let results: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        let winner = match state.get().unwrap().as_ref().unwrap() {
            InitializedRuntime::Dynamic(directory) => directory,
            #[cfg(feature = "static-plugins")]
            InitializedRuntime::Static => panic!("expected dynamic runtime"),
        };
        for (directory, result) in results {
            assert_eq!(result.is_ok(), directory.canonicalize().unwrap() == *winner);
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_initialization_is_not_retried_with_another_directory() {
        let dirs = TestDirectories::new();
        let state = OnceLock::new();
        let error =
            initialize_dynamic(&state, &dirs.first, |_| Err("initial failure".into())).unwrap_err();
        assert_eq!(error, "initial failure");
        assert_eq!(
            initialize_dynamic(&state, &dirs.second, |_| panic!("must not retry")).unwrap_err(),
            error
        );
    }

    #[cfg(feature = "static-plugins")]
    #[test]
    fn static_runtime_cannot_be_reconfigured_as_dynamic() {
        let dirs = TestDirectories::new();
        let state = OnceLock::new();
        state.set(Ok(InitializedRuntime::Static)).unwrap();
        assert!(
            initialize_dynamic(&state, &dirs.first, |_| panic!("must not initialize")).is_err()
        );
        assert!(require_dynamic_directory(&state, &dirs.first).is_err());
    }

    #[cfg(not(feature = "static-plugins"))]
    #[test]
    fn initialized_runtime_rejects_a_different_video_plugin_directory() {
        test_init();
        let dirs = TestDirectories::new();
        assert!(initialize(&dirs.second).is_err());
        assert!(load_video_plugins(&dirs.second).is_err());
        load_video_plugins(Path::new(env!("LINGXI_DEV_GST_PLUGIN_DIR"))).unwrap();
    }
}
