//! One complete ScreenCaptureKit frame. No legacy CoreGraphics capture fallback.
use crate::error::{XCapError, XCapResult};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use image::RgbaImage;
use objc2::{
    AllocAnyThread, DefinedClass, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayMode, CGWindowID, CGWindowListOption,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::*;
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::*;
use std::{
    sync::{
        Mutex,
        mpsc::{SyncSender, sync_channel},
    },
    time::Duration,
};
const MAX_PIXELS: usize = 33_554_432;
const WAIT: Duration = Duration::from_secs(2);
fn error(stage: &str) -> XCapError {
    XCapError::new(format!("ScreenCaptureKit {stage}"))
}
fn native_error(stage: &str, e: &NSError) -> XCapError {
    XCapError::new(format!(
        "ScreenCaptureKit {stage}: {} ({})",
        e.domain(),
        e.code()
    ))
}
// SCShareableContent is an immutable snapshot delivered on the framework queue.
// Retain before leaving that callback, then inspect it only on the capture worker.
struct Content(Retained<SCShareableContent>);
unsafe impl Send for Content {}
#[derive(Debug)]
struct OutputState {
    sender: Mutex<Option<SyncSender<XCapResult<RgbaImage>>>>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "LingXiSCKSingleFrameOutput"]
    #[ivars = OutputState]
    #[derive(Debug)]
    struct Output;
    unsafe impl NSObjectProtocol for Output {}
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn frame(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            autoreleasepool(|_| {
                // Idle/blank/suspended frames are not a successful screenshot.
                if !unsafe { sample.is_valid() && sample.data_is_ready() } {
                    return;
                }
                let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
                    return;
                };
                if attachments.len() == 0 {
                    return;
                }
                let raw = unsafe { attachments.value_at_index(0) };
                if raw.is_null() {
                    return;
                }
                // ScreenCaptureKit supplies a toll-free bridged dictionary here.
                let info = unsafe { &*raw.cast::<NSDictionary<NSString, NSNumber>>() };
                let Some(status) = info.objectForKey(unsafe { SCStreamFrameInfoStatus }) else {
                    return;
                };
                if status.integerValue() != 0 {
                    return;
                } // SCFrameStatusComplete
                let mut sender = match self.ivars().sender.lock() {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let Some(sender) = sender.take() else {
                    return;
                };
                let _ = sender.try_send(unsafe { copy_frame(sample) });
            });
        }
    }
);
impl Output {
    fn new(sender: SyncSender<XCapResult<RgbaImage>>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputState {
            sender: Mutex::new(Some(sender)),
        });
        unsafe { msg_send![super(this), init] }
    }
    fn close(&self) {
        if let Ok(mut s) = self.ivars().sender.lock() {
            s.take();
        }
    }
}
fn packed_rgba(data: &[u8], width: usize, height: usize, stride: usize) -> XCapResult<RgbaImage> {
    let row = width.checked_mul(4).ok_or_else(|| error("row overflow"))?;
    let size = stride
        .checked_mul(height)
        .ok_or_else(|| error("buffer overflow"))?;
    if width == 0
        || height == 0
        || width.checked_mul(height).is_none_or(|n| n > MAX_PIXELS)
        || stride < row
        || data.len() < size
    {
        return Err(error("invalid BGRA frame layout"));
    }
    let mut pixels = Vec::with_capacity(row * height);
    for source in data[..size].chunks_exact(stride) {
        for p in source[..row].chunks_exact(4) {
            // SCK BGRA is premultiplied. RgbaImage callers expect straight alpha.
            let a = u32::from(p[3]);
            let straight = |c: u8| {
                if a == 0 {
                    0
                } else {
                    ((u32::from(c) * 255 + a / 2) / a).min(255) as u8
                }
            };
            pixels.extend_from_slice(&[straight(p[2]), straight(p[1]), straight(p[0]), p[3]]);
        }
    }
    RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| error("invalid frame dimensions"))
}
unsafe fn copy_frame(sample: &CMSampleBuffer) -> XCapResult<RgbaImage> {
    let buffer = unsafe { sample.image_buffer() }.ok_or_else(|| error("missing pixel buffer"))?;
    if CVPixelBufferIsPlanar(&buffer)
        || CVPixelBufferGetPixelFormatType(&buffer) != kCVPixelFormatType_32BGRA
    {
        return Err(error("unexpected pixel format"));
    }
    let flags = CVPixelBufferLockFlags::ReadOnly;
    if unsafe { CVPixelBufferLockBaseAddress(&buffer, flags) } != 0 {
        return Err(error("pixel lock failed"));
    }
    scopeguard::defer! { unsafe { CVPixelBufferUnlockBaseAddress(&buffer, flags); } }
    let width = CVPixelBufferGetWidth(&buffer);
    let height = CVPixelBufferGetHeight(&buffer);
    let stride = CVPixelBufferGetBytesPerRow(&buffer);
    let length = stride
        .checked_mul(height)
        .ok_or_else(|| error("pixel layout overflow"))?;
    let data_size = CVPixelBufferGetDataSize(&buffer);
    let ptr = CVPixelBufferGetBaseAddress(&buffer);
    if ptr.is_null() || length > data_size || length > MAX_PIXELS * 8 {
        return Err(error("pixel buffer unavailable or oversized"));
    }
    packed_rgba(
        unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), length) },
        width,
        height,
        stride,
    )
}
// Block-owned keepalive outlives the caller's bounded wait. In particular,
// dropping a Receiver does not release output/queue while native stop is pending.
fn completion_with_keepalive<K: 'static>(
    keepalive: K,
    after_stop: impl Fn() -> XCapResult<()> + 'static,
) -> (
    RcBlock<dyn Fn(*mut NSError)>,
    std::sync::mpsc::Receiver<XCapResult<()>>,
) {
    let (tx, rx) = sync_channel(1);
    (
        RcBlock::new(move |e: *mut NSError| {
            let _keepalive = &keepalive;
            let result = match unsafe { e.as_ref() } {
                None => after_stop(),
                Some(e) => Err(native_error("stream stop", e)),
            };
            let _ = tx.try_send(result);
        }),
        rx,
    )
}
fn stop_completion(
    stream: &Retained<SCStream>,
    output: &Retained<Output>,
    queue: &dispatch2::DispatchRetained<DispatchQueue>,
) -> (
    RcBlock<dyn Fn(*mut NSError)>,
    std::sync::mpsc::Receiver<XCapResult<()>>,
) {
    let stream = stream.clone();
    let output = output.clone();
    completion_with_keepalive(queue.clone(), move || {
        unsafe {
            stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
            )
        }
        .map_err(|e| native_error("remove stream output", &e))
    })
}
struct Session {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _queue: dispatch2::DispatchRetained<DispatchQueue>,
    finished: bool,
}
impl Session {
    fn finish(&mut self) -> XCapResult<()> {
        self.output.close();
        let (done, rx) = stop_completion(&self.stream, &self.output, &self._queue);
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(&done));
        }
        let result = rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| error("stream stop timeout"))?;
        self.finished = result.is_ok();
        result
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish();
        }
    }
}
/// For a window, `id` is its exact CGWindowID; for display/region it is the
/// exact CGDirectDisplayID supplied by ImplMonitor (never rectangle fallback).
pub fn capture(rect: CGRect, option: CGWindowListOption, id: CGWindowID) -> XCapResult<RgbaImage> {
    autoreleasepool(|_| unsafe { capture_inner(rect, option, id) })
}
unsafe fn capture_inner(
    rect: CGRect,
    option: CGWindowListOption,
    id: u32,
) -> XCapResult<RgbaImage> {
    // Reject invalid geometry before invoking any OS capture API.
    dimensions(rect, 1.0)?;
    let (tx, rx) = sync_channel(1);
    let done = RcBlock::new(move |content: *mut SCShareableContent, e: *mut NSError| {
        let result = if let Some(e) = unsafe { e.as_ref() } {
            Err(native_error("shareable content", e))
        } else {
            unsafe { Retained::retain(content) }
                .map(Content)
                .ok_or_else(|| error("missing shareable content"))
        };
        let _ = tx.try_send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false, false, &done);
    }
    let content = rx
        .recv_timeout(WAIT)
        .map_err(|_| error("content enumeration timeout"))??;
    let config = unsafe { SCStreamConfiguration::new() };
    let (filter, scale) = if option == CGWindowListOption::OptionIncludingWindow {
        let window = unsafe { content.0.windows() }
            .into_iter()
            .find(|w| unsafe { w.windowID() == id })
            .ok_or_else(|| error("requested window no longer exists"))?;
        let frame = unsafe { window.frame() };
        if frame != rect {
            return Err(error("window geometry changed before capture"));
        }
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
        };
        if !filter.respondsToSelector(objc2::sel!(pointPixelScale)) {
            return Err(error("window pixel scale requires macOS 14 or later"));
        }
        let scale = unsafe { filter.pointPixelScale() } as f64;
        (filter, scale)
    } else if option == CGWindowListOption::OptionAll {
        let display = unsafe { content.0.displays() }
            .into_iter()
            .find(|d| unsafe { d.displayID() == id })
            .ok_or_else(|| error("requested display no longer exists"))?;
        let frame = CGDisplayBounds(id);
        if rect.origin.x < frame.origin.x
            || rect.origin.y < frame.origin.y
            || rect.origin.x + rect.size.width > frame.origin.x + frame.size.width
            || rect.origin.y + rect.size.height > frame.origin.y + frame.size.height
        {
            return Err(error("crop outside requested display"));
        }
        let mode = CGDisplayCopyDisplayMode(id).ok_or_else(|| error("display mode unavailable"))?;
        let scale = CGDisplayMode::pixel_width(Some(&mode)) as f64 / frame.size.width;
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };
        unsafe {
            config.setSourceRect(CGRect {
                origin: CGPoint {
                    x: rect.origin.x - frame.origin.x,
                    y: rect.origin.y - frame.origin.y,
                },
                size: rect.size,
            });
        }
        (filter, scale)
    } else {
        return Err(error("unsupported capture selection"));
    };
    let (width, height) = dimensions(rect, scale)?;
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setShowsCursor(false);
        config.setCapturesAudio(false);
        config.setQueueDepth(3);
        config.setScalesToFit(true);
    }
    let (tx, rx) = sync_channel(1);
    let output = Output::new(tx);
    let queue = DispatchQueue::new("com.lingxi.computer.sck-frame", DispatchQueueAttr::SERIAL);
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None)
    };
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*output),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|e| native_error("add stream output", &e))?;
    let mut session = Session {
        stream,
        output,
        _queue: queue,
        finished: false,
    };
    // If start completes after its caller times out, stop again from that
    // completion. A late successful start must not leave a background stream.
    let abandoned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let late = abandoned.clone();
    let late_stream = session.stream.clone();
    let late_output = session.output.clone();
    let late_queue = session._queue.clone();
    let (tx, started) = sync_channel(1);
    let done = RcBlock::new(move |e: *mut NSError| {
        if late.load(std::sync::atomic::Ordering::Acquire) {
            let (stopped, _ignored) = stop_completion(&late_stream, &late_output, &late_queue);
            unsafe {
                late_stream.stopCaptureWithCompletionHandler(Some(&stopped));
            }
        }
        let result = match unsafe { e.as_ref() } {
            None => Ok(()),
            Some(e) => Err(native_error("stream start", e)),
        };
        let _ = tx.try_send(result);
    });
    unsafe {
        session
            .stream
            .startCaptureWithCompletionHandler(Some(&done));
    }
    let result = match started.recv_timeout(WAIT) {
        Ok(Ok(())) => rx
            .recv_timeout(WAIT)
            .map_err(|_| error("complete frame timeout"))
            .and_then(|frame| frame),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            abandoned.store(true, std::sync::atomic::Ordering::Release);
            Err(error("stream start timeout"))
        }
    };
    let cleanup = session.finish();
    match result {
        Ok(image) => {
            cleanup?;
            Ok(image)
        }
        Err(e) => Err(e),
    }
}
fn dimensions(rect: CGRect, scale: f64) -> XCapResult<(usize, usize)> {
    let w = (rect.size.width * scale).round();
    let h = (rect.size.height * scale).round();
    if !rect.origin.x.is_finite()
        || !rect.origin.y.is_finite()
        || !scale.is_finite()
        || scale <= 0.
        || !w.is_finite()
        || !h.is_finite()
        || w < 1.
        || h < 1.
        || w > 32768.
        || h > 32768.
        || w * h > MAX_PIXELS as f64
    {
        return Err(error("capture dimensions exceed budget"));
    }
    Ok((w as usize, h as usize))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn padded_rows_and_straight_alpha() {
        let image = packed_rgba(
            &[
                10, 20, 30, 255, 99, 99, 99, 99, 32, 16, 64, 128, 99, 99, 99, 99,
            ],
            1,
            2,
            8,
        )
        .unwrap();
        assert_eq!(image.as_raw(), &[30, 20, 10, 255, 128, 32, 64, 128]);
        assert_eq!(
            packed_rgba(&[90, 80, 70, 0], 1, 1, 4).unwrap().as_raw(),
            &[0, 0, 0, 0]
        );
    }
    #[test]
    fn invalid_layout_fails_closed() {
        assert!(packed_rgba(&[0; 8], 2, 1, 4).is_err());
        assert!(packed_rgba(&[0; 4], 1, 2, 4).is_err());
        assert!(packed_rgba(&[], usize::MAX, 2, usize::MAX).is_err());
    }
    #[test]
    fn physical_dimensions_are_bounded() {
        let rect = CGRect {
            origin: CGPoint::ZERO,
            size: objc2_core_foundation::CGSize {
                width: 100.,
                height: 50.,
            },
        };
        assert_eq!(dimensions(rect, 2.).unwrap(), (200, 100));
        assert!(dimensions(rect, f64::NAN).is_err());
        assert!(dimensions(rect, 0.).is_err());
    }
    #[test]
    fn teardown_closes_output_before_late_frames() {
        let (tx, rx) = sync_channel(1);
        let output = Output::new(tx);
        output.close();
        output.close();
        assert!(output.ivars().sender.lock().unwrap().is_none());
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }
    #[test]
    fn timed_out_receiver_does_not_release_native_keepalive() {
        let keepalive = std::sync::Arc::new(());
        let weak = std::sync::Arc::downgrade(&keepalive);
        let (done, rx) = completion_with_keepalive(keepalive, || Ok(()));
        drop(rx);
        assert!(weak.upgrade().is_some());
        done.call((std::ptr::null_mut(),));
        assert!(weak.upgrade().is_some());
        drop(done);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn timeout_does_not_block_late_lifecycle_callback() {
        let (done, rx) = completion_with_keepalive((), || Ok(()));
        drop(rx);
        done.call((std::ptr::null_mut(),));
        done.call((std::ptr::null_mut(),));
    }
}

// Pinned native 2.1.325 display capture uses SCScreenshotManager and returns
// CGImage directly. This path deliberately never packs pixels into RgbaImage.
struct CapturedImage(objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>);
// CGImage is immutable; retain in the callback before transferring ownership.
unsafe impl Send for CapturedImage {}

pub fn filtered_display_image(
    display_id: u32,
    allowed: &[String],
    host_root: &str,
    source_rect: Option<[f64; 4]>,
    width: u32,
    height: u32,
) -> XCapResult<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    autoreleasepool(|_| {
        filtered_display_image_inner(display_id, allowed, host_root, source_rect, width, height)
    })
}

