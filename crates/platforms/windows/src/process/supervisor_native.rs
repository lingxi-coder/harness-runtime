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
    fn ConvertSidToStringSidW(sid: Handle, text: *mut *mut u16) -> i32;
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
struct Token(Handle);
impl Drop for Token {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[repr(C)]
struct SidAndAttributes {
    sid: Handle,
    attributes: u32,
}
#[repr(C)]
struct TokenUserInformation {
    user: SidAndAttributes,
}

// TOKEN_USER contains a pointer to a SID stored in the same allocation.
struct TokenUser(Vec<usize>);
impl TokenUser {
    fn current() -> std::io::Result<Self> {
        let mut token = std::ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
            return Err(last_error());
        }
        let token = Token(token);
        let mut needed = 0;
        if unsafe { GetTokenInformation(token.0, 1, std::ptr::null_mut(), 0, &mut needed) } != 0 {
            return Err(std::io::Error::other("unexpected empty token user"));
        }
        let error = last_error();
        if error.raw_os_error() != Some(122) {
            return Err(error);
        }
        if (needed as usize) < std::mem::size_of::<TokenUserInformation>() {
            return Err(std::io::Error::other("invalid token user size"));
        }
        // usize storage supplies the alignment required by TOKEN_USER/SID_AND_ATTRIBUTES.
        let mut data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe { GetTokenInformation(token.0, 1, data.as_mut_ptr().cast(), needed, &mut needed) }
            == 0
        {
            return Err(last_error());
        }
        let user = Self(data);
        if user.sid().is_null() {
            return Err(std::io::Error::other("token user SID is missing"));
        }
        Ok(user)
    }

    fn sid(&self) -> Handle {
        // SAFETY: the aligned buffer contains a TOKEN_USER written by Windows.
        unsafe { (*self.0.as_ptr().cast::<TokenUserInformation>()).user.sid }
    }
}

fn verify_user_owner(owner: Handle, expected_user: Handle) -> std::io::Result<()> {
    // Both SIDs are provided by Windows and remain allocated during EqualSid.
    if owner.is_null() || expected_user.is_null() || unsafe { EqualSid(owner, expected_user) } == 0
    {
        return Err(std::io::Error::other(
            "supervisor directory owner differs from current user",
        ));
    }
    Ok(())
}

