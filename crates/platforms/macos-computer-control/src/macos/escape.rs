//! Foreground computer-use Escape abort hook, matched to the pinned 2.1.325
//! ComputerUseSwift callback (0x58098), expectation timer (0x584a8) and teardown.
//! No key characters, text or unrelated keystroke data are retained. A raw tap
//! is necessary: core-graphics 0.24's callback wrapper passes the original
//! event when its Rust callback returns None, whereas this hook must consume Esc.
use core_foundation::{
    base::TCFType,
    mach_port::{CFMachPort, CFMachPortInvalidate, CFMachPortRef},
    runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopSource, CFRunLoopSourceInvalidate},
};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

type Callback = Arc<dyn Fn() + Send + Sync>;
type Event = *mut c_void;
const KEY_DOWN: u32 = 10;
const ESCAPE_KEYCODE: i64 = 53;
const MODIFIER_MASK: u64 = 0x1e0000;
const EXPECTATION_LIFETIME: Duration = Duration::from_millis(100);
const ACK_TIMEOUT: Duration = Duration::from_millis(750);
const LOOP_SLICE: Duration = Duration::from_millis(10);

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        location: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(*mut c_void, u32, Event, *mut c_void) -> Event,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(port: CFMachPortRef, enable: bool);
    fn CGEventTapIsEnabled(port: CFMachPortRef) -> bool;
    fn CGEventGetIntegerValueField(event: Event, field: u32) -> i64;
    fn CGEventGetFlags(event: Event) -> u64;
}