fn filtered_display_image_inner(
    display_id: u32,
    allowed: &[String],
    host_root: &str,
    source_rect: Option<[f64; 4]>,
    width: u32,
    height: u32,
) -> XCapResult<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
    if width == 0
        || height == 0
        || width > 32768
        || height > 32768
        || u64::from(width) * u64::from(height) > MAX_PIXELS as u64
    {
        return Err(error("native target dimensions exceed budget"));
    }
    if let Some([x, y, w, h]) = source_rect {
        if ![x, y, w, h].iter().all(|n| n.is_finite()) || x < 0. || y < 0. || w <= 0. || h <= 0. {
            return Err(error("invalid native source rectangle"));
        }
    }
    let (tx, rx) = sync_channel(1);
    let done = RcBlock::new(move |content: *mut SCShareableContent, e: *mut NSError| {
        let result = if let Some(e) = unsafe { e.as_ref() } {
            Err(native_error("native content", e))
        } else {
            unsafe { Retained::retain(content) }
                .map(Content)
                .ok_or_else(|| error("missing native content"))
        };
        let _ = tx.try_send(result);
    });
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false,false,&done);
    }
    let content = rx
        .recv_timeout(WAIT)
        .map_err(|_| error("native content timeout"))??;
    let display = unsafe { content.0.displays() }
        .into_iter()
        .find(|d| unsafe { d.displayID() == display_id })
        .ok_or_else(|| error("native display unavailable"))?;
    let excluded = unsafe { content.0.applications() }
        .into_iter()
        .filter(|app| {
            let id = unsafe { app.bundleIdentifier() }.to_string();
            let pid = unsafe { app.processID() };
            // The authenticated host's root family replaces Claude's own identity;
            // helper bundle identities must not expose the main LingXi window.
            if is_host_family(&id, host_root) {
                return true;
            }
            if id.is_empty() {
                if NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_none() {
                    return false;
                }
                return resolve_bundle_ancestor(pid).is_none_or(|id| !allowed.contains(&id));
            }
            if allowed.contains(&id) || SYSTEM_CHROME.contains(&id.as_str()) {
                return false;
            }
            NSRunningApplication::runningApplicationWithProcessIdentifier(pid).is_none_or(|app| {
                app.activationPolicy() != NSApplicationActivationPolicy::Prohibited
            })
        })
        .collect::<Vec<_>>();
    let refs = excluded.iter().map(|app| &**app).collect::<Vec<_>>();
    let excluded = NSArray::from_slice(&refs);
    let filter = unsafe {
        SCContentFilter::initWithDisplay_excludingApplications_exceptingWindows(
            SCContentFilter::alloc(),
            &display,
            &excluded,
            &NSArray::new(),
        )
    };
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width as usize);
        config.setHeight(height as usize);
        config.setShowsCursor(false);
        config.setCapturesAudio(false);
        if let Some([x, y, w, h]) = source_rect {
            config.setSourceRect(CGRect {
                origin: CGPoint { x, y },
                size: objc2_core_foundation::CGSize {
                    width: w,
                    height: h,
                },
            });
            config.setScalesToFit(true);
        }
    }
    native_capture_image(&filter, &config, width, height)
}

