//! Window-scoped Accessibility executor. It never activates an application,
//! moves the real cursor, posts global input, or reads/writes the clipboard.
//! Each operation resolves the live process/window again; AX objects and tree
//! indices never cross this native request boundary.
use super::{app_info_from, tcc, ComputerError, NSRunningApplication, NSString};
use base64::Engine;
use core_foundation::{
    array::CFArray,
    base::{CFRange, CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::CFNumber,
    string::{CFString, CFStringRef},
};
use core_graphics::geometry::{CGPoint, CGSize};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

const AX_TIMEOUT: f32 = 0.2;
const MAX_NODES: usize = 2048;
const MAX_DEPTH: usize = 32;
const MAX_PIXELS: u64 = 33_554_432;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyElementAtPosition(
        element: CFTypeRef,
        x: f32,
        y: f32,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        parameter: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: CFTypeRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: CFTypeRef, names: *mut CFTypeRef) -> i32;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, output: *mut std::ffi::c_void) -> bool;
    fn AXValueCreate(kind: u32, value: *const std::ffi::c_void) -> CFTypeRef;
    // Core Accessibility's window identity bridge. Matching geometry/title is
    // not an acceptable replacement because two live windows can share both.
    fn _AXUIElementGetWindow(element: CFTypeRef, window_id: *mut u32) -> i32;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapIsEnabled(port: core_foundation::mach_port::CFMachPortRef) -> bool;
    fn CGWindowListCopyWindowInfo(option: u32, window: u32) -> CFTypeRef;
}
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

