//! Native embedding seam. The platform owns devices, permissions and lifecycle;
//! processing, codecs and transport stay in the shared Rust crates.
mod ice_config;
mod probe_session;
const MEDIA_WORKER_STACK: usize = 16 * 1024 * 1024;

/// Run on a background diagnostic worker, never on a device or UI callback.
/// Returns the verified number of 20 ms Opus packets, or a fixed negative code.
/// No model/network/hardware is used by this fixture.
#[unsafe(no_mangle)]
pub extern "C" fn lingxi_webrtc_verify_codecs() -> i32 {
    std::thread::Builder::new()
        .name("webrtc-codec-probe".into())
        .stack_size(MEDIA_WORKER_STACK)
        .spawn(|| {
            #[cfg(feature = "static-plugins")]
            if lingxi_webrtc_media::runtime::initialize_static().is_err() {
                return -1;
            }
            match lingxi_webrtc_media::diagnostics::verify_codecs() {
                Ok(packets) => i32::try_from(packets).unwrap_or(-2),
                Err(_) => -2,
            }
        })
        .map_or(-3, |thread| thread.join().unwrap_or(-3))
}
