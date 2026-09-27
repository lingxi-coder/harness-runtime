//! Windows capability boundary for the shared shell supervisor.
#![allow(unsafe_code)]
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

type Handle = *mut c_void;
#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: Handle,
    inherit: i32,
}
#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut Handle,
        size: *mut u32,
    ) -> i32;
    fn ConvertSecurityDescriptorToStringSecurityDescriptorW(
        descriptor: Handle,
        revision: u32,
        info: u32,
        text: *mut *mut u16,
        len: *mut u32,
    ) -> i32;
    fn GetSecurityInfo(
        handle: Handle,
        kind: u32,
        info: u32,
        owner: *mut Handle,
        group: *mut Handle,
        dacl: *mut Handle,
        sacl: *mut Handle,
        descriptor: *mut Handle,
    ) -> u32;
    fn OpenProcessToken(process: Handle, access: u32, token: *mut Handle) -> i32;
    fn GetTokenInformation(
        token: Handle,
        class: u32,
        data: Handle,
        size: u32,
        needed: *mut u32,
    ) -> i32;
    fn EqualSid(a: Handle, b: Handle) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(memory: Handle) -> Handle;
    fn GetCurrentProcess() -> Handle;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
    fn WaitForSingleObject(handle: Handle, timeout_ms: u32) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
    fn CreateDirectoryW(path: *const u16, attributes: *const SecurityAttributes) -> i32;
    fn GetNamedPipeServerProcessId(pipe: Handle, pid: *mut u32) -> i32;
}
#[link(name = "bcrypt")]
extern "system" {
    fn BCryptGenRandom(algorithm: Handle, bytes: *mut u8, len: u32, flags: u32) -> i32;
}
struct Local(Handle);
impl Drop for Local {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}
fn private_descriptor() -> std::io::Result<Local> {
    let text: Vec<u16> = "D:P(A;;GA;;;OW)(A;;GA;;;SY)\0".encode_utf16().collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: buffers are valid for this synchronous allocation; Local frees it.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    Ok(Local(descriptor))
}
pub(super) fn random_nonce() -> std::io::Result<String> {
    let mut bytes = [0u8; 24];
    // SAFETY: system-preferred RNG accepts a null algorithm and a valid buffer.
    if unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            2,
        )
    } < 0
    {
        return Err(std::io::Error::other("Windows system RNG failed"));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub(super) fn create_private_directory(nonce: &str) -> std::io::Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("lxs-{}", &nonce[..16]));
    let descriptor = private_descriptor()?;
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor.0,
        inherit: 0,
    };
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // Create-new only: never accept a precreated attacker-owned directory.
    if unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) } == 0 {
        return Err(last_error());
    }
    validate_private_directory(&path)?;
    Ok(path)
}
pub(super) fn validate_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    if !path.is_absolute() || !meta.is_dir() || meta.file_attributes() & 0x400 != 0 {
        return Err(std::io::Error::other(
            "supervisor directory is a reparse point or not absolute",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0200_0000 | 0x0020_0000)
        .open(path)?;
    let opened = file.metadata()?;
    if !opened.is_dir() || opened.file_attributes() & 0x400 != 0 {
        return Err(std::io::Error::other(
            "opened supervisor directory is a reparse point",
        ));
    }
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: file is held while querying owner and DACL; returned pointers live in descriptor.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            1,
            1 | 4,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = Local(descriptor);
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
        return Err(last_error());
    }
    struct Token(Handle);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let token = Token(token);
    let mut needed = 0;
    unsafe {
        GetTokenInformation(token.0, 1, std::ptr::null_mut(), 0, &mut needed);
    }
    // usize storage supplies the alignment required by TOKEN_USER/SID_AND_ATTRIBUTES.
    let mut data = vec![
        0usize;
        (needed as usize + std::mem::size_of::<usize>() - 1)
            / std::mem::size_of::<usize>()
    ];
    if unsafe { GetTokenInformation(token.0, 1, data.as_mut_ptr().cast(), needed, &mut needed) }
        == 0
    {
        return Err(last_error());
    }
    if data.is_empty() || unsafe { EqualSid(owner, data[0] as Handle) } == 0 {
        return Err(std::io::Error::other(
            "supervisor directory owner differs from current user",
        ));
    }
    let mut text = std::ptr::null_mut();
    let mut len = 0;
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor.0,
            1,
            4,
            &mut text,
            &mut len,
        )
    } == 0
    {
        return Err(last_error());
    }
    let allocation = Local(text.cast());
    let dacl = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) })
        .trim_end_matches('\0')
        .to_string();
    drop(allocation);
    // The filesystem maps GENERIC_ALL to FILE_ALL_ACCESS when creating the
    // directory; both serialize the same access rights for these two ACEs.
    if dacl.replace(";;FA;;;", ";;GA;;;") != "D:P(A;;GA;;;OW)(A;;GA;;;SY)" {
        return Err(std::io::Error::other(
            "supervisor directory DACL is not private and protected",
        ));
    }
    Ok(())
}
pub(super) fn create_pipe(endpoint: &str, first: bool) -> std::io::Result<NamedPipeServer> {
    let descriptor = private_descriptor()?;
    let mut attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor: descriptor.0,
        inherit: 0,
    };
    // SAFETY: CreateNamedPipe copies the security descriptor during this call.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                endpoint,
                (&mut attributes as *mut SecurityAttributes).cast(),
            )
    }
}
pub(super) fn verify_server(pipe: RawHandle, expected: u32) -> std::io::Result<()> {
    let mut actual = 0;
    // SAFETY: caller holds an open named-pipe client for this synchronous query.
    if unsafe { GetNamedPipeServerProcessId(pipe, &mut actual) } == 0 {
        return Err(last_error());
    }
    if actual != expected {
        return Err(std::io::Error::other("supervisor named-pipe PID mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_supervisor_directory_is_created_once_and_validated() {
        let nonce = random_nonce().unwrap();
        assert_eq!(nonce.len(), 48);
        let directory = create_private_directory(&nonce).unwrap();
        validate_private_directory(&directory).unwrap();
        assert!(create_private_directory(&nonce).is_err());
        std::fs::remove_dir(directory).unwrap();
    }
    #[tokio::test]
    async fn pipe_rejects_name_takeover_and_wrong_server_identity() {
        use tokio::net::windows::named_pipe::ClientOptions;
        let endpoint = format!(r"\\.\pipe\lingxi-shell-{}", random_nonce().unwrap());
        let server = create_pipe(&endpoint, true).unwrap();
        assert!(create_pipe(&endpoint, true).is_err());
        let client = ClientOptions::new().open(&endpoint).unwrap();
        server.connect().await.unwrap();
        verify_server(client.as_raw_handle(), std::process::id()).unwrap();
        assert!(verify_server(client.as_raw_handle(), std::process::id().wrapping_add(1)).is_err());
        // A replacement acceptor is created while the original is still held.
        let _next = create_pipe(&endpoint, false).unwrap();
        assert!(create_pipe(&endpoint, true).is_err());
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateMemoryResourceNotification(kind: u32) -> Handle;
    fn QueryMemoryResourceNotification(resource: Handle, state: *mut i32) -> i32;
}
pub(super) fn memory_pressure() -> bool {
    // A real OS low-memory signal, without inferring pressure from our own RSS.
    let handle = unsafe { CreateMemoryResourceNotification(0) };
    if handle.is_null() {
        return false;
    }
    let mut pressure = 0;
    let success = unsafe { QueryMemoryResourceNotification(handle, &mut pressure) };
    unsafe {
        CloseHandle(handle);
    }
    success != 0 && pressure != 0
}

/// Distinguish a known dead PID from an inaccessible process.
pub(super) fn process_is_alive(pid: u32) -> Option<bool> {
    // SAFETY: the query-only handle is closed on every successful-open path.
    let process = unsafe { OpenProcess(0x1000 | 0x0010_0000, 0, pid) };
    if process.is_null() {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(87) => Some(false), // ERROR_INVALID_PARAMETER: PID absent
            _ => None,
        };
    }
    // Waiting on the process object avoids mistaking an actual exit code 259
    // for STILL_ACTIVE, which GetExitCodeProcess alone cannot distinguish.
    let state = unsafe { WaitForSingleObject(process, 0) };
    unsafe {
        CloseHandle(process);
    }
    match state {
        0 => Some(false),
        258 => Some(true),
        _ => None,
    }
}