#[derive(Clone)]
struct Ax(CFType);
impl Ax {
    fn from_copy(raw: CFTypeRef) -> Option<Self> {
        if raw.is_null() {
            return None;
        }
        let value = unsafe { CFType::wrap_under_create_rule(raw) };
        if value.type_of() != unsafe { AXUIElementGetTypeID() } {
            return None;
        }
        if unsafe { AXUIElementSetMessagingTimeout(value.as_concrete_TypeRef(), AX_TIMEOUT) } != 0 {
            return None;
        }
        Some(Self(value))
    }
    fn raw(&self) -> CFTypeRef {
        self.0.as_concrete_TypeRef()
    }
    fn pid(&self) -> Option<i32> {
        let mut pid = 0;
        (unsafe { AXUIElementGetPid(self.raw(), &mut pid) } == 0 && pid > 0).then_some(pid)
    }
    fn attr(&self, name: &str) -> Option<CFType> {
        let mut raw = std::ptr::null();
        let name = CFString::new(name);
        let status = unsafe {
            AXUIElementCopyAttributeValue(self.raw(), name.as_concrete_TypeRef(), &mut raw)
        };
        // Copy ownership holds even for an unexpected error/value combination.
        let value = (!raw.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(raw) });
        (status == 0).then_some(value).flatten()
    }
    fn child(&self, name: &str) -> Option<Self> {
        Self::from_value(self.attr(name)?)
    }
    fn from_value(value: CFType) -> Option<Self> {
        if value.type_of() != unsafe { AXUIElementGetTypeID() } {
            return None;
        }
        if unsafe { AXUIElementSetMessagingTimeout(value.as_concrete_TypeRef(), AX_TIMEOUT) } != 0 {
            return None;
        }
        Some(Self(value))
    }
    fn children(&self, name: &str) -> Vec<Self> {
        self.attr(name)
            .map(|v| {
                array_values(&v)
                    .into_iter()
                    .filter_map(Self::from_value)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn text(&self, name: &str) -> Option<String> {
        self.attr(name)?
            .downcast::<CFString>()
            .map(|s| s.to_string())
    }
    fn boolean(&self, name: &str) -> bool {
        self.attr(name)
            .and_then(|v| v.downcast::<CFBoolean>())
            .is_some_and(bool::from)
    }
    fn settable(&self, name: &str) -> bool {
        let name = CFString::new(name);
        let mut yes = 0;
        unsafe {
            AXUIElementIsAttributeSettable(self.raw(), name.as_concrete_TypeRef(), &mut yes) == 0
                && yes != 0
        }
    }
    fn set(&self, name: &str, value: &CFType) -> i32 {
        unsafe {
            AXUIElementSetAttributeValue(
                self.raw(),
                CFString::new(name).as_concrete_TypeRef(),
                value.as_concrete_TypeRef(),
            )
        }
    }
    fn actions(&self) -> Vec<String> {
        let mut raw = std::ptr::null();
        let status = unsafe { AXUIElementCopyActionNames(self.raw(), &mut raw) };
        let value = (!raw.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(raw) });
        if status != 0 {
            return vec![];
        }
        value
            .map(|v| {
                array_values(&v)
                    .into_iter()
                    .filter_map(|v| v.downcast::<CFString>().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }
    fn perform(&self, action: &str) -> i32 {
        unsafe { AXUIElementPerformAction(self.raw(), CFString::new(action).as_concrete_TypeRef()) }
    }
    fn window_id(&self) -> Option<u32> {
        let mut id = 0;
        (unsafe { _AXUIElementGetWindow(self.raw(), &mut id) } == 0 && id != 0).then_some(id)
    }
    fn bounds(&self) -> Option<Bounds> {
        let position = self.attr("AXPosition")?;
        let size = self.attr("AXSize")?;
        if position.type_of() != unsafe { AXValueGetTypeID() }
            || size.type_of() != unsafe { AXValueGetTypeID() }
        {
            return None;
        }
        let mut p = CGPoint::new(0., 0.);
        let mut s = CGSize::new(0., 0.);
        if !unsafe {
            AXValueGetValue(
                position.as_concrete_TypeRef(),
                1,
                (&mut p as *mut CGPoint).cast(),
            ) && AXValueGetValue(
                size.as_concrete_TypeRef(),
                2,
                (&mut s as *mut CGSize).cast(),
            )
        } {
            return None;
        }
        Bounds::new(p.x, p.y, s.width, s.height)
    }
}
fn array_values(value: &CFType) -> Vec<CFType> {
    if value.type_of() != CFArray::<CFType>::type_id() {
        return vec![];
    }
    let array =
        unsafe { CFArray::<CFType>::wrap_under_get_rule(value.as_concrete_TypeRef().cast()) };
    array.iter().take(MAX_NODES).map(|v| (*v).clone()).collect()
}
#[derive(Clone, Copy)]
struct Bounds {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}
impl Bounds {
    fn new(x: f64, y: f64, w: f64, h: f64) -> Option<Self> {
        ([x, y, w, h].iter().all(|v| v.is_finite()) && w > 0. && h > 0.).then_some(Self {
            x,
            y,
            w,
            h,
        })
    }
    fn value(self) -> Value {
        json!({"x":self.x,"y":self.y,"w":self.w,"h":self.h})
    }
    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}
struct Target {
    app: String,
    pid: i32,
    process: Ax,
    process_app: objc2::rc::Retained<NSRunningApplication>,
    launch_time: Option<u64>,
    is_cancelled: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    collision_generation: u64,
    notify_expected_escape: std::sync::Arc<dyn Fn() + Send + Sync>,
}
impl Target {
    fn resolve(
        app: &str,
        is_cancelled: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
        collision_generation: u64,
        notify_expected_escape: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, ComputerError> {
        if app.is_empty() {
            return Err(ComputerError::Other(
                "app bundle identifier is required".into(),
            ));
        }
        let apps =
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(app));
        if apps.len() != 1 {
            return Err(ComputerError::Other(
                "application is not running or has ambiguous process identity".into(),
            ));
        }
        let native = apps
            .iter()
            .next()
            .ok_or_else(|| ComputerError::Other("application exited".into()))?;
        let pid = native.processIdentifier();
        if pid <= 0 || pid == std::process::id() as i32 {
            return Err(ComputerError::PermissionDenied(
                "self application is not an app-scoped subject".into(),
            ));
        }
        let process = Ax::from_copy(unsafe { AXUIElementCreateApplication(pid) })
            .ok_or_else(|| ComputerError::Other("AX application unavailable".into()))?;
        if process.pid() != Some(pid) {
            return Err(ComputerError::Other("AX application owner changed".into()));
        }
        let launch_time = native
            .launchDate()
            .map(|date| date.timeIntervalSince1970().to_bits());
        Ok(Self {
            app: app.into(),
            pid,
            process,
            process_app: native,
            launch_time,
            is_cancelled,
            collision_generation,
            notify_expected_escape,
        })
    }
    fn live(&self) -> bool {
        !(self.is_cancelled)() && self.identity_live()
    }
    // Cleanup identity is independent of permission/cancellation. It permits
    // only balancing an already-delivered down on its exact original subject.
    fn identity_live(&self) -> bool {
        if self.process_app.isTerminated() {
            return false;
        }
        NSRunningApplication::runningApplicationWithProcessIdentifier(self.pid)
            .as_deref()
            .is_some_and(|current| {
                !current.isTerminated()
                    && app_info_from(current).is_some_and(|app| app.bundle_id == self.app)
                    && current
                        .launchDate()
                        .map(|date| date.timeIntervalSince1970().to_bits())
                        == self.launch_time
            })
    }
    fn windows(&self) -> Vec<Ax> {
        self.process
            .children("AXWindows")
            .into_iter()
            .filter(|w| w.pid() == Some(self.pid) && w.window_id().is_some())
            .collect()
    }
    fn window(&self, id: Option<u32>) -> Result<Ax, ComputerError> {
        if !self.live() {
            return Err(ComputerError::Other(
                "application process exited or changed identity".into(),
            ));
        }
        let windows = self.windows();
        let matching: Vec<_> = windows
            .iter()
            .filter(|w| id.map_or_else(|| w.boolean("AXMain"), |id| w.window_id() == Some(id)))
            .cloned()
            .collect();
        if matching.len() == 1 {
            return Ok(matching[0].clone());
        }
        if id.is_none() && windows.len() == 1 {
            return Ok(windows[0].clone());
        }
        Err(ComputerError::Other(
            "window is absent or ambiguous; refresh app_list_windows".into(),
        ))
    }
    fn owns(&self, element: &Ax, id: u32) -> bool {
        !(self.is_cancelled)() && self.owns_identity(element, id)
    }
    fn owns_identity(&self, element: &Ax, id: u32) -> bool {
        if !self.identity_live() || element.pid() != Some(self.pid) {
            return false;
        }
        element
            .window_id()
            .or_else(|| element.child("AXWindow").and_then(|w| w.window_id()))
            == Some(id)
    }
}

pub(super) async fn execute_with_cancel_and_escape(
    action: &str,
    app: &str,
    input: &Utf16JsonProjection,
    is_cancelled: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    notify_expected_escape: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> Result<Utf16JsonProjection, ComputerError> {
    let action = action.to_owned();
    let app = app.to_owned();
    let input: Value = serde_json::from_str(
        &input
            .to_json_string()
            .map_err(|e| ComputerError::Other(e.to_string()))?,
    )
    .map_err(|e| ComputerError::Other(e.to_string()))?;
    let output = tokio::task::spawn_blocking(move || {
        if is_cancelled() {
            return Err(ComputerError::Other("computer operation cancelled".into()));
        }
        execute_sync(&action, &app, &input, is_cancelled, notify_expected_escape)
    })
    .await
    .map_err(|e| ComputerError::Other(format!("app-scoped worker failed: {e}")))??;
    Utf16JsonProjection::from_serde_value(&output).map_err(|e| ComputerError::Other(e.to_string()))
}
fn execute_sync(
    action: &str,
    app: &str,
    input: &Value,
    is_cancelled: std::sync::Arc<dyn Fn() -> bool + Send + Sync>,
    notify_expected_escape: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> Result<Value, ComputerError> {
    if action == "session_guard" {
        let mut state = json!({"screenLocked":tcc::screen_locked()?});
        // Zero explicitly denotes an unknown owner; only presence is used to
        // block keyboard work when the OS's secure-input flag is active.
        if unsafe { IsSecureEventInputEnabled() } {
            state["secureInputPid"] = json!(0);
        }
        collision_monitor().append_state(&mut state);
        return Ok(state);
    }
    if action == "find_window_displays" {
        return find_window_displays(input);
    }
    if action == "list_apps" {
        return list_apps();
    }
    if action == "release" {
        return Ok(json!({"released":true}));
    }
    if !tcc::query_os_permissions()?.0 {
        return Err(ComputerError::PermissionDenied(
            "macOS Accessibility permission is not granted".into(),
        ));
    }
    let collision_generation = input
        .get("collision_generation")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            COLLISION_MONITOR.get().map_or(0, |state| {
                state.generation.load(std::sync::atomic::Ordering::Acquire)
            })
        });
    let target = Target::resolve(
        app,
        is_cancelled,
        collision_generation,
        notify_expected_escape,
    )?;
    let id = window_id(input)?;
    match action {
        "ensure_accessibility" => ensure_ax_enabled(&target),
        "list_windows" => list_windows(&target),
        "window_owner" => Ok(id
            .filter(|id| target.live() && super::spaces::owner_matches(target.pid, *id))
            .map_or(Value::Null, |_| json!({"bundleId":app,"pid":target.pid}))),
        "is_window_ax_live" => Ok(json!(target.window(id).is_ok())),
        "capture_window" => capture(&target, id, input),
        "dispatch" => dispatch(&target, id, input),
        "menu" => menu(&target, input),
        "bring_to_current_space" => {
            let id = id.ok_or_else(|| ComputerError::Other("window_id is required".into()))?;
            super::spaces::bring(
                target.pid,
                id,
                input
                    .get("dry_run")
                    .or_else(|| input.get("dryRun"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                &|| {
                    target.live()
                        && !collision_blocked(&target, id)
                        && tcc::accessibility_granted()
                        && tcc::screen_locked().is_ok_and(|locked| !locked)
                        && super::spaces::owner_matches(target.pid, id)
                },
            )
        }
        _ => Err(ComputerError::Unsupported(format!(
            "app-scoped native action {action}"
        ))),
    }
}
fn window_id(input: &Value) -> Result<Option<u32>, ComputerError> {
    input
        .get("window_id")
        .or_else(|| input.get("windowId"))
        .map(|v| {
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n != 0)
                .ok_or_else(|| ComputerError::Other("window_id must be a positive integer".into()))
        })
        .transpose()
}
fn is_from_dev_path(path: &str, user_applications: Option<&str>) -> bool {
    if [
        "/Applications/",
        "/System/Applications/",
        "/System/Library/",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix))
    {
        return false;
    }
    !user_applications
        .is_some_and(|root| path.starts_with(&format!("{}/", root.trim_end_matches('/'))))
}
fn list_apps() -> Result<Value, ComputerError> {
    use objc2_foundation::{NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask};
    let directories = NSFileManager::defaultManager().URLsForDirectory_inDomains(
        NSSearchPathDirectory::ApplicationDirectory,
        NSSearchPathDomainMask::UserDomainMask,
    );
    let user_applications = directories
        .iter()
        .next()
        .and_then(|url| url.path())
        .map(|path| path.to_string());
    let mut rows = std::collections::BTreeMap::new();
    for app in super::apps::list_installed_apps() {
        let mut row =
            json!({"bundleId":app.bundle_id,"displayName":app.display_name,"isRunning":false});
        if let Some(path) = super::NSWorkspace::sharedWorkspace()
            .URLForApplicationWithBundleIdentifier(&NSString::from_str(&app.bundle_id))
            .and_then(|url| url.path())
            .map(|path| path.to_string())
        {
            row["path"] = json!(path);
        }
        rows.insert(app.bundle_id.clone(), row);
    }
    for running in &*super::NSWorkspace::sharedWorkspace().runningApplications() {
        if running.isTerminated()
            || running.activationPolicy() != super::NSApplicationActivationPolicy::Regular
        {
            continue;
        }
        let Some(app) = app_info_from(&running) else {
            continue;
        };
        let mut row = json!({"bundleId":app.bundle_id,"displayName":app.display_name,"isRunning":true,"pid":running.processIdentifier()});
        if let Some(path) = running
            .bundleURL()
            .and_then(|url| url.path())
            .map(|path| path.to_string())
        {
            row["isFromDevPath"] = json!(is_from_dev_path(&path, user_applications.as_deref()));
            row["path"] = json!(path);
        }
        rows.insert(app.bundle_id, row);
    }
    Ok(Value::Array(rows.into_values().collect()))
}
// Pinned native AXEnablement.ensureEnabled (0x17cf0) caches each PID after
// success or AXErrorAttributeUnsupported. This is target-app AX opt-in, never
// a system Accessibility request and never an additional OS-permission gate.
fn ax_enable_result(status: i32) -> (bool, bool) {
    (matches!(status, 0 | -25205), status == 0)
}
fn ensure_ax_enabled(target: &Target) -> Result<Value, ComputerError> {
    type Identity = (i32, Option<u64>);
    static ENABLED: std::sync::OnceLock<std::sync::Mutex<HashSet<Identity>>> =
        std::sync::OnceLock::new();
    let mut enabled = ENABLED
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| ComputerError::Other("AX enablement cache poisoned".into()))?;
    let key = (target.pid, target.launch_time);
    if enabled.contains(&key) {
        return Ok(json!({"newlyEnabled":false,"cached":true,"axStatus":null}));
    }
    if !target.live() {
        return Err(ComputerError::Other(
            "application identity or grant changed before AX enablement".into(),
        ));
    }
    if !tcc::accessibility_granted() {
        return Err(ComputerError::PermissionDenied(
            "macOS Accessibility permission is not granted".into(),
        ));
    }
    let status = target.process.set(
        "AXManualAccessibility",
        &CFBoolean::true_value().as_CFType(),
    );
    let (cache, newly_enabled) = ax_enable_result(status);
    if cache {
        enabled.insert(key);
    }
    // Preserve the native retry contract on other errors without confusing an
    // optional target attribute failure with a missing macOS grant.
    Ok(json!({"newlyEnabled":newly_enabled,"cached":false,"axStatus":status}))
}
fn list_windows(target: &Target) -> Result<Value, ComputerError> {
    if !target.live() {
        return Err(ComputerError::Other("application identity changed".into()));
    }
    // AXWindows can be empty when every app window is on another Space. Native
    // WindowServer records establish exact ID/PID ownership for observation and
    // Space moves; they never substitute for a live AX target for input.
    let mut rows = cg_window_rows(target.pid)?;
    for w in target.windows() {
        let Some(bounds) = w.bounds() else { continue };
        let Some(id) = w.window_id() else { continue };
        let entry = json!({"windowId":id,"rawTitle":w.text("AXTitle").unwrap_or_default(),"isMain":w.boolean("AXMain"),"isMinimized":w.boolean("AXMinimized"),"isOffSpace":rows.iter().find(|row|row["windowId"]==id).map(|row|row["isOffSpace"].clone()).unwrap_or(json!(true)),"bounds":bounds.value()});
        if let Some(existing) = rows.iter_mut().find(|row| row["windowId"] == id) {
            *existing = entry;
        } else {
            rows.push(entry);
        }
    }
    if !target.live() {
        return Err(ComputerError::Other(
            "application identity changed during window enumeration".into(),
        ));
    }
    Ok(Value::Array(rows))
}
fn cg_window_rows(pid: i32) -> Result<Vec<Value>, ComputerError> {
    let raw = unsafe { CGWindowListCopyWindowInfo(1 << 4, 0) }; // all Spaces, exclude desktop elements
    if raw.is_null() {
        return Err(ComputerError::Other(
            "WindowServer window enumeration unavailable".into(),
        ));
    }
    let owner = unsafe { CFType::wrap_under_create_rule(raw) };
    if owner.type_of() != CFArray::<*const std::ffi::c_void>::type_id() {
        return Err(ComputerError::Other(
            "invalid WindowServer window array".into(),
        ));
    }
    let array: CFArray<CFType> =
        unsafe { CFArray::wrap_under_get_rule(owner.as_concrete_TypeRef().cast()) };
    if array.len() > 16384 {
        return Err(ComputerError::Other(
            "WindowServer enumeration exceeds budget".into(),
        ));
    }
    let mut rows = Vec::new();
    for value in array.iter() {
        if value.type_of()
            != CFDictionary::<*const std::ffi::c_void, *const std::ffi::c_void>::type_id()
        {
            continue;
        }
        let dict: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(value.as_concrete_TypeRef().cast()) };
        let number = |key: &str| {
            dict.find(CFString::new(key))
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i64())
        };
        if number("kCGWindowOwnerPID") != Some(i64::from(pid))
            || number("kCGWindowLayer") != Some(0)
        {
            continue;
        }
        let Some(id) = number("kCGWindowNumber")
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v != 0)
        else {
            continue;
        };
        let Some(bounds) = dict.find(CFString::new("kCGWindowBounds")) else {
            continue;
        };
        if bounds.type_of()
            != CFDictionary::<*const std::ffi::c_void, *const std::ffi::c_void>::type_id()
        {
            continue;
        }
        let bounds: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef().cast()) };
        let n = |key: &str| {
            bounds
                .find(CFString::new(key))
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_f64())
        };
        let Some(bounds) = n("X")
            .zip(n("Y"))
            .zip(n("Width").zip(n("Height")))
            .and_then(|((x, y), (w, h))| Bounds::new(x, y, w, h))
        else {
            continue;
        };
        let title = dict
            .find(CFString::new("kCGWindowName"))
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let onscreen = dict
            .find(CFString::new("kCGWindowIsOnscreen"))
            .and_then(|v| v.downcast::<CFBoolean>())
            .map(bool::from)
            .unwrap_or(false);
        rows.push(json!({"windowId":id,"rawTitle":title,"isMain":false,"isMinimized":false,"isOffSpace":!onscreen,"bounds":bounds.value()}));
    }
    Ok(rows)
}
fn sheet_refusal(target: &Target, window: &Ax) -> Option<&'static str> {
    let sheets = window.children("AXSheets");
    if sheets.is_empty() {
        return None;
    }
    Some(
        if sheets.iter().any(|sheet| sheet.pid() != Some(target.pid)) {
            "foreign_pid"
        } else {
            "modal_sheet"
        },
    )
}
fn summary(target: &Target, window: &Ax, bounds: Bounds) -> (Vec<Value>, bool, bool) {
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut stack = vec![(window.clone(), 0)];
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    let mut foreign = sheet_refusal(target, window) == Some("foreign_pid");
    let mut truncated = false;
    while let Some((element, depth)) = stack.pop() {
        if nodes.len() >= MAX_NODES || Instant::now() >= deadline {
            truncated = true;
            break;
        }
        if !seen.insert(element.raw() as usize) {
            continue;
        }
        if element.pid() != Some(target.pid) {
            foreign = true;
            continue;
        }
        if let Some(b) = element.bounds() {
            nodes.push(json!({"role":element.text("AXRole").unwrap_or_default(),"subrole":element.text("AXSubrole"),"title":element.text("AXTitle"),"x":b.x-bounds.x,"y":b.y-bounds.y,"w":b.w,"h":b.h}));
        }
        let children = element.children("AXChildren");
        if depth >= MAX_DEPTH {
            truncated |= !children.is_empty();
        } else {
            let remaining = MAX_NODES.saturating_sub(nodes.len() + stack.len());
            truncated |= children.len() > remaining;
            stack.extend(
                children
                    .into_iter()
                    .take(remaining)
                    .rev()
                    .map(|c| (c, depth + 1)),
            );
        }
    }
    (nodes, truncated, foreign)
}
fn capture(target: &Target, id: Option<u32>, input: &Value) -> Result<Value, ComputerError> {
    if !tcc::query_os_permissions()?.1 {
        return Err(ComputerError::PermissionDenied(
            "macOS Screen Recording permission is not granted".into(),
        ));
    }
    let _ = ensure_ax_enabled(target)?;
    let window = target.window(id)?;
    let id = window
        .window_id()
        .ok_or_else(|| ComputerError::Other("window identity unavailable".into()))?;
    let bounds = window
        .bounds()
        .ok_or_else(|| ComputerError::Other("window bounds unavailable".into()))?;
    // Capturing only a base backing store would omit a same-process modal
    // sheet. Refuse rather than label stale/incomplete pixels as the window.
    if sheet_refusal(target, &window) == Some("modal_sheet") {
        return Err(ComputerError::Unsupported(
            "background capture of an owned modal sheet".into(),
        ));
    }
    let (nodes, truncated, foreign) = summary(target, &window, bounds);
    let requested = input
        .get("dimensions")
        .or_else(|| input.get("size"))
        .unwrap_or(input);
    let w = requested
        .get("w")
        .or_else(|| requested.get("width"))
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let h = requested
        .get("h")
        .or_else(|| requested.get("height"))
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let quality = match input.get("quality") {
        None => 75,
        Some(value) => value
            .as_u64()
            .filter(|quality| *quality <= 100)
            .ok_or_else(|| {
                ComputerError::Other("capture quality must be an integer between 0 and 100".into())
            })?,
    };
    if !foreign && xcap::native_filtered_capture_available() {
        let size = w.zip(h);
        let image = xcap::native_window_image(id, [bounds.x, bounds.y, bounds.w, bounds.h], size)
            .map_err(|e| super::monitor_error(&e))?;
        if !target.owns(&target.window(Some(id))?, id) {
            return Err(ComputerError::Other(
                "window changed during native capture".into(),
            ));
        }
        let encoded = super::jpeg::encode_window_cgimage(&image, quality as f64 / 100.)?;
        return Ok(
            json!({"base64":base64::engine::general_purpose::STANDARD.encode(encoded),"mediaType":"image/jpeg","width":objc2_core_graphics::CGImage::width(Some(&image)),"height":objc2_core_graphics::CGImage::height(Some(&image)),"windowBounds":bounds.value(),"resolvedWindowId":id,"axSummary":nodes,"summaryTruncated":truncated,"foreignPanelPresent":false}),
        );
    }
    let mut image = capture_pixels(id, bounds)?;
    if foreign {
        let sheets = window.children("AXSheets");
        let rectangles: Vec<_> = sheets
            .iter()
            .filter(|sheet| sheet.pid() != Some(target.pid))
            .filter_map(Ax::bounds)
            .collect();
        if rectangles.is_empty() {
            mask_foreign_region(&mut image, bounds, bounds);
        } else {
            for rectangle in rectangles {
                mask_foreign_region(&mut image, bounds, rectangle);
            }
        }
    }
    if let (Some(w), Some(h)) = (w, h) {
        if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
            return Err(ComputerError::Other(
                "capture dimensions exceed budget".into(),
            ));
        }
        image = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
    }
    // Re-resolve after capture; never publish a frame for a reused/closed id.
    if !target.owns(&target.window(Some(id))?, id) {
        return Err(ComputerError::Other("window changed during capture".into()));
    }
    // The pinned native appScopedCaptureWindowMethod continuation explicitly
    // passes CGFloat 0.75 to CaptureWindow.capture (0x27f8c). Keep the private
    // integer quality override for existing internal callers; public tools do
    // not expose a model quality parameter.
    let png = super::encode_png(image.clone())?;
    let encoded = super::jpeg::encode_png_with_quality(&png, quality as f64 / 100.0)?;
    Ok(
        json!({"base64":base64::engine::general_purpose::STANDARD.encode(encoded),"mediaType":"image/jpeg","width":image.width(),"height":image.height(),"windowBounds":bounds.value(),"resolvedWindowId":id,"axSummary":nodes,"summaryTruncated":truncated,"foreignPanelPresent":foreign}),
    )
}
fn mask_foreign_region(image: &mut image::RgbaImage, window: Bounds, region: Bounds) {
    let scale_x = f64::from(image.width()) / window.w;
    let scale_y = f64::from(image.height()) / window.h;
    let left = ((region.x - window.x) * scale_x)
        .floor()
        .clamp(0., f64::from(image.width())) as u32;
    let top = ((region.y - window.y) * scale_y)
        .floor()
        .clamp(0., f64::from(image.height())) as u32;
    let right = ((region.x + region.w - window.x) * scale_x)
        .ceil()
        .clamp(0., f64::from(image.width())) as u32;
    let bottom = ((region.y + region.h - window.y) * scale_y)
        .ceil()
        .clamp(0., f64::from(image.height())) as u32;
    for y in top..bottom {
        for x in left..right {
            let shade = if (x / 6 + y / 6) % 2 == 0 { 48 } else { 96 };
            image.put_pixel(x, y, image::Rgba([shade, shade, shade, 255]));
        }
    }
}
fn capture_pixels(id: u32, bounds: Bounds) -> Result<image::RgbaImage, ComputerError> {
    xcap::capture_window_by_id(id, [bounds.x, bounds.y, bounds.w, bounds.h])
        .map_err(|e| ComputerError::Other(format!("window ScreenCaptureKit capture failed: {e}")))
}

