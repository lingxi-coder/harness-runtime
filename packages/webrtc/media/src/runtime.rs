//! Explicit, process-once media initialization. No environment mutation, plugin
//! scanning subprocess, global teardown, or executable-relative helper paths.
use gstreamer::{self as gst, glib};
use std::{ffi::CString, path::Path, sync::OnceLock};
static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();
pub fn initialize(plugin_directory: &Path) -> Result<(), String> {
    INITIALIZED
        .get_or_init(|| initialize_inner(plugin_directory))
        .clone()
}
fn initialize_inner(directory: &Path) -> Result<(), String> {
    let directory = directory
        .canonicalize()
        .map_err(|_| "media plugin directory unavailable")?;
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
        gst::Plugin::load_file(plugin_file(&directory, name))
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
    INITIALIZED
        .get_or_init(|| {
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
            Ok(())
        })
        .clone()
}
pub(crate) fn ensure_initialized() -> Result<(), &'static str> {
    match INITIALIZED.get() {
        Some(Ok(())) => Ok(()),
        _ => Err("initialize media runtime before opening devices"),
    }
}
pub fn load_video_plugins(directory: &Path) -> Result<(), String> {
    ensure_initialized().map_err(str::to_owned)?;
    for name in ["vpx", "videoconvertscale"] {
        gst::Plugin::load_file(plugin_file(directory, name))
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