#[derive(Default)]
struct Control {
    epoch: u64,
    active: bool,
    expected: u64,
    callback: Option<Callback>,
}
#[derive(Default)]
struct State {
    control: Mutex<Control>,
    port: AtomicUsize,
    callback_thread: Mutex<Option<ThreadId>>,
}
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Pass,
    Expected,
    Abort(u64),
}
impl State {
    fn lock(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn new_epoch(&self) -> u64 {
        let (epoch, old) = {
            let mut state = self.lock();
            state.epoch = state.epoch.wrapping_add(1);
            state.active = false;
            state.expected = 0;
            self.port.store(0, Ordering::Release);
            (state.epoch, state.callback.take())
        };
        // Dropping a host closure can release arbitrary host state; never do it
        // while holding the small event-filter mutex.
        drop(old);
        epoch
    }
    fn activate(&self, epoch: u64, callback: Callback) -> bool {
        let mut state = self.lock();
        if state.epoch != epoch {
            return false;
        }
        state.expected = 0;
        state.callback = Some(callback);
        state.active = true;
        true
    }
    fn is_active_epoch(&self, epoch: u64) -> bool {
        let state = self.lock();
        state.active && state.epoch == epoch
    }
    fn epoch_matches(&self, epoch: u64) -> bool {
        self.lock().epoch == epoch
    }
    fn expect(&self) -> Option<u64> {
        let mut state = self.lock();
        if !state.active {
            return None;
        }
        state.expected = state.expected.saturating_add(1);
        Some(state.epoch)
    }
    fn expire(&self, epoch: u64) {
        let mut state = self.lock();
        if state.epoch == epoch && state.expected > 0 {
            state.expected -= 1;
        }
    }
    fn decide(&self, event_type: u32, keycode: i64, flags: u64) -> Decision {
        if event_type != KEY_DOWN || keycode != ESCAPE_KEYCODE || flags & MODIFIER_MASK != 0 {
            return Decision::Pass;
        }
        let mut state = self.lock();
        if !state.active {
            return Decision::Pass;
        }
        if state.expected > 0 {
            state.expected -= 1;
            Decision::Expected
        } else {
            Decision::Abort(state.epoch)
        }
    }
    fn callback(&self, epoch: u64) -> Option<Callback> {
        let state = self.lock();
        (state.active && state.epoch == epoch)
            .then(|| state.callback.clone())
            .flatten()
    }
}

enum Dispatch {
    Abort(u64),
    Barrier(mpsc::SyncSender<()>),
}
struct CallbackContext {
    state: Arc<State>,
    dispatch: mpsc::Sender<Dispatch>,
}
unsafe extern "C" fn event_callback(
    _proxy: *mut c_void,
    kind: u32,
    event: Event,
    context: *mut c_void,
) -> Event {
    if context.is_null() {
        return event;
    }
    let context = unsafe { &*context.cast::<CallbackContext>() };
    if kind == u32::MAX || kind == u32::MAX - 1 {
        let port = context.state.port.load(Ordering::Acquire) as CFMachPortRef;
        if !port.is_null() {
            unsafe {
                CGEventTapEnable(port, true);
            }
        }
        return event;
    }
    if event.is_null() || kind != KEY_DOWN {
        return event;
    }
    let keycode = unsafe { CGEventGetIntegerValueField(event, 9) };
    if keycode != ESCAPE_KEYCODE {
        return event;
    }
    let flags = unsafe { CGEventGetFlags(event) };
    match context.state.decide(kind, keycode, flags) {
        Decision::Pass => event,
        Decision::Expected => std::ptr::null_mut(),
        Decision::Abort(epoch) => {
            let _ = context.dispatch.send(Dispatch::Abort(epoch));
            std::ptr::null_mut()
        }
    }
}
struct NativeTap {
    port: CFMachPort,
    source: CFRunLoopSource,
    runloop: CFRunLoop,
    context: Box<CallbackContext>,
}
impl NativeTap {
    fn install(
        state: Arc<State>,
        dispatch: mpsc::Sender<Dispatch>,
        epoch: u64,
        callback: Callback,
    ) -> Option<Self> {
        let context = Box::new(CallbackContext {
            state: state.clone(),
            dispatch,
        });
        let raw = unsafe {
            CGEventTapCreate(
                1,
                0,
                0,
                1 << KEY_DOWN,
                event_callback,
                (&*context as *const CallbackContext).cast_mut().cast(),
            )
        };
        if raw.is_null() {
            return None;
        }
        let port = unsafe { CFMachPort::wrap_under_create_rule(raw) };
        let source = match port.create_runloop_source(0) {
            Ok(source) => source,
            Err(()) => {
                unsafe {
                    CFMachPortInvalidate(raw);
                }
                return None;
            }
        };
        let runloop = CFRunLoop::get_current();
        let tap = Self {
            port,
            source,
            runloop,
            context,
        };
        tap.runloop
            .add_source(&tap.source, unsafe { kCFRunLoopDefaultMode });
        state.port.store(raw as usize, Ordering::Release);
        unsafe {
            CGEventTapEnable(raw, true);
        }
        if !unsafe { CGEventTapIsEnabled(raw) } || !state.activate(epoch, callback) {
            return None;
        }
        Some(tap)
    }
}
impl Drop for NativeTap {
    fn drop(&mut self) {
        let raw = self.port.as_concrete_TypeRef();
        let _ = self.context.state.port.compare_exchange(
            raw as usize,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        unsafe {
            CGEventTapEnable(raw, false);
            CFMachPortInvalidate(raw);
        }
        self.runloop
            .remove_source(&self.source, unsafe { kCFRunLoopDefaultMode });
        unsafe {
            CFRunLoopSourceInvalidate(self.source.as_concrete_TypeRef());
        }
        // The source is invalidated/removed on its own thread before context is
        // dropped; no native callback can retain a dangling user_info pointer.
    }
}

enum Command {
    Register {
        epoch: u64,
        callback: Callback,
        ack: mpsc::SyncSender<bool>,
    },
    Unregister {
        epoch: u64,
        ack: mpsc::SyncSender<()>,
    },
    Expected {
        epoch: u64,
        deadline: Instant,
    },
    Shutdown,
}
#[derive(Clone)]
struct Controller {
    commands: mpsc::Sender<Command>,
    dispatch: mpsc::Sender<Dispatch>,
}
fn start_controller(state: Arc<State>) -> Option<Controller> {
    let (dispatch, reader) = mpsc::channel();
    let callback_state = state.clone();
    std::thread::Builder::new()
        .name("computer-escape-abort".into())
        .spawn(move || {
            *callback_state
                .callback_thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(std::thread::current().id());
            while let Ok(job) = reader.recv() {
                match job {
                    Dispatch::Abort(epoch) => {
                        if let Some(callback) = callback_state.callback(epoch) {
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                callback()
                            }));
                        }
                    }
                    Dispatch::Barrier(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        })
        .ok()?;
    let (commands, receiver) = mpsc::channel();
    let native_dispatch = dispatch.clone();
    std::thread::Builder::new()
        .name("computer-escape-listener".into())
        .spawn(move || {
            let mut tap = None;
            let mut timers: Vec<(u64, Instant)> = Vec::new();
            loop {
                let first = if tap.is_some() {
                    receiver.try_recv().ok()
                } else {
                    match receiver.recv_timeout(LOOP_SLICE) {
                        Ok(command) => Some(command),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                };
                let mut current = first;
                let mut shutdown = false;
                for batch_index in 0..64 {
                    let Some(command) = current.take() else {
                        break;
                    };
                    match command {
                        Command::Register {
                            epoch,
                            callback,
                            ack,
                        } => {
                            // A timed-out caller invalidates the epoch; a delayed
                            // startup must never resurrect a supposedly failed hook.
                            if state.epoch_matches(epoch) {
                                tap.take();
                                timers.clear();
                                tap = NativeTap::install(
                                    state.clone(),
                                    native_dispatch.clone(),
                                    epoch,
                                    callback,
                                );
                            }
                            let _ = ack.send(tap.is_some() && state.is_active_epoch(epoch));
                        }
                        Command::Unregister { epoch, ack } => {
                            if state.epoch_matches(epoch) {
                                tap.take();
                                timers.clear();
                            }
                            let _ = ack.send(());
                        }
                        Command::Expected { epoch, deadline } => timers.push((epoch, deadline)),
                        Command::Shutdown => {
                            shutdown = true;
                            break;
                        }
                    }
                    current = if batch_index < 63 {
                        receiver.try_recv().ok()
                    } else {
                        None
                    };
                }
                let now = Instant::now();
                timers.retain(|(epoch, deadline)| {
                    if *deadline <= now {
                        state.expire(*epoch);
                        false
                    } else {
                        true
                    }
                });
                if shutdown {
                    break;
                }
                if tap.is_some() {
                    CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, LOOP_SLICE, true);
                }
            }
            state.new_epoch();
            drop(tap);
        })
        .ok()?;
    Some(Controller { commands, dispatch })
}

#[derive(Default)]
pub(super) struct EscapeHook {
    state: Arc<State>,
    controller: Mutex<Option<Controller>>,
}
impl EscapeHook {
    pub(super) fn register(&self, callback: Callback) -> bool {
        let mut controller = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let epoch = self.state.new_epoch();
        if controller.is_none() {
            *controller = start_controller(self.state.clone());
        }
        let Some(controller) = controller.as_ref() else {
            return false;
        };
        let (ack, result) = mpsc::sync_channel(1);
        if controller
            .commands
            .send(Command::Register {
                epoch,
                callback,
                ack,
            })
            .is_err()
        {
            return false;
        }
        match result.recv_timeout(ACK_TIMEOUT) {
            Ok(true) if self.state.is_active_epoch(epoch) => true,
            _ => {
                let epoch = self.state.new_epoch();
                let (ack, _) = mpsc::sync_channel(1);
                let _ = controller.commands.send(Command::Unregister { epoch, ack });
                false
            }
        }
    }
    pub(super) fn unregister(&self) {
        let epoch = self.state.new_epoch();
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(controller) = controller else {
            return;
        };
        let (ack, result) = mpsc::sync_channel(1);
        if controller
            .commands
            .send(Command::Unregister { epoch, ack })
            .is_ok()
        {
            let _ = result.recv_timeout(ACK_TIMEOUT);
        }
        // Drain previously queued callbacks before ordinary teardown returns.
        // A callback may itself trigger teardown; never wait on its own queue.
        let in_callback = self
            .state
            .callback_thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            == Some(&std::thread::current().id());
        if !in_callback {
            let (ack, result) = mpsc::sync_channel(1);
            if controller.dispatch.send(Dispatch::Barrier(ack)).is_ok() {
                let _ = result.recv_timeout(ACK_TIMEOUT);
            }
        }
    }
    pub(super) fn notify_expected(&self) {
        let Some(epoch) = self.state.expect() else {
            return;
        };
        let deadline = Instant::now() + EXPECTATION_LIFETIME;
        if let Some(controller) = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = controller
                .commands
                .send(Command::Expected { epoch, deadline });
        }
    }
}
impl Drop for EscapeHook {
    fn drop(&mut self) {
        self.unregister();
        if let Some(controller) = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = controller.commands.send(Command::Shutdown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn active() -> (State, u64) {
        let state = State::default();
        let epoch = state.new_epoch();
        assert!(state.activate(epoch, Arc::new(|| {})));
        (state, epoch)
    }
    #[test]
    fn only_unmodified_keydown_escape_is_consumed() {
        let (state, epoch) = active();
        for kind in [0, 1, 11, u32::MAX, u32::MAX - 1] {
            assert_eq!(state.decide(kind, 53, 0), Decision::Pass);
        }
        for key in [0, 52, 54, 255] {
            assert_eq!(state.decide(KEY_DOWN, key, 0), Decision::Pass);
        }
        for flags in [0x20000, 0x40000, 0x80000, 0x100000, 0x1e0000] {
            assert_eq!(state.decide(KEY_DOWN, 53, flags), Decision::Pass);
        }
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Abort(epoch));
        assert_eq!(state.decide(KEY_DOWN, 53, 0x10000), Decision::Abort(epoch));
        assert_eq!(state.decide(KEY_DOWN, 53, 0x800000), Decision::Abort(epoch));
    }
    #[test]
    fn expected_escape_is_consumed_once_without_abort() {
        let (state, epoch) = active();
        assert_eq!(state.expect(), Some(epoch));
        assert_eq!(state.decide(KEY_DOWN, 53, 0x20000), Decision::Pass);
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Expected);
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Abort(epoch));
    }
    #[test]
    fn expectation_expiry_decrements_only_positive_count_in_same_epoch() {
        let (state, epoch) = active();
        state.expect();
        state.expect();
        state.expire(epoch);
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Expected);
        state.expire(epoch);
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Abort(epoch));
    }
    #[test]
    fn teardown_clears_callback_and_counter_and_invalidates_old_timers() {
        let (state, epoch) = active();
        state.expect();
        let next = state.new_epoch();
        assert_ne!(epoch, next);
        assert!(state.callback(epoch).is_none());
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Pass);
        assert!(state.activate(next, Arc::new(|| {})));
        state.expect();
        state.expire(epoch);
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Expected);
        assert_eq!(state.decide(KEY_DOWN, 53, 0), Decision::Abort(next));
    }
    #[test]
    fn delayed_registration_cannot_revive_invalidated_epoch() {
        let state = State::default();
        let old = state.new_epoch();
        let current = state.new_epoch();
        assert!(!state.activate(old, Arc::new(|| {})));
        assert!(!state.is_active_epoch(current));
        assert!(state.activate(current, Arc::new(|| {})));
        assert!(state.callback(old).is_none());
    }
}