fn record() -> Value {
    json!({"outcome":"unsupported","reasonCode":null,"foreignPid":null,"role":"AXUnknown","subrole":null,"title":null,"axOp":"none","axError":null,"descentPath":[],"textWritable":false,"path":"ax","previousContent":null,"previousContentTruncated":false,"screenPt":null})
}
fn refuse(mut record: Value, reason: &str) -> Value {
    record["reasonCode"] = json!(reason);
    record
}
fn element_for(target: &Target, window: &Ax, id: u32, input: &Value) -> Result<Ax, &'static str> {
    if let Some(reason) = sheet_refusal(target, window) {
        return Err(reason);
    }
    if input.get("target").and_then(Value::as_str) == Some("focused") {
        let focused = target
            .process
            .child("AXFocusedUIElement")
            .ok_or("no_focused_text")?;
        return target
            .owns(&focused, id)
            .then_some(focused)
            .ok_or("focus_in_sibling_window");
    }
    let point = input
        .get("point")
        .or_else(|| input.get("windowPt"))
        .ok_or("canvas")?;
    let (x, y) = (
        point.get("x").and_then(Value::as_f64).ok_or("canvas")?,
        point.get("y").and_then(Value::as_f64).ok_or("canvas")?,
    );
    let bounds = window.bounds().ok_or("window_ax_opaque")?;
    if ![x, y].iter().all(|v| v.is_finite()) || x < 0. || y < 0. || x >= bounds.w || y >= bounds.h {
        return Err("canvas");
    }
    // Walk the selected window, never system-wide hit-test (which can hit a
    // different foreground window even though this app owns the coordinates).
    let mut stack = vec![(window.clone(), 0)];
    let mut selected = window.clone();
    let mut selected_depth = 0;
    let mut ambiguous = false;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut count = 0;
    while let Some((element, depth)) = stack.pop() {
        count += 1;
        if count > MAX_NODES || Instant::now() > deadline {
            return Err("window_ax_opaque");
        }
        if element.pid() != Some(target.pid) {
            continue;
        }
        if element
            .bounds()
            .is_some_and(|b| b.contains(bounds.x + x, bounds.y + y))
        {
            // Equally deep overlapping branches cannot establish a unique
            // intended AX target; never pick one by traversal order.
            if depth > selected_depth {
                selected = element.clone();
                selected_depth = depth;
                ambiguous = false;
            } else if depth == selected_depth && element.raw() != selected.raw() {
                ambiguous = true;
            }
            if depth < MAX_DEPTH {
                let children = element.children("AXChildren");
                let remaining = MAX_NODES.saturating_sub(count + stack.len());
                if children.len() > remaining {
                    return Err("window_ax_opaque");
                }
                stack.extend(children.into_iter().rev().map(|c| (c, depth + 1)));
            }
        }
    }
    if ambiguous {
        return Err("canvas");
    }
    if !target.owns(&selected, id) {
        return Err("foreign_pid");
    }
    Ok(selected)
}
fn mutation_allowed(target: &Target, element: &Ax, id: u32) -> bool {
    !collision_blocked(target, id)
        && target.owns(element, id)
        && tcc::query_os_permissions().is_ok_and(|state| state.0)
        && tcc::screen_locked().is_ok_and(|locked| !locked)
}
fn set_owned(target: &Target, element: &Ax, id: u32, name: &str, value: &CFType) -> i32 {
    if !mutation_allowed(target, element, id) {
        return -25202;
    }
    element.set(name, value)
}
fn perform_owned(target: &Target, element: &Ax, id: u32, action: &str) -> i32 {
    if !mutation_allowed(target, element, id) {
        return -25202;
    }
    element.perform(action)
}
fn dispatch(target: &Target, id: Option<u32>, input: &Value) -> Result<Value, ComputerError> {
    let result = dispatch_ax(target, id, input)?;
    let reason = result
        .get("reasonCode")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !matches!(
        reason,
        "canvas"
            | "key_not_mapped"
            | "ax_write_silent_noop"
            | "no_range_for_position"
            | "non_text_drag"
            | "not_text_editable"
    ) {
        return Ok(result);
    }
    dispatch_raw(target, id, input, result)
}
fn dispatch_ax(target: &Target, id: Option<u32>, input: &Value) -> Result<Value, ComputerError> {
    let mut out = record();
    let window = target.window(id)?;
    let id = window
        .window_id()
        .ok_or_else(|| ComputerError::Other("window identity unavailable".into()))?;
    if tcc::screen_locked()? || window.boolean("AXMinimized") {
        return Ok(refuse(out, "window_not_reachable"));
    }
    let intent = input
        .get("intent")
        .ok_or_else(|| ComputerError::Other("dispatch intent is required".into()))?;
    let kind = intent
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ComputerError::Other("intent.kind is required".into()))?;
    let element = match element_for(target, &window, id, input) {
        Ok(e) => e,
        Err(reason) => return Ok(refuse(out, reason)),
    };
    let role = element.text("AXRole").unwrap_or_default();
    let subrole = element.text("AXSubrole");
    out["role"] = json!(role);
    out["subrole"] = json!(subrole);
    out["title"] = json!(element.text("AXTitle"));
    out["descentPath"] = json!([window.text("AXRole").unwrap_or_default(), role]);
    out["textWritable"] = json!(element.settable("AXSelectedText") || element.settable("AXValue"));
    if matches!(kind, "type" | "key")
        && (subrole.as_deref() == Some("AXSecureTextField")
            || element.boolean("AXProtectedContent")
            || unsafe { IsSecureEventInputEnabled() })
    {
        return Ok(refuse(out, "secure_field"));
    }
    if !target.owns(&element, id) {
        return Ok(refuse(out, "foreign_pid"));
    }
    if let Some(reason) = sheet_refusal(target, &window) {
        return Ok(refuse(out, reason));
    }
    let before_text = (kind == "type").then(|| element.text("AXValue")).flatten();
    if let Some(reason) = collision_refusal(target, id) {
        return Ok(refuse(out, reason));
    }
    let status = match kind {
        "click" => {
            if intent.get("button").and_then(Value::as_str) == Some("right") {
                return Ok(refuse(out, "context_menu"));
            }
            if intent.get("count").and_then(Value::as_u64).unwrap_or(1) != 1 {
                return Ok(refuse(out, "canvas"));
            }
            if element.settable("AXSelectedTextRange") {
                if let Some(range) = range_at_point(
                    &element,
                    &window,
                    input.get("point").or_else(|| input.get("windowPt")),
                ) {
                    out["axOp"] = json!("set AXSelectedTextRange");
                    set_owned(target, &element, id, "AXSelectedTextRange", &range)
                } else {
                    return Ok(refuse(out, "no_range_for_position"));
                }
            } else if element.actions().iter().any(|a| a == "AXPress") {
                out["axOp"] = json!("AXPress");
                perform_owned(target, &element, id, "AXPress")
            } else {
                return Ok(refuse(out, "canvas"));
            }
        }
        "type" => {
            let text = intent
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| ComputerError::Other("intent.text must be a string".into()))?;
            let before = element.text("AXValue");
            let replace = intent.get("mode").and_then(Value::as_str) == Some("replace")
                || intent.get("overwriteExisting").and_then(Value::as_bool) == Some(true);
            let value = CFString::new(text).as_CFType();
            if replace && element.settable("AXValue") {
                if let Some(before) = before.as_ref() {
                    out["previousContent"] = json!(before.chars().take(500).collect::<String>());
                    out["previousContentTruncated"] = json!(before.chars().count() > 500);
                }
                out["axOp"] = json!("set AXValue");
                set_owned(target, &element, id, "AXValue", &value)
            } else if element.settable("AXSelectedText") {
                if input.get("target").and_then(Value::as_str) != Some("focused") {
                    if let Some(range) = range_at_point(
                        &element,
                        &window,
                        input.get("point").or_else(|| input.get("windowPt")),
                    ) {
                        let status = set_owned(target, &element, id, "AXSelectedTextRange", &range);
                        if status != 0 {
                            out["axError"] = json!(status.to_string());
                            return Ok(refuse(out, "no_range_for_position"));
                        }
                    }
                }
                out["axOp"] = json!("set AXSelectedText");
                set_owned(target, &element, id, "AXSelectedText", &value)
            } else if element.settable("AXValue") && before.as_deref().unwrap_or("").is_empty() {
                out["axOp"] = json!("set AXValue");
                set_owned(target, &element, id, "AXValue", &value)
            } else {
                out["previousContent"] =
                    json!(before.map(|s| s.chars().take(500).collect::<String>()));
                return Ok(refuse(
                    out,
                    if element.settable("AXValue") {
                        "would_replace_content"
                    } else {
                        "not_text_editable"
                    },
                ));
            }
        }
        "key" => {
            let combo = intent
                .get("combo")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            let action = match combo.as_str() {
                "return" | "enter" | "space" => "AXPress",
                "escape" | "esc" => "AXCancel",
                _ => return Ok(refuse(out, "key_not_mapped")),
            };
            if !element.actions().iter().any(|a| a == action) {
                return Ok(refuse(out, "key_not_mapped"));
            }
            out["axOp"] = json!(action);
            perform_owned(target, &element, id, action)
        }
        "scroll" => {
            let dx = intent.get("dx").and_then(Value::as_f64).unwrap_or(0.);
            let dy = intent.get("dy").and_then(Value::as_f64).unwrap_or(0.);
            if dx != 0. {
                return Ok(refuse(out, "horizontal_scroll"));
            }
            if !dy.is_finite() {
                return Err(ComputerError::Other("scroll delta must be finite".into()));
            }
            let action = if dy < 0. {
                "AXScrollUpByPage"
            } else {
                "AXScrollDownByPage"
            };
            let mut candidate = element.clone();
            let mut found = None;
            for _ in 0..8 {
                if candidate.actions().iter().any(|a| a == action) {
                    found = Some(candidate);
                    break;
                }
                let Some(parent) = candidate.child("AXParent") else {
                    break;
                };
                if !target.owns(&parent, id) {
                    break;
                }
                candidate = parent;
            }
            let Some(candidate) = found else {
                return Ok(refuse(out, "canvas"));
            };
            if !target.owns(&candidate, id) {
                return Ok(refuse(out, "foreign_pid"));
            }
            out["axOp"] = json!(action);
            perform_owned(target, &candidate, id, action)
        }
        "drag" => {
            let Some(start) = range_at_point(
                &element,
                &window,
                input.get("point").or_else(|| input.get("windowPt")),
            ) else {
                return Ok(refuse(out, "non_text_drag"));
            };
            let Some(end) = range_at_point(&element, &window, intent.get("toWindowPt")) else {
                return Ok(refuse(out, "non_text_drag"));
            };
            let (Some(start), Some(end)) = (cf_range(&start), cf_range(&end)) else {
                return Ok(refuse(out, "non_text_drag"));
            };
            let range = CFRange {
                location: start.location.min(end.location),
                length: (end.location - start.location).abs(),
            };
            let value =
                Ax::from_copy_value(unsafe { AXValueCreate(4, (&range as *const CFRange).cast()) })
                    .ok_or_else(|| ComputerError::Other("range allocation failed".into()))?;
            out["axOp"] = json!("set AXSelectedTextRange");
            set_owned(target, &element, id, "AXSelectedTextRange", &value)
        }
        _ => {
            return Err(ComputerError::Unsupported(format!(
                "background dispatch {kind}"
            )))
        }
    };
    if !target.owns(&element, id) {
        out["partialDelivery"] = json!(status == 0 && kind == "type");
        return Ok(refuse(out, "foreign_pid"));
    }
    if let Some(reason) = sheet_refusal(target, &window) {
        out["partialDelivery"] = json!(status == 0 && kind == "type");
        return Ok(refuse(out, reason));
    }
    if status == 0 && kind == "type" {
        let text = intent.get("text").and_then(Value::as_str).unwrap_or("");
        let replacing = intent.get("mode").and_then(Value::as_str) == Some("replace")
            || intent.get("overwriteExisting").and_then(Value::as_bool) == Some(true);
        if !text.is_empty()
            && before_text.is_some()
            && before_text == element.text("AXValue")
            && !(replacing && before_text.as_deref() == Some(text))
        {
            return Ok(refuse(out, "ax_write_silent_noop"));
        }
    }
    if status != 0 {
        if let Some(reason) = collision_refusal(target, id) {
            return Ok(refuse(out, reason));
        }
    }
    out["axError"] = json!(if status == 0 {
        "success".to_owned()
    } else {
        status.to_string()
    });
    out["outcome"] = json!(if status == 0 { "ok" } else { "ineffective" });
    Ok(out)
}
fn dispatch_raw(
    target: &Target,
    id: Option<u32>,
    input: &Value,
    mut out: Value,
) -> Result<Value, ComputerError> {
    use core_graphics::event::CGEventFlags;
    use objc2_app_kit::NSEventType;
    let window = target.window(id)?;
    let id = window
        .window_id()
        .ok_or_else(|| ComputerError::Other("window identity unavailable".into()))?;
    let Some(bounds) = window.bounds() else {
        return Ok(refuse(out, "window_ax_opaque"));
    };
    if window.boolean("AXMinimized") || tcc::screen_locked()? {
        return Ok(refuse(out, "window_not_reachable"));
    }
    if let Some(reason) = sheet_refusal(target, &window) {
        return Ok(refuse(out, reason));
    }
    let element = match element_for(target, &window, id, input) {
        Ok(e) => e,
        Err(reason) => return Ok(refuse(out, reason)),
    };
    let intent = &input["intent"];
    let kind = intent["kind"].as_str().unwrap_or("");
    let role = element.text("AXRole").unwrap_or_default();
    let text_editable = matches!(role.as_str(), "AXTextField" | "AXTextArea" | "AXComboBox");
    // Opening contextual/pop-up controls is not background-safe.
    if matches!(
        role.as_str(),
        "AXMenuBar" | "AXMenuBarItem" | "AXMenuItem" | "AXPopUpButton" | "AXMenuButton"
    ) {
        return Ok(out);
    }
    let current = || {
        mutation_allowed(target, &element, id)
            && window.bounds().is_some_and(|now| {
                now.x == bounds.x && now.y == bounds.y && now.w == bounds.w && now.h == bounds.h
            })
            && sheet_refusal(target, &window).is_none()
    };
    let focused = || {
        target
            .process
            .child("AXFocusedUIElement")
            .is_some_and(|focus| target.owns(&focus, id) && focus.0 == element.0)
    };
    let keyboard_safe = || {
        current()
            && focused()
            && element.text("AXSubrole").as_deref() != Some("AXSecureTextField")
            && !element.boolean("AXProtectedContent")
            && !unsafe { IsSecureEventInputEnabled() }
    };
    let mut delivered = false;
    out["path"] = json!("rawfallback");
    out["axError"] = out
        .get("axError")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(json!("not_supported"));
    let result: Result<(), ComputerError> = (|| {
        match kind {
            "type" => {
                if !text_editable
                    || !keyboard_safe()
                    || intent["overwriteExisting"].as_bool() == Some(true)
                {
                    return Err(ComputerError::Other(
                        "raw keyboard focus is not the selected editable element".into(),
                    ));
                }
                let text = intent["text"]
                    .as_str()
                    .ok_or_else(|| ComputerError::Other("intent.text is required".into()))?;
                if text.encode_utf16().count() > 65536 {
                    return Err(ComputerError::Other(
                        "background text exceeds budget".into(),
                    ));
                }
                if intent["mode"].as_str() == Some("replace") {
                    if !keyboard_safe() {
                        return Err(ComputerError::Other(
                            "background keyboard guard changed".into(),
                        ));
                    }
                    super::raw_input::key(target.pid, 0, CGEventFlags::CGEventFlagCommand)?;
                    delivered = true;
                }
                for scalar in text.chars() {
                    if !keyboard_safe() {
                        return Err(ComputerError::Other(
                            "background keyboard guard changed".into(),
                        ));
                    }
                    super::raw_input::text(target.pid, id, &scalar.to_string())?;
                    delivered = true;
                }
            }
            "key" => {
                if !text_editable || !keyboard_safe() {
                    return Err(ComputerError::Other(
                        "raw keyboard focus is not the selected editable element".into(),
                    ));
                }
                let Some((code, flags)) = raw_key(intent["combo"].as_str().unwrap_or("")) else {
                    return Err(ComputerError::Unsupported("background key combo".into()));
                };
                if code == 53 && flags.is_empty() {
                    (target.notify_expected_escape)();
                }
                super::raw_input::key(target.pid, code, flags)?;
                delivered = true;
            }
            "click" | "drag" => {
                if !super::raw_input::available() {
                    return Err(ComputerError::Unsupported("background mouse SPI".into()));
                }
                if intent["button"].as_str().is_some_and(|b| b != "left") {
                    return Err(ComputerError::Unsupported("background context menu".into()));
                }
                // Reference background dragging remains text-selection only.
                if kind == "drag" && !text_editable {
                    return Err(ComputerError::Unsupported("non_text_drag".into()));
                }
                let point = input
                    .get("point")
                    .or_else(|| input.get("windowPt"))
                    .ok_or_else(|| ComputerError::Other("mouse point required".into()))?;
                let origin = raw_point(point, bounds)?;
                let end = if kind == "drag" {
                    raw_point(&intent["toWindowPt"], bounds)?
                } else {
                    origin
                };
                let count = intent["count"].as_u64().unwrap_or(1);
                if !(1..=3).contains(&count) {
                    return Err(ComputerError::Other(
                        "background click count out of range".into(),
                    ));
                }
                let post = |kind, point: (f64, f64), click_count: u64| {
                    super::raw_input::mouse(
                        target.pid,
                        id,
                        (bounds.x + point.0, bounds.y + point.1),
                        point,
                        kind,
                        click_count as usize,
                    )
                };
                for click_count in 1..=if kind == "click" { count } else { 1 } {
                    if !current() {
                        return Err(ComputerError::Other(
                            "background mouse guard changed".into(),
                        ));
                    }
                    post(NSEventType::LeftMouseDown, origin, click_count)?;
                    delivered = true;
                    // Once down is posted, always balance its up to the exact same
                    // still-owned window; never post the release to a global tap.
                    let mut last_point = origin;
                    let drag = (|| {
                        if kind == "drag" {
                            for step in 1..=12 {
                                if !current() {
                                    return Err(ComputerError::Other(
                                        "background drag guard changed".into(),
                                    ));
                                }
                                let t = f64::from(step) / 12.;
                                let point = (
                                    origin.0 + (end.0 - origin.0) * t,
                                    origin.1 + (end.1 - origin.1) * t,
                                );
                                post(NSEventType::LeftMouseDragged, point, click_count)?;
                                last_point = point;
                                std::thread::sleep(Duration::from_millis(8));
                            }
                        }
                        Ok(())
                    })();
                    complete_mouse_delivery(drag, target.owns_identity(&window, id), || {
                        post(NSEventType::LeftMouseUp, last_point, click_count)
                    })?;
                    if !current() {
                        return Err(ComputerError::Other(
                            "background mouse guard changed after delivery".into(),
                        ));
                    }
                }
            }
            _ => return Err(ComputerError::Unsupported("background raw intent".into())),
        }
        Ok(())
    })();
    if let Err(error) = result {
        out["partialDelivery"] = json!(delivered);
        out["outcome"] = json!(if delivered {
            "ineffective"
        } else {
            "unsupported"
        });
        out["reasonCode"] = json!(match kind {
            "type" => "no_focused_text",
            "key" => "key_not_mapped",
            "drag" => "non_text_drag",
            _ => "canvas",
        });
        // Keep a bounded diagnostic, with no attempted text payload.
        out["axError"] = json!(error.to_string().chars().take(240).collect::<String>());
        return Ok(out);
    }
    out["outcome"] = json!("ok");
    out["reasonCode"] = Value::Null;
    out["path"] = json!("cgevent");
    Ok(out)
}
fn complete_mouse_delivery(
    action: Result<(), ComputerError>,
    same_identity: bool,
    release: impl FnOnce() -> Result<(), ComputerError>,
) -> Result<(), ComputerError> {
    if !same_identity {
        return Err(ComputerError::Other(
            "window identity changed before mouse-up; delivery is uncertain".into(),
        ));
    }
    // A release is cleanup, not a new mouse action. Attempt it even when the
    // original action was cancelled, but only for the exact original identity.
    let released = release();
    action.and(released)
}
fn raw_point(point: &Value, bounds: Bounds) -> Result<(f64, f64), ComputerError> {
    let x = point["x"].as_f64().unwrap_or(f64::NAN);
    let y = point["y"].as_f64().unwrap_or(f64::NAN);
    if !x.is_finite() || !y.is_finite() || x < 0. || y < 0. || x >= bounds.w || y >= bounds.h {
        return Err(ComputerError::Other(
            "background point outside target window".into(),
        ));
    }
    Ok((x, y))
}
fn raw_key(combo: &str) -> Option<(u16, core_graphics::event::CGEventFlags)> {
    use core_graphics::event::CGEventFlags as F;

    let parts = combo
        .split('+')
        .filter(|v| !v.is_empty())
        .map(lingxi_core::host::computer_control::canonical_computer_key)
        .collect::<Vec<_>>();
    let (key, mods) = parts.split_last()?;
    let mut flags = F::empty();
    for modifier in mods {
        flags |= match modifier.as_str() {
            "cmd" | "command" | "super" | "meta" => F::CGEventFlagCommand,
            "ctrl" | "control" => F::CGEventFlagControl,
            "alt" | "option" => F::CGEventFlagAlternate,
            "shift" => F::CGEventFlagShift,
            _ => return None,
        };
    }
    let code = match key.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "9" => 25,
        "7" => 26,
        "8" => 28,
        "0" => 29,
        "o" => 31,
        "u" => 32,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "k" => 40,
        "n" => 45,
        "m" => 46,
        "tab" => 48,
        "space" => 49,
        "backspace" => 51,
        "delete" => 117,
        "escape" | "esc" => 53,
        "home" => 115,
        "pageup" => 116,
        "end" => 119,
        "pagedown" => 121,
        "left" | "arrowleft" => 123,
        "right" | "arrowright" => 124,
        "down" | "arrowdown" => 125,
        "up" | "arrowup" => 126,
        _ => return None,
    };
    Some((code, flags))
}
impl Ax {
    fn from_copy_value(raw: CFTypeRef) -> Option<CFType> {
        (!raw.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(raw) })
    }
}
fn range_at_point(element: &Ax, window: &Ax, point: Option<&Value>) -> Option<CFType> {
    let point = point?;
    let bounds = window.bounds()?;
    let point = CGPoint::new(
        bounds.x + point.get("x")?.as_f64()?,
        bounds.y + point.get("y")?.as_f64()?,
    );
    let point =
        Ax::from_copy_value(unsafe { AXValueCreate(1, (&point as *const CGPoint).cast()) })?;
    let mut range = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyParameterizedAttributeValue(
            element.raw(),
            CFString::new("AXRangeForPosition").as_concrete_TypeRef(),
            point.as_concrete_TypeRef(),
            &mut range,
        )
    };
    let value = Ax::from_copy_value(range)?;
    if status != 0 {
        return None;
    }
    // AXRangeForPosition commonly returns the character span. A click places
    // the caret; retaining that span would replace a character on app_type.
    let range = cf_range(&value)?;
    let caret = CFRange {
        location: range.location,
        length: 0,
    };
    Ax::from_copy_value(unsafe { AXValueCreate(4, (&caret as *const CFRange).cast()) })
}
fn cf_range(value: &CFType) -> Option<CFRange> {
    if value.type_of() != unsafe { AXValueGetTypeID() } {
        return None;
    }
    let mut range = CFRange {
        location: 0,
        length: 0,
    };
    unsafe {
        AXValueGetValue(
            value.as_concrete_TypeRef(),
            4,
            (&mut range as *mut CFRange).cast(),
        )
    }
    .then_some(range)
    .filter(|range| range.location >= 0 && range.length >= 0)
}

