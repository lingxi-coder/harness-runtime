//! System directories obtained from the Windows API, independent of inherited
//! environment variables. Callers must separately secure any product paths.
#![allow(unsafe_code)]

use std::{ffi::OsString, io, os::windows::ffi::OsStringExt, path::PathBuf};
use windows_sys::Win32::System::SystemInformation::{
    GetSystemDirectoryW, GetSystemWindowsDirectoryW,
};

/// Windows directories returned by the operating system, not by process env.
#[derive(Debug, Clone)]
pub struct SystemPaths {
    /// The shared Windows directory for use as SystemRoot and WINDIR.
    pub windows_dir: PathBuf,
    /// The operating-system library directory, normally Windows/System32.
    pub system_dir: PathBuf,
}

fn query(api: unsafe extern "system" fn(*mut u16, u32) -> u32) -> io::Result<PathBuf> {
    // Win32 paths are bounded to 32767 UTF-16 code units, including the NUL.
    const LIMIT: usize = 32768;
    let mut buffer = vec![0u16; 260];
    loop {
        // SAFETY: buffer is initialized and writable for exactly its reported
        // length; both selected Win32 APIs synchronously write at most that.
        let length = unsafe { api(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length >= buffer.len() {
            let capacity = length
                .checked_add(1)
                .filter(|capacity| *capacity <= LIMIT)
                .ok_or_else(|| io::Error::other("Windows system path is too long"))?;
            buffer.resize(capacity, 0);
            continue;
        }
        let mut wide = buffer[..length].to_vec();
        if wide.contains(&0) {
            return Err(io::Error::other("Windows system path contains NUL"));
        }
        // A root installation can be reported as a drive designator. Make it
        // an absolute path without assuming that Windows is installed on C:.
        if wide.len() == 2
            && wide[1] == b':' as u16
            && ((b'A' as u16..=b'Z' as u16).contains(&wide[0])
                || (b'a' as u16..=b'z' as u16).contains(&wide[0]))
        {
            wide.push(b'\\' as u16);
        }
        let path = PathBuf::from(OsString::from_wide(&wide));
        if !path.is_absolute() {
            return Err(io::Error::other("Windows system path is not absolute"));
        }
        return Ok(path);
    }
}

/// Query the system Windows and library directories through Win32 APIs.
pub fn system_paths() -> io::Result<SystemPaths> {
    Ok(SystemPaths {
        windows_dir: query(GetSystemWindowsDirectoryW)?,
        system_dir: query(GetSystemDirectoryW)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    unsafe extern "system" fn long_directory(buffer: *mut u16, capacity: u32) -> u32 {
        let mut path = vec![b'X' as u16, b':' as u16, b'\\' as u16];
        path.extend(std::iter::repeat_n(b'p' as u16, 300));
        if capacity as usize <= path.len() {
            return (path.len() + 1) as u32;
        }
        // SAFETY: the fake API writes only after checking capacity, matching
        // the contract exercised by the production wrapper.
        unsafe {
            std::ptr::copy_nonoverlapping(path.as_ptr(), buffer, path.len());
            *buffer.add(path.len()) = 0;
        }
        path.len() as u32
    }

    unsafe extern "system" fn drive_root(buffer: *mut u16, capacity: u32) -> u32 {
        if capacity < 3 {
            return 3;
        }
        // SAFETY: this branch requires room for two code units and a NUL.
        unsafe {
            *buffer = b'X' as u16;
            *buffer.add(1) = b':' as u16;
            *buffer.add(2) = 0;
        }
        2
    }

    #[test]
    fn grows_buffer_for_long_paths_and_preserves_drive_roots() {
        let long = query(long_directory).unwrap();
        assert!(long.is_absolute());
        assert_eq!(long.as_os_str().encode_wide().count(), 303);
        assert_eq!(query(drive_root).unwrap(), PathBuf::from(r"X:\"));
    }

    #[test]
    fn real_system_paths_are_absolute_directories() {
        let paths = system_paths().unwrap();
        assert!(paths.windows_dir.is_absolute() && paths.windows_dir.is_dir());
        assert!(paths.system_dir.is_absolute() && paths.system_dir.is_dir());
    }
}
