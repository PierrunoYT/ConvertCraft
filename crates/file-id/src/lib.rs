//! File identity details that std does not expose: the Windows 128-bit file ID
//! (`FILE_ID_INFO`) and the Linux inode generation. It lives in its own crate so
//! ConvertCraft itself can forbid unsafe code.
//!
//! The 64-bit `nFileIndex` from `GetFileInformationByHandle` is not unique on ReFS,
//! which has 128-bit IDs; `FileIdInfo` is the documented identity on every file system.
#![cfg(any(windows, target_os = "linux"))]

use std::io;
#[cfg(windows)]
use std::mem::{MaybeUninit, size_of};
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx,
};

/// Volume serial number and file ID of an open file or directory.
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FileId {
    pub volume: u64,
    pub id: u128,
}

/// Fails where the file system or network redirector does not support `FileIdInfo`.
#[cfg(windows)]
pub fn file_id(handle: &impl AsRawHandle) -> io::Result<FileId> {
    let mut info = MaybeUninit::<FILE_ID_INFO>::uninit();
    // SAFETY: the borrowed handle stays open for the call, and the buffer is a
    // writable FILE_ID_INFO of exactly the size passed.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileIdInfo,
            info.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful call filled the whole structure.
    let info = unsafe { info.assume_init() };
    Ok(FileId {
        volume: info.VolumeSerialNumber,
        id: u128::from_le_bytes(info.FileId.Identifier),
    })
}

/// Inode generation (`FS_IOC_GETVERSION`) of an open file or directory, as an opaque
/// value. File systems such as ext4 hand a deleted file's inode number to the next new
/// file, often within the same timestamp tick; the generation differs, much like the
/// sequence number in an NTFS file ID. Fails where the file system does not support it.
#[cfg(target_os = "linux")]
pub fn generation(file: &impl std::os::fd::AsFd) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    let mut generation: libc::c_long = 0;
    // SAFETY: the borrowed descriptor stays open for the call, and the kernel writes
    // at most the c_long this request is declared with.
    let result = unsafe {
        libc::ioctl(
            file.as_fd().as_raw_fd(),
            libc::FS_IOC_GETVERSION,
            &mut generation,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(generation as u64)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::fs::{self, File};

    #[test]
    fn hard_links_share_an_id_and_distinct_files_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        fs::hard_link(&a, dir.path().join("link")).unwrap();
        let id = |name: &str| file_id(&File::open(dir.path().join(name)).unwrap()).unwrap();
        assert_eq!(id("a"), id("link"));
        assert_ne!(id("a"), id("b"));
        assert_ne!(id("a").id, 0);
    }
}
