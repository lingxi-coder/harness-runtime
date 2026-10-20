//! Offline, on-device acceptance of shared codecs. No device APIs/model.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "static-plugins")]
    lingxi_webrtc_media::runtime::initialize_static()?;
    #[cfg(not(feature = "static-plugins"))]
    {
        let directory = std::env::args().nth(1).ok_or("provide plugin directory")?;
        let directory = std::path::Path::new(&directory);
        lingxi_webrtc_media::runtime::initialize(directory)?;
        lingxi_webrtc_media::runtime::load_video_plugins(directory)?;
    }
    let packets = lingxi_webrtc_media::diagnostics::verify_codecs()?;
    println!(
        "PASS: shared Sonora/Rubato/Opus ({packets} x 20ms packets) and GStreamer VP8 (5 frames) on {}",
        std::env::consts::OS
    );
    Ok(())
}