fn native_capture_image(
    filter: &SCContentFilter,
    config: &SCStreamConfiguration,
    width: u32,
    height: u32,
) -> XCapResult<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    use objc2_core_foundation::CFRetained;
    use objc2_core_graphics::CGImage;
    use std::ptr::NonNull;
    let (tx, rx) = sync_channel(1);
    let done = RcBlock::new(move |image: *mut CGImage, e: *mut NSError| {
        let result = if let Some(e) = unsafe { e.as_ref() } {
            Err(native_error("native image", e))
        } else {
            NonNull::new(image)
                .map(|p| CapturedImage(unsafe { CFRetained::retain(p) }))
                .ok_or_else(|| error("missing native image"))
        };
        let _ = tx.try_send(result);
    });
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &config,
            Some(&done),
        );
    }
    let image = rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| error("native image timeout"))??;
    if CGImage::width(Some(&image.0)) != width as usize
        || CGImage::height(Some(&image.0)) != height as usize
    {
        return Err(error("native image dimension mismatch"));
    }
    Ok(image.0)
}

const SYSTEM_CHROME: [&str; 8] = [
    "com.apple.dock",
    "com.apple.wallpaper.agent",
    "com.apple.controlcenter",
    "com.apple.SystemUIServer",
    "com.apple.TextInputSwitcher",
    "com.apple.wifi.WiFiAgent",
    "com.apple.AccessibilityUIServer",
    "com.apple.loginwindow",
];
fn is_host_family(id: &str, root: &str) -> bool {
    id == root
        || id
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('.'))
}
fn resolve_bundle_ancestor(mut pid: i32) -> Option<String> {
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut std::ffi::c_void,
            size: i32,
        ) -> i32;
    }
    for _ in 0..8 {
        if pid < 2 {
            return None;
        }
        if let Some(id) =
            objc2_app_kit::NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
                .and_then(|app| app.bundleIdentifier())
        {
            return Some(id.to_string());
        }
        // Official proc_bsdinfo: 136 bytes, pbi_ppid offset16. The reference
        // calls this same flavor3/size0x88 API and bounds ancestry at8.
        let mut info = [0u8; 136];
        if unsafe { proc_pidinfo(pid, 3, 0, info.as_mut_ptr().cast(), 136) } != 136 {
            return None;
        }
        pid = i32::from_ne_bytes(info[16..20].try_into().expect("fixed proc field"));
    }
    None
}

