use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;

use winapi::shared::minwindef::{DWORD, FALSE};
use winapi::um::consoleapi::{GetConsoleMode, SetConsoleMode};
use winapi::um::fileapi::ReadFile;
use winapi::um::handleapi::INVALID_HANDLE_VALUE;
use winapi::um::processenv::GetStdHandle;
use winapi::um::synchapi::{CreateEventW, SetEvent, WaitForMultipleObjects};
use winapi::um::winbase::{INFINITE, STD_INPUT_HANDLE, WAIT_OBJECT_0};
use winapi::um::winnt::HANDLE;

const ENABLE_VIRTUAL_TERMINAL_INPUT: DWORD = 0x0200;
const INPUT_QUEUE_CAPACITY: usize = 128;
const PUMP_RUNNING: u8 = 0;
const PUMP_QUEUE_OVERFLOW: u8 = 1;
const PUMP_INPUT_CLOSED: u8 = 2;
const PUMP_READ_FAILED: u8 = 3;
const PUMP_WAIT_FAILED: u8 = 4;

/// Cancellable raw-byte reader for the Windows console. Enabling virtual
/// terminal input makes arrows, function keys, mouse input, and paste arrive as
/// the same VT byte sequences a Unix terminal supplies. A dedicated stop event
/// wakes the reader without leaving an orphan blocking stdin thread.
pub struct WindowsStdinPump {
    receiver: Receiver<Vec<u8>>,
    stop_event: OwnedHandle,
    thread: Option<JoinHandle<()>>,
    stdin_handle: HANDLE,
    original_mode: DWORD,
    failure: Arc<AtomicU8>,
}

// Windows kernel handles may be transferred between threads. The pump moves
// ownership exactly once to its input thread; Drop restores the console mode
// and closes the owned stop event on that same final owner.
unsafe impl Send for WindowsStdinPump {}

impl WindowsStdinPump {
    /// Start a bounded, cancellable VT-input pump for the current console.
    pub fn start() -> io::Result<Self> {
        let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if stdin_handle.is_null() || stdin_handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut original_mode = 0;
        if unsafe { GetConsoleMode(stdin_handle, &mut original_mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { SetConsoleMode(stdin_handle, original_mode | ENABLE_VIRTUAL_TERMINAL_INPUT) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }

        let stop = unsafe {
            // Manual-reset event: once Drop signals it, every waiter stays awake.
            CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null())
        };
        if stop.is_null() {
            let error = io::Error::last_os_error();
            let _ = unsafe { SetConsoleMode(stdin_handle, original_mode) };
            return Err(error);
        }
        let stop_event = unsafe { OwnedHandle::from_raw_handle(stop.cast()) };
        let stop_raw = stop_event.as_raw_handle() as usize;
        let stdin_raw = stdin_handle as usize;
        let (sender, receiver) = mpsc::sync_channel(INPUT_QUEUE_CAPACITY);
        let failure = Arc::new(AtomicU8::new(PUMP_RUNNING));
        let thread_failure = Arc::clone(&failure);
        let thread = std::thread::Builder::new()
            .name("lingxi-windows-stdin".to_string())
            .spawn(move || {
                let stop = stop_raw as HANDLE;
                let stdin = stdin_raw as HANDLE;
                let handles = [stop, stdin];
                let mut buffer = [0_u8; 8192];
                loop {
                    let wait = unsafe {
                        WaitForMultipleObjects(
                            DWORD::try_from(handles.len()).unwrap_or(2),
                            handles.as_ptr(),
                            FALSE,
                            INFINITE,
                        )
                    };
                    if wait == WAIT_OBJECT_0 {
                        break;
                    }
                    if wait != WAIT_OBJECT_0 + 1 {
                        thread_failure.store(PUMP_WAIT_FAILED, Ordering::Release);
                        break;
                    }
                    let mut read = 0_u32;
                    let ok = unsafe {
                        ReadFile(
                            stdin,
                            buffer.as_mut_ptr().cast(),
                            DWORD::try_from(buffer.len()).unwrap_or(8192),
                            &mut read,
                            std::ptr::null_mut(),
                        )
                    };
                    if ok == 0 {
                        thread_failure.store(PUMP_READ_FAILED, Ordering::Release);
                        break;
                    }
                    if read == 0 {
                        thread_failure.store(PUMP_INPUT_CLOSED, Ordering::Release);
                        break;
                    }
                    let Ok(read) = usize::try_from(read) else {
                        break;
                    };
                    match sender.try_send(buffer[..read].to_vec()) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            // Expose overflow to the attach loop. It will close
                            // the controller and restore the local terminal;
                            // silently losing the console reader would strand
                            // the user in raw/alternate-screen mode.
                            thread_failure.store(PUMP_QUEUE_OVERFLOW, Ordering::Release);
                            break;
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            thread_failure.store(PUMP_INPUT_CLOSED, Ordering::Release);
                            break;
                        }
                    }
                }
            })?;

        Ok(Self {
            receiver,
            stop_event,
            thread: Some(thread),
            stdin_handle,
            original_mode,
            failure,
        })
    }

    /// Try to receive the next raw VT input chunk.
    pub fn try_recv(&self) -> Result<Vec<u8>, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Return the terminal-reader failure, if it stopped unexpectedly.
    /// Callers must terminate the attach so their raw-mode guard can unwind.
    #[must_use]
    pub fn failure_message(&self) -> Option<&'static str> {
        match self.failure.load(Ordering::Acquire) {
            PUMP_RUNNING => None,
            PUMP_QUEUE_OVERFLOW => Some("Windows terminal input queue overflowed"),
            PUMP_INPUT_CLOSED => Some("Windows terminal input closed"),
            PUMP_READ_FAILED => Some("failed to read Windows terminal input"),
            PUMP_WAIT_FAILED => Some("failed waiting for Windows terminal input"),
            _ => Some("Windows terminal input stopped"),
        }
    }
}

impl Drop for WindowsStdinPump {
    fn drop(&mut self) {
        let _ = unsafe { SetEvent(self.stop_event.as_raw_handle().cast()) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = unsafe { SetConsoleMode(self.stdin_handle, self.original_mode) };
    }
}
