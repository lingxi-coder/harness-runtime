use std::io;
use std::mem::size_of;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use winapi::shared::minwindef::DWORD;
use winapi::shared::sddl::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
use winapi::um::winbase::LocalFree;
use winapi::um::winnt::PSECURITY_DESCRIPTOR;

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0.cast());
        }
    }
}

/// Create a named-pipe server whose protected DACL grants full access only to
/// the object owner (the current user token) and LocalSystem. The security
/// descriptor is supplied to `CreateNamedPipeW` atomically, so there is no
/// post-creation ACL race.
pub fn create_current_user_named_pipe(
    options: &ServerOptions,
    name: &str,
) -> io::Result<NamedPipeServer> {
    // P = protected DACL (do not inherit permissive parent ACEs); OW = owner
    // rights, whose owner is the current process token for a new kernel object.
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
    unsafe {
        options
            .create_with_security_attributes_raw(name, std::ptr::from_mut(&mut attributes).cast())
    }
}
