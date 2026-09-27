use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::slice;

use winapi::shared::minwindef::{BOOL, DWORD};
use winapi::shared::winerror::ERROR_NO_MORE_FILES;
use winapi::um::fileapi::{
    GetFileInformationByHandle, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    FILE_DISPOSITION_INFO, FILE_ID_BOTH_DIR_INFO,
};
use winapi::um::minwinbase::{
    FileDispositionInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
};
use winapi::um::winbase::{
    FileIdType, GetFileInformationByHandleEx, OpenFileById, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR,
};
use winapi::um::winnt::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_LIST_DIRECTORY,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ, HANDLE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsFileIdentity {
    pub volume_serial_number: u32,
    pub file_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsDirectoryEntry {
    pub file_id: i64,
    pub is_directory: bool,
}

pub fn open_reparse_guarded(path: &Path, delete_access: bool) -> io::Result<File> {
    let mut access = GENERIC_READ | FILE_LIST_DIRECTORY;
    if delete_access {
        access |= DELETE;
    }
    let file = std::fs::OpenOptions::new()
        .access_mode(access)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let identity = file_identity(&file)?;
    let _ = identity;
    Ok(file)
}

pub fn enumerate_directory_by_handle(dir: &File) -> io::Result<Vec<WindowsDirectoryEntry>> {
    let mut entries = Vec::new();
    let mut restart = true;
    loop {
        let mut buf = vec![0u8; 64 * 1024];
        let class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.as_raw_handle() as HANDLE,
                class,
                buf.as_mut_ptr().cast(),
                DWORD::try_from(buf.len()).unwrap_or(u32::MAX),
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(err);
        }
        restart = false;
        let mut offset = 0usize;
        loop {
            let entry = unsafe { &*(buf.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>()) };
            let name_len = usize::try_from(entry.FileNameLength / 2).unwrap_or(0);
            let name_ptr = entry.FileName.as_ptr();
            let name = unsafe { slice::from_raw_parts(name_ptr, name_len) };
            if name != ['.' as u16] && name != ['.' as u16, '.' as u16] {
                entries.push(WindowsDirectoryEntry {
                    file_id: unsafe { *entry.FileId.QuadPart() },
                    is_directory: entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                });
            }
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += usize::try_from(entry.NextEntryOffset).unwrap_or(0);
        }
    }
    Ok(entries)
}

pub fn open_child_by_id(parent: &File, file_id: i64, delete_access: bool) -> io::Result<File> {
    let mut descriptor: FILE_ID_DESCRIPTOR = unsafe { std::mem::zeroed() };
    descriptor.dwSize = DWORD::try_from(size_of::<FILE_ID_DESCRIPTOR>()).unwrap_or(u32::MAX);
    descriptor.Type = FileIdType;
    unsafe {
        *descriptor.u.FileId_mut() = std::mem::zeroed();
        *descriptor.u.FileId_mut().QuadPart_mut() = file_id;
    }
    let mut access = GENERIC_READ | FILE_LIST_DIRECTORY;
    if delete_access {
        access |= DELETE;
    }
    let handle = unsafe {
        OpenFileById(
            parent.as_raw_handle() as HANDLE,
            &mut descriptor,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null_mut(),
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if handle.is_null() || handle == (-1isize) as HANDLE {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_handle(handle.cast()) };
    let _ = file_identity(&file)?;
    Ok(file)
}

pub fn file_identity(file: &File) -> io::Result<WindowsFileIdentity> {
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    let ok: BOOL = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other("managed worktree path is a reparse point"));
    }
    Ok(WindowsFileIdentity {
        volume_serial_number: info.dwVolumeSerialNumber,
        file_index: ((u64::from(info.nFileIndexHigh)) << 32) | u64::from(info.nFileIndexLow),
    })
}

pub fn delete_by_handle(file: &File) -> io::Result<()> {
    let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: 1 };
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfo,
            (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
            DWORD::try_from(size_of::<FILE_DISPOSITION_INFO>()).unwrap_or(u32::MAX),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
