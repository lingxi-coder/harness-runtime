//! Window-to-current-Space native bridge. Private ABI is inferred from current
//! yabai source, and checked against the loaded SkyLight image/runtime metadata.
//! This does not claim the unobserved Claude desktop host uses the same ABI.
use super::ComputerError;
use core_foundation::{
    array::CFArray,
    base::{CFType, CFTypeRef, TCFType},
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject},
    sel,
};
use objc2_foundation::{NSArray, NSNumber};
use serde_json::{json, Value};
use std::{
    ffi::{c_char, c_void, CStr},
    sync::OnceLock,
    time::{Duration, Instant},
};

#[link(name = "System")]
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(index: u32) -> *const c_char;
    fn _dyld_get_image_header(index: u32) -> *const u8;
    fn _dyld_get_image_vmaddr_slide(index: u32) -> isize;
}
#[link(name = "objc")]
unsafe extern "C" {
    fn class_getInstanceMethod(class: *const AnyClass, sel: objc2::runtime::Sel) -> *mut c_void;
    fn method_getTypeEncoding(method: *mut c_void) -> *const c_char;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, window: u32) -> CFTypeRef;
}
const IMAGE: &CStr = c"/System/Library/PrivateFrameworks/SkyLight.framework/Versions/A/SkyLight";
const BRIDGE:&[u8]=b"__ZL54SLSPerformAsynchronousBridgedWindowManagementOperationP47SLSAsynchronousBridgedWindowManagementOperation\0";
#[derive(Clone, Copy)]
struct Api {
    connection: usize,
    active: usize,
    space_type: usize,
    memberships: usize,
    modern: Option<usize>,
    legacy: Option<usize>,
}
static API: OnceLock<Option<Api>> = OnceLock::new();
fn symbol(handle: *mut c_void, name: &CStr) -> Option<usize> {
    let p = unsafe { dlsym(handle, name.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}
fn api() -> Option<Api> {
    *API.get_or_init(|| unsafe {
        let handle = dlopen(IMAGE.as_ptr(), 1);
        if handle.is_null() {
            return None;
        }
        let class = AnyClass::get(c"SLSBridgedMoveWindowsToManagedSpaceOperation");
        let modern = class.and_then(|class| {
            let method = class_getInstanceMethod(class, sel!(initWithWindows:spaceID:));
            if method.is_null() {
                return None;
            }
            let encoding = method_getTypeEncoding(method);
            if encoding.is_null() || CStr::from_ptr(encoding).to_bytes() != b"@32@0:8@16Q24" {
                return None;
            }
            bounded_local_bridge()
        });
        let legacy = symbol(handle, c"SLSMoveWindowsToManagedSpace");
        if modern.is_none() && legacy.is_none() {
            return None;
        }
        Some(Api {
            connection: symbol(handle, c"SLSMainConnectionID")?,
            active: symbol(handle, c"SLSGetActiveSpace")?,
            space_type: symbol(handle, c"SLSSpaceGetType")?,
            memberships: symbol(handle, c"SLSCopySpacesForWindows")?,
            modern,
            legacy,
        })
    })
}
pub(super) fn available() -> bool {
    api().is_some()
}
fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}
fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}
fn contained(start: u64, len: u64, base: u64, size: u64) -> bool {
    start >= base
        && start
            .checked_add(len)
            .zip(base.checked_add(size))
            .is_some_and(|(a, b)| a <= b)
}
/// Loaded OS image only. Validate every command/table against __LINKEDIT, bound
/// the scan to 1M entries and compare only the fixed symbol bytes. Shared-cache
/// string regions can approach 1GB: never copy or scan the complete string table.
unsafe fn bounded_local_bridge() -> Option<usize> {
    let images = unsafe { _dyld_image_count() }.min(16384);
    for i in 0..images {
        let name = unsafe { _dyld_get_image_name(i) };
        if name.is_null() {
            continue;
        }
        if unsafe { CStr::from_ptr(name) } != IMAGE {
            continue;
        }
        let header = unsafe { _dyld_get_image_header(i) };
        if header.is_null() {
            return None;
        }
        let h = unsafe { std::slice::from_raw_parts(header, 32) };
        if u32_at(h, 0)? != 0xfeedfacf {
            return None;
        }
        let count = u32_at(h, 16)? as usize;
        let size = u32_at(h, 20)? as usize;
        if count > 8192 || size > 1_048_576 {
            return None;
        }
        let commands = unsafe { std::slice::from_raw_parts(header.add(32), size) };
        let mut offset = 0;
        let mut text = None;
        let mut link = None;
        let mut symbols = None;
        for _ in 0..count {
            let kind = u32_at(commands, offset)?;
            let length = u32_at(commands, offset + 4)? as usize;
            if length < 8 || offset.checked_add(length)? > size {
                return None;
            }
            let c = &commands[offset..offset + length];
            if kind == 0x19 && length >= 72 {
                let segment = c.get(8..24)?;
                let bounds = (
                    u64_at(c, 24)?,
                    u64_at(c, 32)?,
                    u64_at(c, 40)?,
                    u64_at(c, 48)?,
                );
                if segment.starts_with(b"__TEXT\0") {
                    text = Some(bounds);
                }
                if segment.starts_with(b"__LINKEDIT\0") {
                    link = Some(bounds);
                }
            } else if kind == 2 && length >= 24 {
                symbols = Some((
                    u32_at(c, 8)? as u64,
                    u32_at(c, 12)? as u64,
                    u32_at(c, 16)? as u64,
                    u32_at(c, 20)? as u64,
                ));
            }
            offset += length;
        }
        let (text_base, text_size, _, _) = text?;
        let (vm, _, file, file_size) = link?;
        let (symoff, nsyms, stroff, strsize) = symbols?;
        if nsyms > 1_000_000
            || !contained(symoff, nsyms.checked_mul(16)?, file, file_size)
            || !contained(stroff, strsize, file, file_size)
        {
            return None;
        }
        let slide = unsafe { _dyld_get_image_vmaddr_slide(i) };
        let mapped = vm.checked_sub(file)?.checked_add_signed(slide as i64)?;
        for index in 0..nsyms {
            let at = mapped.checked_add(symoff)?.checked_add(index * 16)?;
            let entry = unsafe { std::slice::from_raw_parts(at as *const u8, 16) };
            let strx = u32_at(entry, 0)? as u64;
            if strx.checked_add(BRIDGE.len() as u64)? > strsize {
                continue;
            }
            if entry[4] & 0x0e != 0x0e || entry[5] == 0 {
                continue;
            }
            let address = u64_at(entry, 8)?;
            if !contained(address, 4, text_base, text_size) {
                continue;
            }
            let candidate = mapped.checked_add(stroff)?.checked_add(strx)?;
            if unsafe { std::slice::from_raw_parts(candidate as *const u8, BRIDGE.len()) } == BRIDGE
            {
                return usize::try_from(address.checked_add_signed(slide as i64)?).ok();
            }
        }
        return None;
    }
    None
}
fn typed_array(value: &CFType) -> Option<CFArray<CFType>> {
    value.downcast::<CFArray>()?;
    Some(unsafe { CFArray::wrap_under_get_rule(value.as_concrete_TypeRef().cast()) })
}
fn typed_dictionary(value: &CFType) -> Option<CFDictionary<CFString, CFType>> {
    value.downcast::<CFDictionary>()?;
    Some(unsafe { CFDictionary::wrap_under_get_rule(value.as_concrete_TypeRef().cast()) })
}
pub(super) fn owner_matches(pid: i32, id: u32) -> bool {
    let raw = unsafe { CGWindowListCopyWindowInfo(1 << 3, id) };
    if raw.is_null() {
        return false;
    }
    let value = unsafe { CFType::wrap_under_create_rule(raw) };
    let Some(array) = typed_array(&value) else {
        return false;
    };
    if array.len() != 1 {
        return false;
    }
    let Some(value) = array.get(0) else {
        return false;
    };
    let Some(dict) = typed_dictionary(&value) else {
        return false;
    };
    let number = |key: &str| {
        dict.find(CFString::new(key))
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|v| v.to_i64())
    };
    number("kCGWindowNumber") == Some(i64::from(id))
        && number("kCGWindowOwnerPID") == Some(i64::from(pid))
}
fn memberships(api: Api, cid: i32, id: u32) -> Option<Vec<u64>> {
    let window = CFArray::from_CFTypes(&[CFNumber::from(id as i64)]);
    let query: unsafe extern "C" fn(i32, i32, *const c_void) -> CFTypeRef =
        unsafe { std::mem::transmute(api.memberships) };
    let raw = unsafe { query(cid, 7, window.as_concrete_TypeRef().cast()) };
    if raw.is_null() {
        return None;
    }
    let value = unsafe { CFType::wrap_under_create_rule(raw) };
    let array = typed_array(&value)?;
    if array.len() > 128 {
        return None;
    }
    let mut result = Vec::new();
    for v in array.iter() {
        let number = v.downcast::<CFNumber>()?.to_i64()?;
        result.push(u64::try_from(number).ok()?);
    }
    Some(result)
}
fn record(code: &str) -> Value {
    json!({"code":code,"moved":false,"alreadyHere":false})
}
/// Caller also holds the exact bundle/PID/launch/window AX identity and grant
/// lease. `guard` rechecks all of those plus cancellation immediately before
/// native mutation; this helper independently rechecks CGWindow owner PID.
pub(super) fn bring(
    pid: i32,
    id: u32,
    dry_run: bool,
    guard: &dyn Fn() -> bool,
) -> Result<Value, ComputerError> {
    let Some(api) = api() else {
        return Ok(record("op_unavailable"));
    };
    if !guard() {
        return Ok(record("move_failed"));
    }
    if !owner_matches(pid, id) {
        return Ok(record("owner_mismatch"));
    }
    let connection: unsafe extern "C" fn() -> i32 = unsafe { std::mem::transmute(api.connection) };
    let active: unsafe extern "C" fn(i32) -> u64 = unsafe { std::mem::transmute(api.active) };
    let space_type: unsafe extern "C" fn(i32, u64) -> i32 =
        unsafe { std::mem::transmute(api.space_type) };
    let cid = unsafe { connection() };
    let destination = unsafe { active(cid) };
    if cid <= 0 || destination == 0 {
        return Ok(record("space_unclassifiable"));
    }
    let Some(before) = memberships(api, cid, id) else {
        return Ok(record("space_unclassifiable"));
    };
    if before.is_empty() {
        return Ok(record("space_unclassifiable"));
    }
    if before.contains(&destination) {
        return Ok(json!({"alreadyHere":true,"moved":false}));
    }
    match unsafe { space_type(cid, destination) } {
        0 => {}
        4 => return Ok(record("fullscreen_space")),
        _ => return Ok(record("space_unclassifiable")),
    }
    if dry_run {
        return Ok(json!({"alreadyHere":false,"moved":false}));
    }
    // Re-query destination/classification/ownership after all setup. No focus,
    // Space switch, global mouse/key input or app activation is performed here.
    if !guard() {
        return Ok(record("move_failed"));
    }
    if !owner_matches(pid, id) {
        return Ok(record("owner_mismatch"));
    }
    if unsafe { active(cid) } != destination || unsafe { space_type(cid, destination) } != 0 {
        return Ok(record("space_unclassifiable"));
    }
    if let Some(function) = api.modern {
        let Some(class) = AnyClass::get(c"SLSBridgedMoveWindowsToManagedSpaceOperation") else {
            return Ok(record("op_unavailable"));
        };
        let windows = NSArray::from_retained_slice(&[NSNumber::numberWithUnsignedInt(id)]);
        let allocated: *mut AnyObject = unsafe { msg_send![class, alloc] };
        let operation: *mut AnyObject =
            unsafe { msg_send![allocated,initWithWindows:&*windows,spaceID:destination] };
        let Some(operation) = (unsafe { Retained::from_raw(operation) }) else {
            return Ok(record("op_unavailable"));
        };
        if !guard() {
            return Ok(record("move_failed"));
        }
        if !owner_matches(pid, id) {
            return Ok(record("owner_mismatch"));
        }
        if unsafe { active(cid) } != destination || unsafe { space_type(cid, destination) } != 0 {
            return Ok(record("space_unclassifiable"));
        }
        let perform: unsafe extern "C" fn(*const AnyObject) -> i64 =
            unsafe { std::mem::transmute(function) };
        unsafe {
            perform(&*operation);
        }
    } else if let Some(function) = api.legacy {
        let windows = CFArray::from_CFTypes(&[CFNumber::from(id as i64)]);
        let perform: unsafe extern "C" fn(i32, *const c_void, u64) =
            unsafe { std::mem::transmute(function) };
        if !guard() || !owner_matches(pid, id) {
            return Ok(record("owner_mismatch"));
        }
        unsafe {
            perform(cid, windows.as_concrete_TypeRef().cast(), destination);
        }
    } else {
        return Ok(record("op_unavailable"));
    }
    let deadline = Instant::now() + Duration::from_millis(750);
    loop {
        if !guard() || !owner_matches(pid, id) {
            return Ok(record("owner_mismatch"));
        }
        if unsafe { active(cid) } != destination {
            return Ok(record("space_unclassifiable"));
        }
        if memberships(api, cid, id).is_some_and(|s| s.contains(&destination)) {
            return Ok(json!({"moved":true,"alreadyHere":false}));
        }
        if Instant::now() >= deadline {
            return Ok(record("move_failed"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_reject_overflow_and_cross_segment_tables() {
        assert!(contained(100, 16, 90, 30));
        assert!(!contained(100, 21, 90, 30));
        assert!(!contained(u64::MAX - 1, 8, 0, u64::MAX));
    }
    #[test]
    fn unavailable_records_never_claim_a_move() {
        for code in [
            "op_unavailable",
            "owner_mismatch",
            "space_unclassifiable",
            "fullscreen_space",
            "move_failed",
        ] {
            let v = record(code);
            assert_eq!(v["code"], code);
            assert_eq!(v["moved"], false);
        }
    }
}
