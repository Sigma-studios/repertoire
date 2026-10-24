//! File handles: the stable identity fanotify uses to name directories.
//!
//! A handle is kept as the bytes of `struct file_handle` (handle_bytes,
//! handle_type, f_handle), exactly as fanotify reports it after the fsid, so
//! the scanner's handles and the events' handles compare equal.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

const MAX_HANDLE_SZ: usize = 128;

/// Handle of `name` inside `dir`, or of `dir` itself when `name` is empty.
/// Doesn't need privileges. Matches `repertoire_core::scan::HandleFn`.
pub fn handle_at(dir: BorrowedFd, name: &[u8]) -> Option<Box<[u8]>> {
    let c = std::ffi::CString::new(name).ok()?;
    // u32-aligned buffer for struct file_handle.
    let mut buf = [0u32; 2 + MAX_HANDLE_SZ / 4];
    buf[0] = MAX_HANDLE_SZ as u32;
    let mut mount_id: libc::c_int = 0;
    let flags = if name.is_empty() { libc::AT_EMPTY_PATH } else { 0 };
    // SAFETY: buf is a file_handle with handle_bytes = MAX_HANDLE_SZ and room
    // for that many bytes after the 8-byte header; other pointers are valid.
    let r = unsafe {
        libc::syscall(libc::SYS_name_to_handle_at, dir.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr(), &mut mount_id, flags)
    };
    if r != 0 {
        return None;
    }
    let n = buf[0] as usize;
    // SAFETY: the kernel filled 8 + n bytes of buf, which is 4-byte aligned
    // and at least that long.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, 8 + n) };
    Some(bytes.into())
}

/// `open_by_handle_at`. Needs `CAP_DAC_READ_SEARCH`. `mount` is any open
/// file on the same filesystem.
pub fn open_by_handle(mount: BorrowedFd, handle: &[u8], flags: libc::c_int) -> io::Result<OwnedFd> {
    if handle.len() < 8 || handle.len() > 8 + MAX_HANDLE_SZ {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut buf = [0u32; 2 + MAX_HANDLE_SZ / 4];
    // SAFETY: buf has room for handle.len() bytes (checked above).
    unsafe { std::ptr::copy_nonoverlapping(handle.as_ptr(), buf.as_mut_ptr() as *mut u8, handle.len()) };
    // SAFETY: buf holds a well-formed file_handle copied from the kernel.
    let fd = unsafe { libc::syscall(libc::SYS_open_by_handle_at, mount.as_raw_fd(), buf.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the syscall returned a new fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// The path an open fd refers to now, from `/proc/self/fd`. `None` once
/// it was deleted.
pub fn fd_path(fd: BorrowedFd) -> Option<PathBuf> {
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
    if link.as_os_str().as_bytes().ends_with(b" (deleted)") || !link.is_absolute() {
        return None;
    }
    Some(link)
}

/// The filesystem id fanotify reports with every event (`statfs.f_fsid`).
pub fn fsid(fd: BorrowedFd) -> io::Result<[u8; 8]> {
    // SAFETY: zeroed statfs is a valid out-parameter; fd is live.
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::fstatfs(fd.as_raw_fd(), &mut s) != 0 {
            return Err(io::Error::last_os_error());
        }
        // fsid_t's fields are private in libc; it's two ints.
        Ok(std::mem::transmute::<libc::fsid_t, [u8; 8]>(s.f_fsid))
    }
}