/// Independent-window native CGImage, without RGBA/profile conversion.
pub fn native_window_image(
    id: u32,
    bounds: [f64; 4],
    target: Option<(u32, u32)>,
) -> XCapResult<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    autoreleasepool(|_| {
        use objc2_core_graphics::CGImage;
        if objc2::runtime::AnyClass::get(c"SCScreenshotManager").is_none() {
            return Err(error("native window capture requires macOS 14"));
        }
        let [x, y, w, h] = bounds;
        if !bounds.iter().all(|n| n.is_finite()) || w <= 0. || h <= 0. {
            return Err(error("invalid native window bounds"));
        }
        let (tx, rx) = sync_channel(1);
        let done = RcBlock::new(move |content: *mut SCShareableContent, e: *mut NSError| {
            let result = if let Some(e) = unsafe { e.as_ref() } {
                Err(native_error("window content", e))
            } else {
                unsafe { Retained::retain(content) }
                    .map(Content)
                    .ok_or_else(|| error("missing window content"))
            };
            let _ = tx.try_send(result);
        });
        unsafe {
            SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(false,false,&done);
        }
        let content = rx
            .recv_timeout(WAIT)
            .map_err(|_| error("native window enumeration timeout"))??;
        let window = unsafe { content.0.windows() }
            .into_iter()
            .find(|window| unsafe { window.windowID() == id })
            .ok_or_else(|| error("native window no longer exists"))?;
        let actual = unsafe { window.frame() };
        if actual.origin.x != x
            || actual.origin.y != y
            || actual.size.width != w
            || actual.size.height != h
        {
            return Err(error("native window geometry changed"));
        }
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
        };
        let scale = if filter.respondsToSelector(objc2::sel!(pointPixelScale)) {
            (unsafe { filter.pointPixelScale() }) as f64
        } else {
            1.
        };
        let (width, height) = target.unwrap_or((
            (w * scale).round().max(1.) as u32,
            (h * scale).round().max(1.) as u32,
        ));
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS as u64 {
            return Err(error("native window dimensions exceed budget"));
        }
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setShowsCursor(false);
            config.setCapturesAudio(false);
            if config.respondsToSelector(objc2::sel!(setIncludeChildWindows:)) {
                config.setIncludeChildWindows(false);
            }
        }
        let image = native_capture_image(&filter, &config, width, height)?;
        if CGImage::width(Some(&image)) != width as usize
            || CGImage::height(Some(&image)) != height as usize
        {
            return Err(error("native window output dimensions changed"));
        }
        Ok(image)
    })
}
