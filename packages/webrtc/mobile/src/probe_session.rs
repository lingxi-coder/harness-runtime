//! Standalone device acceptance host. Its executor belongs to this diagnostic
//! host; product embedding uses the host's existing executor and media ports.
use crossbeam_queue::ArrayQueue;
use lingxi_webrtc_core::{IceConfig, RtcSession, SessionConfig, VideoSource};
use lingxi_webrtc_media::{
    AudioControls,
    ports::{CapturePort, NativeAudio, RenderPort},
    video_ports::{NativeVideo, Plane, VideoCapturePort, pack_rgba, pack_yuv420},
};
use std::{
    ffi::{CStr, c_char},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
static CLOCK: OnceLock<Instant> = OnceLock::new();
fn anchor() -> Instant {
    *CLOCK.get_or_init(Instant::now)
}
#[unsafe(no_mangle)]
pub extern "C" fn lingxi_webrtc_clock_ns() -> u64 {
    u64::try_from(anchor().elapsed().as_nanos()).unwrap_or(u64::MAX)
}
struct VideoOutput {
    source: u32,
    ssrc: u32,
    frame: lingxi_webrtc_media::video::VideoFrame,
}
pub struct Probe {
    capture: Option<Mutex<CapturePort>>,
    render: Option<Mutex<RenderPort>>,
    camera: VideoCapturePort,
    screen: VideoCapturePort,
    frames: ArrayQueue<VideoOutput>,
    stop: AtomicBool,
    controls: AtomicU32,
    speaker_test_requested: AtomicBool,
    state: AtomicI32,
    failure: AtomicI32,
    captured: AtomicU64,
    rendered: AtomicU64,
    video: AtomicU64,
    camera_decoded: AtomicU64,
    screen_decoded: AtomicU64,
    microphone_peak: AtomicU32,
    speaker_peak: AtomicU32,
    local_relay: AtomicU32,
    remote_relay: AtomicU32,
    relay_required: bool,
    encoded_camera: AtomicU64,
    encoded_screen: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
}
#[repr(C)]
#[derive(Default)]
pub struct ProbeStats {
    pub state: i32,
    pub failure: i32,
    pub captured_samples: u64,
    pub rendered_samples: u64,
    pub decoded_video_frames: u64,
    pub microphone_peak: u32,
    pub speaker_peak: u32,
    pub camera_decoded_frames: u64,
    pub screen_decoded_frames: u64,
    pub local_relay_candidates: u32,
    pub remote_relay_candidates: u32,
    pub relay_required: u32,
    pub encoded_camera_frames: u64,
    pub encoded_screen_frames: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VideoPlane {
    pub data: *const u8,
    pub len: usize,
    pub row_stride: usize,
    pub pixel_stride: usize,
}
#[repr(C)]
pub struct VideoMeta {
    pub width: u32,
    pub height: u32,
    pub ssrc: u32,
    pub source: u32,
    pub size: usize,
    pub offsets: [usize; 3],
    pub strides: [i32; 3],
}

async fn post(
    endpoint: &str,
    path: &str,
    body: &str,
    token: &str,
    selected_ip: Option<&mut Option<std::net::IpAddr>>,
) -> Result<Vec<u8>, i32> {
    tokio::time::timeout(Duration::from_secs(20), async {
        // iOS can reject the first socket while its local-network consent sheet
        // is pending. Keep this bounded by the outer deadline so granting the
        // permission can complete the same Start action.
        let mut attempts = 0;
        let mut socket = loop {
            match tokio::net::TcpStream::connect(endpoint).await {
                Ok(socket) => break socket,
                Err(error) if cfg!(target_os = "ios") && error.raw_os_error() == Some(65) && attempts < 20 => {
                    attempts += 1;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => return Err(-1000-error.raw_os_error().unwrap_or(99)),
            }
        };
        if let Some(selected_ip)=selected_ip {*selected_ip=Some(socket.local_addr().map_err(|_|-1100)?.ip());}
        let request = format!("POST {path} HTTP/1.1\r\nHost: {endpoint}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len());
        socket.write_all(request.as_bytes()).await.map_err(|_| -1101)?;
        let mut reply = Vec::new();
        socket.take(131073).read_to_end(&mut reply).await.map_err(|_| -1102)?;
        if reply.len()>131072 { return Err(-1103); }
        let split=reply.windows(4).position(|v| v==b"\r\n\r\n").ok_or(-1104)?;
        if !reply.starts_with(b"HTTP/1.1 2") {
            let status=std::str::from_utf8(&reply[..split]).ok().and_then(|header|header.split_whitespace().nth(1)).and_then(|s|s.parse::<i32>().ok()).unwrap_or(0);
            return Err(-2000-status);
        }
        Ok(reply[split+4..].to_vec())
    }).await.map_err(|_| -1199)?
}
async fn connect(
    endpoint: &str,
    token: &str,
    ice: IceConfig,
    probe: &Probe,
) -> Result<RtcSession, i32> {
    // The diagnostic server and TURN endpoint share this LAN route. Bind the
    // interface actually selected by signaling instead of waiting on unrelated
    // cellular/VPN allocations that cannot reach the local test server.
    let mut selected_ip = None;
    let offer: serde_json::Value = serde_json::from_slice(
        &post(endpoint, "/offer", "{}", token, Some(&mut selected_ip)).await?,
    )
    .map_err(|_| -1201)?;
    let required = ice.relay_only;
    let remote_sdp = offer["sdp"].as_str().ok_or(-1203)?;
    probe.remote_relay.store(
        crate::ice_config::relay_candidates(remote_sdp, required)?,
        Ordering::Release,
    );
    let mut peer = RtcSession::with_config(SessionConfig {
        bind_addresses: vec![std::net::SocketAddr::new(selected_ip.ok_or(-1204)?, 0).to_string()],
        video: true,
        answerer: true,
        ice,
        ..Default::default()
    })
    .await
    .map_err(|_| -1202)?;
    let result = async {
        let answer = peer
            .answer(offer["sdp"].as_str().ok_or(-1203)?.into())
            .await
            .map_err(|error| match error {
                "too many remote candidates" => -1210,
                "failed to apply remote offer" => -1211,
                "failed to create answer" => -1212,
                "answer gathering timed out" => -1213,
                _ => -1219,
            })?;
        probe.local_relay.store(
            crate::ice_config::relay_candidates(&answer, required)?,
            Ordering::Release,
        );
        let body = serde_json::json!({"id":offer["id"],"sdp":answer}).to_string();
        post(endpoint, "/answer", &body, token, None).await?;
        peer.wait_ready().await.map_err(|_| -1220)?;
        let bytes: Vec<u8> = (0..16384).map(|n| (n % 256) as u8).collect();
        peer.send_bytes(&bytes).await.map_err(|_| -1221)?;
        let message = tokio::time::timeout(Duration::from_secs(10), peer.receive())
            .await
            .map_err(|_| -1222)?
            .ok_or(-1223)?;
        if message.is_string || message.data.as_ref() != bytes.as_slice() {
            return Err(-1224);
        }
        Ok(())
    }
    .await;
    if let Err(error) = result {
        let _ = peer.close().await;
        return Err(error);
    }
    Ok(peer)
}

fn run(
    probe: Arc<Probe>,
    mut audio: Option<NativeAudio>,
    mut video: NativeVideo,
    endpoint: String,
    token: String,
    ice: IceConfig,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    else {
        probe.failure.store(-11, Ordering::Release);
        probe.state.store(-1, Ordering::Release);
        return;
    };
    runtime.block_on(async {
        let mut peer = match connect(&endpoint, &token, ice, &probe).await {
            Ok(peer) => peer,
            Err(code) => {
                probe.failure.store(code, Ordering::Release);
                probe.state.store(-1, Ordering::Release);
                return;
            }
        };
        let sources = peer.remote_video_sources().await.unwrap_or_default();
        probe.state.store(1, Ordering::Release);
        let mut previous = u32::MAX;
        // A running microphone callback does not prove the remote peer is alive.
        // The fixture echoes these binary records without changing their bytes.
        const HEARTBEAT: &[u8] = b"lingxi-webrtc-device-heartbeat-v1";
        let mut heartbeat_due = Instant::now() + Duration::from_secs(5);
        let mut heartbeat_wait = None::<Instant>;
        while !probe.stop.load(Ordering::Acquire) {
            if probe.speaker_test_requested.swap(false, Ordering::AcqRel) {
                if !matches!(
                    tokio::time::timeout(
                        Duration::from_millis(250),
                        peer.send_bytes(b"lingxi-webrtc-speaker-test-v1")
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    probe.failure.store(-1225, Ordering::Release);
                    break;
                }
            }
            if !peer.is_ready() {
                probe.failure.store(-1225, Ordering::Release);
                break;
            }
            while let Some(message) = peer.try_receive() {
                if !message.is_string && message.data.as_ref() == HEARTBEAT {
                    heartbeat_wait = None;
                }
            }
            let now = Instant::now();
            if heartbeat_wait.is_some_and(|deadline| now >= deadline) {
                probe.failure.store(-1226, Ordering::Release);
                break;
            }
            if now >= heartbeat_due && heartbeat_wait.is_none() {
                if !matches!(
                    tokio::time::timeout(Duration::from_millis(250), peer.send_bytes(HEARTBEAT))
                        .await,
                    Ok(Ok(()))
                ) {
                    probe.failure.store(-1225, Ordering::Release);
                    break;
                }
                heartbeat_wait = Some(now + Duration::from_secs(10));
                heartbeat_due = now + Duration::from_secs(5);
            }
            let controls = probe.controls.load(Ordering::Acquire);
            if controls != previous {
                if audio.as_mut().is_some_and(|audio| {
                    audio
                        .controls(AudioControls {
                            microphone_muted: controls & 1 == 0,
                            speaker_suppressed: controls & 2 == 0,
                        })
                        .is_err()
                }) || video
                    .controls(controls & 4 != 0, controls & 8 != 0, controls & 16 != 0)
                    .is_err()
                {
                    probe.failure.store(-12, Ordering::Release);
                    break;
                }
                previous = controls;
            }
            if let Some(audio) = audio.as_mut() {
                if audio.service(&mut peer).await.is_err() {
                    probe.failure.store(-12, Ordering::Release);
                    break;
                }
            } else {
                let _ = peer.set_playback_suppressed(true);
            }
            match video.service(&mut peer).await {
                Ok(frames) => {
                    for frame in frames {
                        let source = match sources.get(&frame.ssrc) {
                            Some(VideoSource::Camera) => 1,
                            Some(VideoSource::Screen) => 2,
                            None => 0,
                        };
                        probe.video.fetch_add(1, Ordering::Relaxed);
                        match source {
                            1 => {
                                probe.camera_decoded.fetch_add(1, Ordering::Relaxed);
                            }
                            2 => {
                                probe.screen_decoded.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                        probe.frames.force_push(VideoOutput {
                            source,
                            ssrc: frame.ssrc,
                            frame: frame.frame,
                        });
                    }
                }
                Err(_) => {
                    probe.failure.store(-12, Ordering::Release);
                    break;
                }
            }
            let counts = video.encoded_frames();
            probe.encoded_camera.store(counts.0, Ordering::Relaxed);
            probe.encoded_screen.store(counts.1, Ordering::Relaxed);
            if let Some(Ok(state)) = audio.as_mut().map(|audio| audio.state()) {
                probe
                    .microphone_peak
                    .fetch_max(u32::from(state.microphone_peak), Ordering::Relaxed);
                probe
                    .speaker_peak
                    .fetch_max(u32::from(state.speaker_peak), Ordering::Relaxed);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = peer.close().await;
        probe.state.store(
            if probe.failure.load(Ordering::Acquire) == 0 {
                0
            } else {
                -1
            },
            Ordering::Release,
        );
    });
}

/// # Safety
/// Strings must be valid NUL-terminated UTF-8. Returned handle is owned by the
/// caller until close; all callbacks must stop before close runs off the UI thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_open(
    endpoint: *const c_char,
    token: *const c_char,
    capture_rate: u32,
    render_rate: u32,
    ice_json: *const u8,
    ice_len: usize,
) -> *mut Arc<Probe> {
    unsafe {
        open(
            endpoint,
            token,
            capture_rate,
            render_rate,
            ice_json,
            ice_len,
            false,
        )
    }
}
/// Video-only host for ReplayKit's independent broadcast-extension lifecycle.
/// No microphone, speaker, AEC, resampler or returned-video decoder is created.
/// # Safety
/// Same pointer and close ownership contract as probe_open.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_broadcast_open(
    endpoint: *const c_char,
    token: *const c_char,
    ice_json: *const u8,
    ice_len: usize,
) -> *mut Arc<Probe> {
    unsafe { open(endpoint, token, 48000, 48000, ice_json, ice_len, true) }
}
unsafe fn open(
    endpoint: *const c_char,
    token: *const c_char,
    capture_rate: u32,
    render_rate: u32,
    ice_json: *const u8,
    ice_len: usize,
    video_only: bool,
) -> *mut Arc<Probe> {
    if ice_len > 65536 || (ice_len != 0 && ice_json.is_null()) {
        return std::ptr::null_mut();
    }
    let bytes = if ice_len == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(ice_json, ice_len) }
    };
    let Ok(ice) = crate::ice_config::parse(bytes) else {
        return std::ptr::null_mut();
    };
    if endpoint.is_null() || token.is_null() {
        return std::ptr::null_mut();
    }
    let (Ok(endpoint), Ok(token)) = (
        unsafe { CStr::from_ptr(endpoint) }.to_str(),
        unsafe { CStr::from_ptr(token) }.to_str(),
    ) else {
        return std::ptr::null_mut();
    };
    if endpoint.len() > 256
        || !(16..=128).contains(&token.len())
        || endpoint.chars().chain(token.chars()).any(char::is_control)
    {
        return std::ptr::null_mut();
    }
    #[cfg(feature = "static-plugins")]
    if lingxi_webrtc_media::runtime::initialize_static().is_err() {
        return std::ptr::null_mut();
    }
    let _ = anchor();
    let endpoint = endpoint.to_owned();
    let token = token.to_owned();
    let (created, receiver) = std::sync::mpsc::sync_channel(1);
    let Ok(thread) = std::thread::Builder::new()
        .name("webrtc-device-probe".into())
        .stack_size(crate::MEDIA_WORKER_STACK)
        .spawn(move || {
            // Construct AEC on this worker, never on a 512 KiB GCD/JNI stack.
            let (audio, capture, render) = if video_only {
                (None, None, None)
            } else {
                let Ok((audio, capture, render)) =
                    NativeAudio::new(capture_rate, render_rate, 1024, 1024)
                else {
                    let _ = created.send(None);
                    return;
                };
                (
                    Some(audio),
                    Some(Mutex::new(capture)),
                    Some(Mutex::new(render)),
                )
            };
            let (video, camera, screen) = NativeVideo::new();
            let probe = Arc::new(Probe {
                capture,
                render,
                camera,
                screen,
                frames: ArrayQueue::new(2),
                stop: AtomicBool::new(false),
                controls: AtomicU32::new(0),
                speaker_test_requested: AtomicBool::new(false),
                state: AtomicI32::new(0),
                failure: AtomicI32::new(0),
                captured: AtomicU64::new(0),
                rendered: AtomicU64::new(0),
                video: AtomicU64::new(0),
                camera_decoded: AtomicU64::new(0),
                screen_decoded: AtomicU64::new(0),
                microphone_peak: AtomicU32::new(0),
                speaker_peak: AtomicU32::new(0),
                local_relay: AtomicU32::new(0),
                remote_relay: AtomicU32::new(0),
                relay_required: ice.relay_only,
                encoded_camera: AtomicU64::new(0),
                encoded_screen: AtomicU64::new(0),
                thread: Mutex::new(None),
            });
            if created.send(Some(probe.clone())).is_err() {
                return;
            }
            run(probe, audio, video, endpoint, token, ice);
        })
    else {
        return std::ptr::null_mut();
    };
    let Ok(Some(probe)) = receiver.recv_timeout(Duration::from_secs(5)) else {
        return std::ptr::null_mut();
    };
    *probe.thread.lock().unwrap() = Some(thread);
    Box::into_raw(Box::new(probe))
}
/// # Safety
/// The handle must still be owned and all native callbacks must have stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_close(handle: *mut Arc<Probe>) {
    if handle.is_null() {
        return;
    }
    let probe = unsafe { Box::from_raw(handle) };
    probe.stop.store(true, Ordering::Release);
    if let Ok(mut thread) = probe.thread.lock() {
        if let Some(thread) = thread.take() {
            let _ = thread.join();
        }
    }
}
/// # Safety
/// Valid retained handle; bits: microphone=1, speaker=2, camera=4, screen=8, video receive=16.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_controls(handle: *const Arc<Probe>, bits: u32) {
    if let Some(probe) = unsafe { handle.as_ref() } {
        if bits & 32 != 0 {
            probe.speaker_test_requested.store(true, Ordering::Release);
        }
        probe.controls.store(bits & 31, Ordering::Release);
    }
}
/// # Safety
/// Valid retained handle and writable stats pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_stats(
    handle: *const Arc<Probe>,
    out: *mut ProbeStats,
) -> i32 {
    let (Some(probe), Some(out)) = (unsafe { handle.as_ref() }, unsafe { out.as_mut() }) else {
        return -1;
    };
    *out = ProbeStats {
        state: probe.state.load(Ordering::Acquire),
        failure: probe.failure.load(Ordering::Acquire),
        captured_samples: probe.captured.load(Ordering::Relaxed),
        rendered_samples: probe.rendered.load(Ordering::Relaxed),
        decoded_video_frames: probe.video.load(Ordering::Relaxed),
        microphone_peak: probe.microphone_peak.load(Ordering::Relaxed),
        speaker_peak: probe.speaker_peak.load(Ordering::Relaxed),
        camera_decoded_frames: probe.camera_decoded.load(Ordering::Relaxed),
        screen_decoded_frames: probe.screen_decoded.load(Ordering::Relaxed),
        local_relay_candidates: probe.local_relay.load(Ordering::Acquire),
        remote_relay_candidates: probe.remote_relay.load(Ordering::Acquire),
        relay_required: u32::from(probe.relay_required),
        encoded_camera_frames: probe.encoded_camera.load(Ordering::Relaxed),
        encoded_screen_frames: probe.encoded_screen.load(Ordering::Relaxed),
    };
    0
}
/// # Safety
/// The retained handle and `count` readable floats must remain valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_capture(
    handle: *const Arc<Probe>,
    pcm: *const f32,
    count: usize,
    captured_ns: u64,
    callback_ns: u64,
) -> i32 {
    let Some(probe) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if pcm.is_null()
        || count == 0
        || count > 8192
        || captured_ns > callback_ns
        || callback_ns > lingxi_webrtc_clock_ns().saturating_add(1_000_000)
    {
        return -1;
    }
    let Some(capture) = probe.capture.as_ref() else {
        return -1;
    };
    let Ok(mut port) = capture.try_lock() else {
        return -2;
    };
    let Some(at) = anchor().checked_add(Duration::from_nanos(captured_ns)) else {
        return -1;
    };
    let pcm = unsafe { std::slice::from_raw_parts(pcm, count) };
    match port.submit(
        pcm,
        at,
        Duration::from_nanos(callback_ns),
        Duration::from_nanos(captured_ns),
    ) {
        Ok(()) => {
            probe.captured.fetch_add(count as u64, Ordering::Relaxed);
            0
        }
        Err(_) => -1,
    }
}
/// # Safety
/// The retained handle and `count` writable floats must remain valid for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_render(
    handle: *const Arc<Probe>,
    pcm: *mut f32,
    count: usize,
    playback_ns: u64,
) -> i32 {
    if pcm.is_null() || count > 8192 {
        return -1;
    }
    let output = unsafe { std::slice::from_raw_parts_mut(pcm, count) };
    output.fill(0.);
    let Some(probe) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if playback_ns > lingxi_webrtc_clock_ns().saturating_add(1_000_000_000) {
        return -1;
    }
    let Some(render) = probe.render.as_ref() else {
        return -1;
    };
    let Ok(mut port) = render.try_lock() else {
        return -2;
    };
    let Some(at) = anchor().checked_add(Duration::from_nanos(playback_ns)) else {
        return -1;
    };
    match port.render(output, at) {
        Ok(()) => {
            probe.rendered.fetch_add(count as u64, Ordering::Relaxed);
            0
        }
        Err(_) => -1,
    }
}
/// # Safety
/// Three readable plane descriptors and their byte ranges must stay valid for
/// this synchronous copy. Source is camera=1 or screen=2; timestamps use clock_ns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_video(
    handle: *const Arc<Probe>,
    source: u32,
    width: u32,
    height: u32,
    planes: *const VideoPlane,
    captured_ns: u64,
) -> i32 {
    let Some(probe) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if planes.is_null() || captured_ns > lingxi_webrtc_clock_ns() {
        return -1;
    }
    let planes = unsafe { std::slice::from_raw_parts(planes, 3) };
    if planes
        .iter()
        .any(|p| p.data.is_null() || p.len > 4 * 1024 * 1024)
    {
        return -1;
    }
    let planes = std::array::from_fn(|n| Plane {
        bytes: unsafe { std::slice::from_raw_parts(planes[n].data, planes[n].len) },
        row_stride: planes[n].row_stride,
        pixel_stride: planes[n].pixel_stride,
    });
    let Ok(frame) = pack_yuv420(width, height, planes) else {
        return -1;
    };
    let Some(at) = anchor().checked_add(Duration::from_nanos(captured_ns)) else {
        return -1;
    };
    let port = match source {
        1 => &probe.camera,
        2 => &probe.screen,
        _ => return -1,
    };
    if port.submit(frame, at).is_ok() {
        0
    } else {
        -1
    }
}
/// # Safety
/// Valid retained handle, writable metadata and `capacity` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_rgba(
    handle: *const Arc<Probe>,
    source: u32,
    width: u32,
    height: u32,
    plane: VideoPlane,
    bgra: i32,
    captured_ns: u64,
) -> i32 {
    let Some(probe) = (unsafe { handle.as_ref() }) else {
        return -1;
    };
    if plane.data.is_null()
        || plane.len > 16 * 1024 * 1024
        || captured_ns > lingxi_webrtc_clock_ns()
    {
        return -1;
    }
    let plane = Plane {
        bytes: unsafe { std::slice::from_raw_parts(plane.data, plane.len) },
        row_stride: plane.row_stride,
        pixel_stride: plane.pixel_stride,
    };
    let Ok(frame) = pack_rgba(width, height, plane, bgra != 0) else {
        return -1;
    };
    let Some(at) = anchor().checked_add(Duration::from_nanos(captured_ns)) else {
        return -1;
    };
    let port = match source {
        1 => &probe.camera,
        2 => &probe.screen,
        _ => return -1,
    };
    if port.submit(frame, at).is_ok() {
        0
    } else {
        -1
    }
}
/// # Safety
/// Valid retained handle, writable metadata and `capacity` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lingxi_webrtc_probe_take_video(
    handle: *const Arc<Probe>,
    out: *mut VideoMeta,
    pixels: *mut u8,
    capacity: usize,
) -> i32 {
    let (Some(probe), Some(out)) = (unsafe { handle.as_ref() }, unsafe { out.as_mut() }) else {
        return -1;
    };
    let Some(video) = probe.frames.pop() else {
        return 0;
    };
    if pixels.is_null() || capacity < video.frame.pixels.len() {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            video.frame.pixels.as_ptr(),
            pixels,
            video.frame.pixels.len(),
        )
    };
    let layout = video.frame.layout;
    *out = VideoMeta {
        width: layout.width,
        height: layout.height,
        ssrc: video.ssrc,
        source: video.source,
        size: video.frame.pixels.len(),
        offsets: layout.offsets.try_into().unwrap(),
        strides: layout.strides.try_into().unwrap(),
    };
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn media_initialization_is_safe_on_a_small_native_callback_stack() {
        let plugins =
            std::env::var_os("LINGXI_MEDIA_TEST_PLUGIN_DIR").expect("run scripts/test-media.py");
        lingxi_webrtc_media::runtime::initialize(std::path::Path::new(&plugins)).unwrap();
        lingxi_webrtc_media::runtime::load_video_plugins(std::path::Path::new(&plugins)).unwrap();
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let endpoint = std::ffi::CString::new("127.0.0.1:1").unwrap();
                let token = std::ffi::CString::new("local-webrtc-test-token").unwrap();
                let handle = unsafe {
                    lingxi_webrtc_probe_open(
                        endpoint.as_ptr(),
                        token.as_ptr(),
                        48000,
                        48000,
                        std::ptr::null(),
                        0,
                    )
                };
                assert!(!handle.is_null());
                unsafe { lingxi_webrtc_probe_close(handle) };
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

#[cfg(test)]
mod broadcast_tests {
    use super::*;
    #[test]
    fn broadcast_host_owns_no_audio_ports() {
        let endpoint = std::ffi::CString::new("127.0.0.1:1").unwrap();
        let token = std::ffi::CString::new("local-test-token-1234").unwrap();
        let handle = unsafe {
            lingxi_webrtc_broadcast_open(endpoint.as_ptr(), token.as_ptr(), std::ptr::null(), 0)
        };
        assert!(!handle.is_null());
        let probe = unsafe { &*handle };
        assert!(probe.capture.is_none());
        assert!(probe.render.is_none());
        let pcm = [0f32; 480];
        assert_eq!(
            unsafe { lingxi_webrtc_probe_capture(handle, pcm.as_ptr(), 480, 0, 0) },
            -1
        );
        unsafe { lingxi_webrtc_probe_close(handle) };
    }
}
