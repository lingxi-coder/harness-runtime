//! Owner-only file creation used by background-session control state.

use std::fs::File;
use std::io;
use std::path::Path;

/// Create a new regular file that is accessible only to the current user (and
/// the local system account on Windows).
///
/// Creation is exclusive on every platform. On Unix the mode is supplied to
/// `open(2)` atomically; on Windows a protected DACL is supplied directly to
/// `CreateFileW`, avoiding an inherited-ACL or post-creation race.
pub fn create_current_user_private_file(path: &Path) -> io::Result<File> {
    create_private_file(path)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn create_private_file(path: &Path) -> io::Result<File> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use winapi::shared::minwindef::DWORD;
    use winapi::shared::sddl::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use winapi::um::fileapi::{CreateFileW, CREATE_NEW};
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
    use winapi::um::winbase::LocalFree;
    use winapi::um::winnt::{FILE_ATTRIBUTE_NORMAL, GENERIC_WRITE, HANDLE, PSECURITY_DESCRIPTOR};

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private file path contains an interior NUL",
        ));
    }
    wide.push(0);

    // Protected DACL: full access for the object owner and LocalSystem only.
    let sddl: Vec<u16> = "D:P(A;;GA;;;OW)(A;;GA;;;SY)\0".encode_utf16().collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            DWORD::from(SDDL_REVISION_1),
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }
    let descriptor = LocalSecurityDescriptor(descriptor);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: DWORD::try_from(size_of::<SECURITY_ATTRIBUTES>())
            .map_err(|_| io::Error::other("security attributes size overflow"))?,
        lpSecurityDescriptor: descriptor.0.cast(),
        bInheritHandle: 0,
    };
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::from_mut(&mut attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut::<std::ffi::c_void>() as HANDLE,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}

#[cfg(not(any(unix, windows)))]
fn create_private_file(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.json");
        drop(create_current_user_private_file(&path).unwrap());
        assert_eq!(
            create_current_user_private_file(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
