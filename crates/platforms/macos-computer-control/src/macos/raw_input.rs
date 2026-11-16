//! Process-directed input. Never posts to a global event tap or warps the cursor.
//! Mouse window-local SPI ABI is declared by WebKit CoreGraphicsTestSPI.h and
//! resolved dynamically; fixture delivery is measured separately from reference
//! parity (the pinned CLI native module does not contain the desktop host path).
use super::ComputerError;
use core_graphics::{
    event::{CGEvent, CGEventFlags},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::{NSPoint, NSProcessInfo, NSString};
use std::{ffi::c_void, sync::OnceLock};
#[link(name = "System")]
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, name: *const std::ffi::c_char) -> *mut c_void;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventPostToPid(pid: i32, event: *const c_void);
}
type SetWindow = unsafe extern "C" fn(*const c_void, objc2_core_foundation::CGPoint);
fn window_setter() -> Option<SetWindow> {
    static FN: OnceLock<Option<SetWindow>> = OnceLock::new();
    *FN.get_or_init(|| {
        let p = unsafe {
            dlsym(
                (-2isize) as *mut c_void,
                c"CGEventSetWindowLocation".as_ptr(),
            )
        };
        (!p.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, SetWindow>(p) })
    })
}
pub(super) fn available() -> bool {
    window_setter().is_some()
}
/// `local` is relative to the target frame's upper-left, in logical points.
pub(super) fn mouse(
    pid: i32,
    id: u32,
    screen: (f64, f64),
    local: (f64, f64),
    kind: NSEventType,
    count: usize,
) -> Result<(), ComputerError> {
    let set = window_setter().ok_or_else(|| {
        ComputerError::Unsupported("background window-local event location".into())
    })?;
    if pid <= 0
        || id == 0
        || ![screen.0, screen.1, local.0, local.1]
            .iter()
            .all(|v| v.is_finite())
        || !(1..=3).contains(&count)
    {
        return Err(ComputerError::Other(
            "invalid background mouse event".into(),
        ));
    }
    let event=NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind,NSPoint::new(local.0,local.1),NSEventModifierFlags::empty(),NSProcessInfo::processInfo().systemUptime(),id as isize,None,9100,count as isize,if kind==NSEventType::LeftMouseUp {0.}else{1.})
        .ok_or_else(||ComputerError::Other("background mouse event allocation failed".into()))?;
    let cg = event
        .CGEvent()
        .ok_or_else(|| ComputerError::Other("background mouse conversion failed".into()))?;
    use objc2_core_graphics::{CGEvent as Cg, CGEventField};
    Cg::set_location(
        Some(&cg),
        objc2_core_foundation::CGPoint::new(screen.0, screen.1),
    );
    for field in [
        CGEventField::MouseEventWindowUnderMousePointer,
        CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent,
    ] {
        Cg::set_integer_value_field(Some(&cg), field, i64::from(id));
    }
    // The NSEvent constructor carries window identity. Its CGEvent conversion
    // does not populate window-local coordinates on current macOS; the verified
    // SPI supplies them without modifying global cursor state.
    let raw = (&*cg as *const objc2_core_graphics::CGEvent).cast();
    unsafe {
        set(raw, objc2_core_foundation::CGPoint::new(local.0, local.1));
        CGEventPostToPid(pid, raw);
    }
    Ok(())
}
pub(super) fn text(pid: i32, id: u32, text: &str) -> Result<(), ComputerError> {
    // Allocate both halves before posting down; allocation failure must not
    // leave an application holding a synthetic key without its matching up.
    let s = NSString::from_str(text);
    let make = |kind| {
        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(kind,NSPoint::ZERO,NSEventModifierFlags::empty(),NSProcessInfo::processInfo().systemUptime(),id as isize,None,&s,&s,false,0)
        .and_then(|e|e.CGEvent()).ok_or_else(||ComputerError::Other("background Unicode event allocation failed".into()))
    };
    let down = make(NSEventType::KeyDown)?;
    let up = make(NSEventType::KeyUp)?;
    unsafe {
        CGEventPostToPid(pid, (&*down as *const objc2_core_graphics::CGEvent).cast());
        CGEventPostToPid(pid, (&*up as *const objc2_core_graphics::CGEvent).cast());
    }
    Ok(())
}

pub(super) fn key(pid: i32, code: u16, flags: CGEventFlags) -> Result<(), ComputerError> {
    let source = CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| ComputerError::Other("background key source unavailable".into()))?;
    let down = CGEvent::new_keyboard_event(source.clone(), code, true)
        .map_err(|_| ComputerError::Other("background key allocation failed".into()))?;
    let up = CGEvent::new_keyboard_event(source, code, false)
        .map_err(|_| ComputerError::Other("background key allocation failed".into()))?;
    down.set_flags(flags);
    up.set_flags(flags);
    down.post_to_pid(pid);
    up.post_to_pid(pid);
    Ok(())
}