fn menu_entries(node: &Ax, deadline: Instant) -> Result<Vec<(Ax, String)>, ComputerError> {
    if Instant::now() >= deadline {
        return Err(ComputerError::Other(
            "menu query exceeded its time budget".into(),
        ));
    }
    let mut children = node.children("AXChildren");
    if children.len() == 1 && children[0].text("AXRole").as_deref() == Some("AXMenu") {
        children = children[0].children("AXChildren");
    }
    if children.len() > 512 {
        return Err(ComputerError::Other(
            "menu query exceeded its item budget".into(),
        ));
    }
    let mut entries = Vec::new();
    for child in children {
        if Instant::now() >= deadline {
            return Err(ComputerError::Other(
                "menu query exceeded its time budget".into(),
            ));
        }
        entries.push((child.clone(), child.text("AXTitle").unwrap_or_default()));
    }
    Ok(entries)
}
fn menu(target: &Target, input: &Value) -> Result<Value, ComputerError> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mode = input.get("mode").and_then(Value::as_str).unwrap_or("list");
    let path: Vec<String> = input
        .get("path")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ComputerError::Other("menu path must contain strings".into()))
        })
        .collect::<Result<_, _>>()?;
    if path.len() > 12 {
        return Err(ComputerError::Other("menu path exceeds depth limit".into()));
    }
    let mut out = json!({"outcome":"unsupported","reasonCode":null,"searched":[],"items":[],"closestMatch":null,"pressedTitle":null,"axError":null});
    if mode == "press" && path.iter().any(|title| normalize_menu(title) == "services") {
        out["reasonCode"] = json!("foreign_pid");
        return Ok(out);
    }
    if mode == "press"
        && path
            .first()
            .is_some_and(|title| matches!(normalize_menu(title).as_str(), "apple" | ""))
    {
        out["reasonCode"] = json!("system_menu");
        return Ok(out);
    }
    let Some(mut node) = target.process.child("AXMenuBar") else {
        out["reasonCode"] = json!("menu_bar");
        return Ok(out);
    };
    let mut last_title = None;
    for title in &path {
        let entries = menu_entries(&node, deadline)?;
        out["searched"] = json!(entries.iter().map(|(_, title)| title).collect::<Vec<_>>());
        let mut matching = entries
            .into_iter()
            .filter(|(_, candidate)| normalize_menu(candidate) == normalize_menu(title));
        let Some((next, title)) = matching.next() else {
            out["reasonCode"] = json!("menu_item_not_found");
            return Ok(out);
        };
        if matching.next().is_some() {
            out["reasonCode"] = json!("menu_item_not_found");
            return Ok(out);
        }
        if next.pid() != Some(target.pid) {
            out["reasonCode"] = json!("foreign_pid");
            return Ok(out);
        }
        last_title = Some(title);
        node = next;
    }
    let entries = menu_entries(&node, deadline)?;
    if mode == "list" {
        out["items"] = json!(entries
            .into_iter()
            .map(|(_, title)| title)
            .collect::<Vec<_>>());
        out["outcome"] = json!("ok");
        return Ok(out);
    }
    if mode != "press" || path.is_empty() {
        return Err(ComputerError::Other(
            "menu press requires a nonempty path".into(),
        ));
    }
    if !entries.is_empty() {
        out["reasonCode"] = json!("path_ends_at_submenu");
        out["searched"] = json!(entries
            .into_iter()
            .map(|(_, title)| title)
            .collect::<Vec<_>>());
        return Ok(out);
    }
    if !node.boolean("AXEnabled") {
        out["reasonCode"] = json!("menu_item_disabled");
        return Ok(out);
    }
    if node
        .text("AXMenuItemCmdChar")
        .is_some_and(|key| matches!(key.to_ascii_lowercase().as_str(), "c" | "v" | "x"))
    {
        out["reasonCode"] = json!("clipboard_menu_item");
        return Ok(out);
    }
    if !target.live()
        || node.pid() != Some(target.pid)
        || !tcc::query_os_permissions().is_ok_and(|state| state.0)
        || !tcc::screen_locked().is_ok_and(|locked| !locked)
    {
        out["reasonCode"] = json!("foreign_pid");
        return Ok(out);
    }
    if let Some(reason) = collision_refusal(target, 0) {
        out["reasonCode"] = json!(reason);
        return Ok(out);
    }
    if Instant::now() >= deadline {
        return Err(ComputerError::Other(
            "menu query exceeded its time budget".into(),
        ));
    }
    let status = node.perform("AXPress");
    out["outcome"] = json!(if status == 0 { "ok" } else { "unsupported" });
    out["pressedTitle"] = json!(last_title);
    out["axError"] = json!(if status == 0 {
        "success".to_owned()
    } else {
        status.to_string()
    });
    out["searched"] = json!([]);
    Ok(out)
}
fn normalize_menu(value: &str) -> String {
    value
        .replace('…', "")
        .replace("...", "")
        .trim()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ax_enablement_unsupported_is_cached_without_a_false_permission_failure() {
        assert_eq!(ax_enable_result(0), (true, true));
        assert_eq!(ax_enable_result(-25205), (true, false));
        for status in [-25204, -25208, -25202] {
            assert_eq!(ax_enable_result(status), (false, false));
        }
    }

    #[test]
    fn cancellation_between_mouse_down_and_up_still_balances_exact_subject() {
        let held = std::cell::Cell::new(true);
        let result =
            complete_mouse_delivery(Err(ComputerError::Other("cancelled".into())), true, || {
                held.set(false);
                Ok(())
            });
        assert!(!held.get());
        assert!(result.is_err());
    }
    #[test]
    fn mouse_identity_loss_never_releases_another_subject_or_reports_success() {
        let released = std::cell::Cell::new(false);
        assert!(complete_mouse_delivery(Ok(()), false, || {
            released.set(true);
            Ok(())
        })
        .is_err());
        assert!(!released.get());
        assert!(
            complete_mouse_delivery(Ok(()), true, || Err(ComputerError::Other(
                "up failed".into()
            )))
            .is_err()
        );
    }

    #[test]
    fn raw_points_cannot_escape_the_resolved_window() {
        let b = Bounds::new(-400., -200., 100., 50.).unwrap();
        assert_eq!(raw_point(&json!({"x":0,"y":49}), b).unwrap(), (0., 49.));
        for point in [
            json!({"x":-1,"y":0}),
            json!({"x":100,"y":0}),
            json!({"x":0,"y":50}),
            json!({"x":"1","y":0}),
            json!({"x":null,"y":0}),
        ] {
            assert!(raw_point(&point, b).is_err());
        }
    }
    #[test]
    fn raw_keys_reject_unknown_modifiers_and_non_key_payloads() {
        use core_graphics::event::CGEventFlags as F;
        assert_eq!(
            raw_key("cmd+shift+a"),
            Some((0, F::CGEventFlagCommand | F::CGEventFlagShift))
        );
        assert_eq!(raw_key("ArrowLeft"), Some((123, F::empty())));
        assert_eq!(raw_key("LeftArrow"), Some((123, F::empty())));
        assert_eq!(raw_key("Delete"), Some((117, F::empty())));
        assert_eq!(raw_key("Backspace"), Some((51, F::empty())));
        assert_eq!(
            raw_key("opt+page_down"),
            Some((121, F::CGEventFlagAlternate))
        );
        for key in [
            "",
            "invalid+a",
            "cmd+two words",
            "shell+return",
            "volume_up",
        ] {
            assert!(raw_key(key).is_none());
        }
    }

    #[test]
    fn rejects_nonfinite_and_empty_window_bounds() {
        assert!(Bounds::new(0., 0., 1., 1.).is_some());
        for b in [
            [0., 0., 0., 1.],
            [0., 0., 1., -1.],
            [f64::NAN, 0., 1., 1.],
            [0., f64::INFINITY, 1., 1.],
        ] {
            assert!(Bounds::new(b[0], b[1], b[2], b[3]).is_none());
        }
    }
    #[test]
    fn rejects_ambiguous_or_wrapped_window_ids() {
        for v in [json!(-1), json!(0), json!(1.5), json!(u64::MAX), json!("1")] {
            assert!(window_id(&json!({"window_id":v})).is_err());
        }
        assert_eq!(window_id(&json!({"window_id":42})).unwrap(), Some(42));
    }
    #[test]
    fn unavailable_collision_monitor_never_fabricates_an_observed_generation() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let monitor = CollisionMonitor {
            available: AtomicBool::new(false),
            generation: AtomicU64::new(7),
            pending: AtomicU64::new(7),
            settled: AtomicU64::new(7),
            last: std::sync::Mutex::new(Some(Collision {
                generation: 7,
                app: Some("example.app".into()),
                window_id: Some(42),
            })),
        };
        let mut value = json!({"screenLocked":false});
        monitor.append_state(&mut value);
        assert_eq!(
            value,
            json!({"screenLocked":false,"collisionMonitoring":false})
        );
    }
    #[test]
    fn collision_observation_reports_only_real_attributed_fields_and_pending_work() {
        use std::sync::atomic::{AtomicBool, AtomicU64};
        let monitor = CollisionMonitor {
            available: AtomicBool::new(true),
            generation: AtomicU64::new(7),
            pending: AtomicU64::new(8),
            settled: AtomicU64::new(7),
            last: std::sync::Mutex::new(Some(Collision {
                generation: 7,
                app: None,
                window_id: None,
            })),
        };
        let mut value = json!({});
        monitor.append_state(&mut value);
        assert_eq!(
            value,
            json!({"collisionMonitoring":true,"collisionGeneration":7,"collisionPending":true})
        );
    }
    #[test]
    fn foreign_panel_mask_covers_only_the_clamped_region() {
        let mut image = image::RgbaImage::from_pixel(20, 20, image::Rgba([255, 0, 0, 255]));
        let window = Bounds::new(100., 100., 20., 20.).unwrap();
        mask_foreign_region(
            &mut image,
            window,
            Bounds::new(110., 110., 20., 20.).unwrap(),
        );
        assert_eq!(image.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_ne!(image.get_pixel(19, 19).0, [255, 0, 0, 255]);
        assert_eq!(image.get_pixel(19, 19).0[3], 255);
    }
    #[test]
    fn dev_path_matches_observed_native_standard_prefixes() {
        for path in [
            "/Applications/Test.app",
            "/System/Applications/Test.app",
            "/System/Library/Test.app",
            "/Users/fixture/Applications/Test.app",
        ] {
            assert!(!is_from_dev_path(path, Some("/Users/fixture/Applications")));
        }
        for path in [
            "/ApplicationsOther/Test.app",
            "/tmp/Test.app",
            "/Users/fixture/build/Test.app",
            "/Users/fixture/ApplicationsOther/Test.app",
        ] {
            assert!(is_from_dev_path(path, Some("/Users/fixture/Applications")));
        }
        assert!(is_from_dev_path(
            "/Users/fixture/Applications/Test.app",
            None
        ));
    }
    #[test]
    fn menu_matching_only_normalizes_ellipsis_spacing_and_case() {
        assert_eq!(normalize_menu(" Save As… "), "save as");
        assert_eq!(normalize_menu("Services..."), "services");
        assert_ne!(normalize_menu("Copy Link"), normalize_menu("Copy"));
    }
}