fn descriptor_with_dacl(dacl: &str) -> std::io::Result<Local> {
    let user = TokenUser::current()?;
    let mut sid_text = std::ptr::null_mut();
    // SAFETY: user owns the TOKEN_USER buffer, and Local frees the string.
    if unsafe { ConvertSidToStringSidW(user.sid(), &mut sid_text) } == 0 {
        return Err(last_error());
    }
    let _sid_allocation = Local(sid_text.cast());
    let mut sid_len = 0;
    // SAFETY: ConvertSidToStringSidW returns a null-terminated UTF-16 string.
    while unsafe { *sid_text.add(sid_len) } != 0 {
        sid_len += 1;
    }
    let mut text: Vec<u16> = "O:".encode_utf16().collect();
    // SAFETY: the SID string allocation remains live and sid_len excludes its terminator.
    text.extend_from_slice(unsafe { std::slice::from_raw_parts(sid_text, sid_len) });
    text.extend(dacl.encode_utf16());
    text.push(0);
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
fn private_descriptor() -> std::io::Result<Local> {
    // TokenOwner may be an administrator group, so specify TokenUser rather
    // than allowing object creation to choose the token's default owner.
    descriptor_with_dacl("D:P(A;;GA;;;OW)(A;;GA;;;SY)")
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
    let user = TokenUser::current()?;
    validate_private_directory_for_user(path, user.sid())
}

fn validate_private_directory_for_user(path: &Path, user: Handle) -> std::io::Result<()> {
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
    verify_user_owner(owner, user)?;
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

    #[link(name = "advapi32")]
    extern "system" {
        fn GetSecurityDescriptorOwner(
            descriptor: Handle,
            owner: *mut Handle,
            defaulted: *mut i32,
        ) -> i32;
    }

    fn descriptor_owner(descriptor: &Local) -> Handle {
        let mut owner = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: descriptor remains alive while the returned owner is used.
        assert_ne!(
            unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) },
            0
        );
        owner
    }

    fn unrelated_owner_descriptor() -> Local {
        let text: Vec<u16> = "O:S-1-0-0D:P(A;;GA;;;OW)(A;;GA;;;SY)\0"
            .encode_utf16()
            .collect();
        let mut unrelated = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut unrelated,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        Local(unrelated)
    }

    fn directory_security_descriptor(path: &Path) -> Local {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(path)
            .unwrap();
        let mut owner = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: the file and both output pointers are live during the query.
        assert_eq!(
            unsafe {
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
            },
            0
        );
        Local(descriptor)
    }

    fn directory_security_sddl(path: &Path) -> String {
        let descriptor = directory_security_descriptor(path);
        let mut text = std::ptr::null_mut();
        let mut len = 0;
        // SAFETY: the descriptor is live and the API allocates the output string.
        assert_ne!(
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor.0,
                    1,
                    1 | 4,
                    &mut text,
                    &mut len,
                )
            },
            0
        );
        let allocation = Local(text.cast());
        // SAFETY: the returned allocation contains len UTF-16 units.
        let result =
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len as usize) })
                .trim_end_matches('\0')
                .to_owned();
        drop(allocation);
        result
    }

    #[test]
    fn private_descriptor_explicitly_owns_resources_as_the_process_user() {
        let descriptor = private_descriptor().unwrap();
        let owner = descriptor_owner(&descriptor);
        let user = TokenUser::current().unwrap();
        assert!(!owner.is_null());
        verify_user_owner(owner, user.sid()).unwrap();

        let unrelated = unrelated_owner_descriptor();
        assert!(verify_user_owner(descriptor_owner(&unrelated), user.sid()).is_err());
        assert!(verify_user_owner(std::ptr::null_mut(), user.sid()).is_err());
    }

    #[test]
    fn private_supervisor_directory_is_created_once_and_validated() {
        let nonce = random_nonce().unwrap();
        assert_eq!(nonce.len(), 48);
        let directory = create_private_directory(&nonce).unwrap();
        validate_private_directory(&directory).unwrap();
        assert!(create_private_directory(&nonce).is_err());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn rooted_output_directory_validates_when_created_and_reopened() {
        use lingxi_core::host::rooted_fs;
        let parent = tempfile::tempdir().unwrap();
        let relative = Path::new("tasks");
        let directory = parent.path().join(relative);
        let created = rooted_fs::ensure_private_directory(
            parent.path(),
            relative,
            rooted_fs::PRIVATE_DIR_MODE,
        )
        .unwrap();
        validate_private_directory(&directory).unwrap();
        let before = directory_security_sddl(&directory);
        std::fs::write(directory.join("preserved"), "output").unwrap();
        rooted_fs::create_new_file_pinned(&directory, Path::new("task.output"), Some(&created))
            .unwrap();
        let output_security = directory_security_sddl(&directory.join("task.output"));
        assert_eq!(output_security, before);
        drop(
            rooted_fs::open_append_file_pinned(
                &directory,
                Path::new("task.output"),
                Some(&created),
            )
            .unwrap(),
        );
        assert_eq!(
            output_security,
            directory_security_sddl(&directory.join("task.output"))
        );

        let reopened = rooted_fs::ensure_private_directory(
            parent.path(),
            relative,
            rooted_fs::PRIVATE_DIR_MODE,
        )
        .unwrap();
        assert_eq!(created, reopened);
        assert_eq!(before, directory_security_sddl(&directory));
        assert_eq!(
            std::fs::read_to_string(directory.join("preserved")).unwrap(),
            "output"
        );
        validate_private_directory(&directory).unwrap();

        // Exercise the real directory/handle query with a different expected
        // user, without needing privileges to assign a foreign file owner.
        let unrelated = unrelated_owner_descriptor();
        let error = validate_private_directory_for_user(&directory, descriptor_owner(&unrelated))
            .unwrap_err();
        assert!(error.to_string().contains("owner differs"), "{error}");
    }

    #[test]
    fn rooted_output_append_handles_read_attributes_without_read_or_overwrite_access() {
        use lingxi_core::host::rooted_fs;
        use std::io::Write;

        #[repr(C)]
        struct IoStatusBlock {
            status_or_pointer: usize,
            information: usize,
        }
        #[link(name = "ntdll")]
        extern "system" {
            fn NtQueryInformationFile(
                handle: Handle,
                status: *mut IoStatusBlock,
                information: *mut c_void,
                length: u32,
                information_class: u32,
            ) -> i32;
        }
        fn access_mask(file: &std::fs::File) -> u32 {
            let mut io_status = IoStatusBlock {
                status_or_pointer: 0,
                information: 0,
            };
            let mut access = 0u32;
            // SAFETY: all pointers are live for this synchronous query;
            // FileAccessInformation is a single ACCESS_MASK.
            let status = unsafe {
                NtQueryInformationFile(
                    file.as_raw_handle(),
                    &mut io_status,
                    std::ptr::from_mut(&mut access).cast(),
                    std::mem::size_of::<u32>() as u32,
                    8,
                )
            };
            assert!(status >= 0, "FileAccessInformation NTSTATUS {status:#x}");
            access
        }

        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("tasks");
        let pin = rooted_fs::ensure_private_directory(
            parent.path(),
            Path::new("tasks"),
            rooted_fs::PRIVATE_DIR_MODE,
        )
        .unwrap();
        let leaf = Path::new("task.output");
        let mut created =
            rooted_fs::open_create_new_file_pinned(&directory, leaf, Some(&pin)).unwrap();
        assert!(created.metadata().unwrap().is_file());
        created.write_all(b"prefix").unwrap();
        drop(created);

        let mut first = rooted_fs::open_append_file_pinned(&directory, leaf, Some(&pin)).unwrap();
        let mut second = rooted_fs::open_append_file_pinned(&directory, leaf, Some(&pin)).unwrap();
        for file in [&first, &second] {
            assert!(file.metadata().unwrap().is_file());
            let access = access_mask(file);
            assert_ne!(access & 0x80, 0, "FILE_READ_ATTRIBUTES: {access:#x}");
            assert_ne!(access & 0x04, 0, "FILE_APPEND_DATA: {access:#x}");
            assert_eq!(
                access & 0x03,
                0,
                "FILE_READ_DATA/FILE_WRITE_DATA: {access:#x}"
            );
        }
        first.write_all(b"-first").unwrap();
        second.write_all(b"-second").unwrap();
        first.write_all(b"-third").unwrap();
        drop((first, second));
        rooted_fs::append_file_pinned(&directory, leaf, "-fourth", Some(&pin)).unwrap();
        assert_eq!(
            std::fs::read(directory.join(leaf)).unwrap(),
            b"prefix-first-second-third-fourth"
        );
        assert_eq!(
            directory_security_sddl(&directory.join(leaf)),
            directory_security_sddl(&directory)
        );
    }

    #[test]
    fn existing_directory_keeps_default_owner_and_rejects_it_when_different() {
        use lingxi_core::host::rooted_fs;
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("tasks");
        // This is the old descriptor shape: Windows chooses TokenOwner when
        // no owner is specified. It can differ from TokenUser on an elevated
        // runner, without requiring this test to acquire extra privileges.
        let text: Vec<u16> = "D:P(A;;GA;;;OW)(A;;GA;;;SY)\0".encode_utf16().collect();
        let mut descriptor = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let descriptor = Local(descriptor);
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: descriptor.0,
            inherit: 0,
        };
        let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the descriptor, attributes, and path remain live during creation.
        assert_ne!(
            unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) },
            0
        );
        let before = directory_security_sddl(&path);
        let directory_descriptor = directory_security_descriptor(&path);
        let owner = descriptor_owner(&directory_descriptor);
        let user = TokenUser::current().unwrap();
        assert!(!owner.is_null());
        // SAFETY: both OS-provided SIDs remain live during the comparison.
        let owner_matches_user = unsafe { EqualSid(owner, user.sid()) } != 0;
        rooted_fs::ensure_private_directory(
            parent.path(),
            Path::new("tasks"),
            rooted_fs::PRIVATE_DIR_MODE,
        )
        .unwrap();
        assert_eq!(before, directory_security_sddl(&path));
        let validated = validate_private_directory(&path);
        if owner_matches_user {
            validated.unwrap();
        } else {
            let error = validated.unwrap_err();
            assert!(error.to_string().contains("owner differs"), "{error}");
        }
    }

    #[test]
    fn supervisor_directory_with_an_extra_public_ace_is_rejected() {
        use lingxi_core::host::rooted_fs;
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("tasks");
        let descriptor = descriptor_with_dacl("D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GR;;;WD)").unwrap();
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            descriptor: descriptor.0,
            inherit: 0,
        };
        let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the descriptor, attributes, and path remain live during creation.
        assert_ne!(
            unsafe { CreateDirectoryW(path_wide.as_ptr(), &attributes) },
            0
        );
        std::fs::write(path.join("preserved"), "output").unwrap();
        let before = directory_security_sddl(&path);
        rooted_fs::ensure_private_directory(
            parent.path(),
            Path::new("tasks"),
            rooted_fs::PRIVATE_DIR_MODE,
        )
        .unwrap();
        assert_eq!(before, directory_security_sddl(&path));
        assert_eq!(
            std::fs::read_to_string(path.join("preserved")).unwrap(),
            "output"
        );
        let error = validate_private_directory(&path).unwrap_err();
        assert!(error.to_string().contains("DACL is not private"), "{error}");
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

/// Retain a process object until the background cleanup test drops the handle.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn hold_process_handle_for_test(
    pid: u32,
) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};

    // SAFETY: OpenProcess takes only scalar arguments and returns an owned,
    // non-inheritable handle on success. SYNCHRONIZE suffices for the test.
    let handle = unsafe { OpenProcess(0x0010_0000, 0, pid) };
    if handle.is_null() {
        return Err(last_error());
    }
    // SAFETY: a successful OpenProcess returned a unique handle which is now
    // transferred to OwnedHandle and closed exactly once when it is dropped.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// Query the same process object retained by the test, even if its PID is reused.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn process_handle_is_alive_for_test(
    handle: &std::os::windows::io::OwnedHandle,
) -> Option<bool> {
    // SAFETY: the borrowed process handle remains open during this query.
    match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
        0 => Some(false),
        258 => Some(true),
        _ => None,
    }
}