#[derive(Clone)]
struct Collision {
    generation: u64,
    app: Option<String>,
    window_id: Option<u32>,
}
struct CollisionMonitor {
    available: std::sync::atomic::AtomicBool,
    generation: std::sync::atomic::AtomicU64,
    pending: std::sync::atomic::AtomicU64,
    settled: std::sync::atomic::AtomicU64,
    last: std::sync::Mutex<Option<Collision>>,
}
static COLLISION_MONITOR: std::sync::OnceLock<std::sync::Arc<CollisionMonitor>> =
    std::sync::OnceLock::new();
impl CollisionMonitor {
    fn append_state(&self, value: &mut Value) {
        use std::sync::atomic::Ordering::Acquire;
        let available = self.available.load(Acquire);
        value["collisionMonitoring"] = json!(available);
        if !available {
            return;
        }
        value["collisionGeneration"] = json!(self.generation.load(Acquire));
        value["collisionPending"] = json!(self.pending.load(Acquire) > self.settled.load(Acquire));
        if let Some(last) = self.last.lock().ok().and_then(|last| last.clone()) {
            if let Some(app) = last.app {
                value["collisionApp"] = json!(app);
            }
            if let Some(id) = last.window_id {
                value["collisionWindowId"] = json!(id);
            }
        }
    }
}
fn collision_monitor() -> &'static std::sync::Arc<CollisionMonitor> {
    COLLISION_MONITOR.get_or_init(|| {
        use std::sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        };
        let state = Arc::new(CollisionMonitor {
            available: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            settled: AtomicU64::new(0),
            last: std::sync::Mutex::new(None),
        });
        let (points, reader) = std::sync::mpsc::sync_channel::<(u64, CGPoint, Instant)>(64);
        let attribution = state.clone();
        let worker = std::thread::Builder::new()
            .name("computer-user-attribution".into())
            .spawn(move || {
                while let Ok((generation, point, when)) = reader.recv() {
                    // Never attribute a delayed point to a newly moved/replaced UI.
                    let (app, window_id) = if when.elapsed() < Duration::from_millis(750) {
                        collision_at_point(point)
                    } else {
                        (None, None)
                    };
                    if let Ok(mut last) = attribution.last.lock() {
                        *last = Some(Collision {
                            generation,
                            app,
                            window_id,
                        });
                    }
                    attribution.generation.store(generation, Ordering::Release);
                    attribution.settled.store(generation, Ordering::Release);
                }
                attribution.available.store(false, Ordering::Release);
            });
        if worker.is_err() {
            return state;
        }
        let tap_state = state.clone();
        let (ready, wait) = std::sync::mpsc::sync_channel(1);
        let _ = std::thread::Builder::new()
            .name("computer-user-listener".into())
            .spawn(move || {
                use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop};
                use core_graphics::event::{
                    CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
                    CGEventType, EventField,
                };
                let callback_state = tap_state.clone();
                let tap = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![
                        CGEventType::LeftMouseDown,
                        CGEventType::RightMouseDown,
                        CGEventType::OtherMouseDown,
                    ],
                    move |_, kind, event| {
                        if matches!(
                            kind,
                            CGEventType::LeftMouseDown
                                | CGEventType::RightMouseDown
                                | CGEventType::OtherMouseDown
                        ) {
                            // This AX executor posts no mouse events. Explicitly
                            // exclude the same process's display-mode synthetic input.
                            if event
                                .get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID)
                                != i64::from(std::process::id())
                            {
                                let generation =
                                    callback_state.pending.fetch_add(1, Ordering::AcqRel) + 1;
                                if points
                                    .try_send((generation, event.location(), Instant::now()))
                                    .is_err()
                                {
                                    callback_state.available.store(false, Ordering::Release);
                                }
                            }
                        } else {
                            callback_state.available.store(false, Ordering::Release);
                        }
                        Some(event.clone())
                    },
                );
                let Ok(tap) = tap else {
                    let _ = ready.send(false);
                    return;
                };
                let Ok(source) = tap.mach_port.create_runloop_source(0) else {
                    let _ = ready.send(false);
                    return;
                };
                let runloop = CFRunLoop::get_current();
                runloop.add_source(&source, unsafe { kCFRunLoopDefaultMode });
                tap.enable();
                let enabled = unsafe { CGEventTapIsEnabled(tap.mach_port.as_concrete_TypeRef()) };
                tap_state.available.store(enabled, Ordering::Release);
                let _ = ready.send(enabled);
                // Retain the tap/source on their owning thread throughout the loop.
                CFRunLoop::run_current();
                tap_state.available.store(false, Ordering::Release);
            });
        let _ = wait.recv_timeout(Duration::from_millis(500));
        state
    })
}
fn collision_at_point(point: CGPoint) -> (Option<String>, Option<u32>) {
    if !point.x.is_finite() || !point.y.is_finite() {
        return (None, None);
    }
    let Some(system) = Ax::from_copy(unsafe { AXUIElementCreateSystemWide() }) else {
        return (None, None);
    };
    let mut raw = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyElementAtPosition(system.raw(), point.x as f32, point.y as f32, &mut raw)
    };
    let Some(element) = Ax::from_copy(raw) else {
        return (None, None);
    };
    if status != 0 {
        return (None, None);
    }
    let Some(pid) = element.pid() else {
        return (None, None);
    };
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .as_deref()
        .and_then(app_info_from)
        .map(|app| app.bundle_id);
    let window = element.window_id().or_else(|| {
        element
            .child("AXWindow")
            .and_then(|window| window.window_id())
    });
    (app, window)
}
fn collision_refusal(target: &Target, window_id: u32) -> Option<&'static str> {
    use std::sync::atomic::Ordering::Acquire;
    let state = COLLISION_MONITOR.get()?;
    if !state.available.load(Acquire) {
        return Some("collision_monitor_unavailable");
    }
    if state.pending.load(Acquire) > state.settled.load(Acquire) {
        return Some("user_collision");
    }
    state
        .last
        .lock()
        .ok()
        .and_then(|last| last.clone())
        .filter(|last| {
            last.generation > target.collision_generation
                && last.app.as_deref().is_none_or(|app| app == target.app)
                && (window_id == 0 || last.window_id.is_none_or(|id| id == window_id))
        })
        .map(|_| "user_collision")
}
fn collision_blocked(target: &Target, window_id: u32) -> bool {
    collision_refusal(target, window_id).is_some()
}
fn find_window_displays(input: &Value) -> Result<Value, ComputerError> {
    let apps = input
        .get("bundleIds")
        .and_then(Value::as_array)
        .ok_or_else(|| ComputerError::Other("bundleIds must be an array".into()))?;
    if apps.len() > 512 {
        return Err(ComputerError::Other(
            "application inventory exceeds budget".into(),
        ));
    }
    let apps: HashSet<&str> = apps
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| ComputerError::Other("bundleIds must contain strings".into()))
        })
        .collect::<Result<_, _>>()?;
    let monitors = xcap::Monitor::all().map_err(|e| ComputerError::Other(e.to_string()))?;
    let mut screens = Vec::new();
    for monitor in monitors {
        let id = monitor
            .id()
            .map_err(|e| ComputerError::Other(e.to_string()))?;
        let bounds = Bounds::new(
            f64::from(
                monitor
                    .x()
                    .map_err(|e| ComputerError::Other(e.to_string()))?,
            ),
            f64::from(
                monitor
                    .y()
                    .map_err(|e| ComputerError::Other(e.to_string()))?,
            ),
            f64::from(
                monitor
                    .width()
                    .map_err(|e| ComputerError::Other(e.to_string()))?,
            ),
            f64::from(
                monitor
                    .height()
                    .map_err(|e| ComputerError::Other(e.to_string()))?,
            ),
        );
        if let Some(bounds) = bounds {
            screens.push((id, bounds));
        }
    }
    let mut locations: std::collections::BTreeMap<String, std::collections::BTreeSet<u32>> =
        std::collections::BTreeMap::new();
    for window in xcap::Window::all().map_err(|e| ComputerError::Other(e.to_string()))? {
        let Ok(pid) = window
            .pid()
            .and_then(|pid| i32::try_from(pid).map_err(|_| xcap::XCapError::new("invalid pid")))
        else {
            continue;
        };
        let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .as_deref()
            .and_then(app_info_from)
        else {
            continue;
        };
        if !apps.contains(app.bundle_id.as_str()) {
            continue;
        }
        let (Ok(x), Ok(y), Ok(w), Ok(h)) =
            (window.x(), window.y(), window.width(), window.height())
        else {
            continue;
        };
        let Some(bounds) = Bounds::new(f64::from(x), f64::from(y), f64::from(w), f64::from(h))
        else {
            continue;
        };
        for (id, screen) in &screens {
            if bounds.x < screen.x + screen.w
                && bounds.x + bounds.w > screen.x
                && bounds.y < screen.y + screen.h
                && bounds.y + bounds.h > screen.y
            {
                locations
                    .entry(app.bundle_id.clone())
                    .or_default()
                    .insert(*id);
            }
        }
    }
    Ok(Value::Array(locations.into_iter().map(|(app,ids)|json!({"bundleId":app,"displayIds":ids.into_iter().collect::<Vec<_>>()})).collect()))
}
