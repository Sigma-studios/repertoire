//! Thin wrappers over the POSIX calls the scanner and the event applier use.
//!
//! Everything works relative to an open directory (`openat`, `fstatat`) so a
//! walk is cheap and isn't confused by renames happening while it runs.

use crate::tree::Kind;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The parts of `struct stat` the index keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    /// Apparent size (`st_size`).
    pub size: u64,
    /// Allocated size (`st_blocks * 512`): what the entry really costs.
    pub bytes: u64,
    pub mtime: i64,
}

// libc's field and constant types differ between Linux and macOS, so the
// casts below are no-ops on one of them.
#[allow(clippy::unnecessary_cast)]
impl Stat {
    pub fn kind(&self) -> Kind {
        match self.mode & libc::S_IFMT as u32 {
            m if m == libc::S_IFDIR as u32 => Kind::Dir,
            m if m == libc::S_IFREG as u32 => Kind::File,
            m if m == libc::S_IFLNK as u32 => Kind::Symlink,
            _ => Kind::Other,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind() == Kind::Dir
    }

    fn from_raw(s: &libc::stat) -> Stat {
        Stat {
            dev: s.st_dev as u64,
            ino: s.st_ino as u64,
            mode: s.st_mode as u32,
            nlink: s.st_nlink as u64,
            uid: s.st_uid,
            gid: s.st_gid,
            size: s.st_size.max(0) as u64,
            bytes: (s.st_blocks.max(0) as u64) * 512,
            mtime: s.st_mtime as i64,
        }
    }
}

pub fn cstring(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Open a directory by path (follows symlinks: used for mount points and
/// user-given roots).
pub fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = cstring(path.as_os_str().as_bytes())?;
    // SAFETY: `c` is a valid NUL-terminated string; the returned fd is owned.
    let fd = check(unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) })?;
    // SAFETY: `fd` was just returned by open and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the directory `name` inside `dir`, never following a symlink.
pub fn open_dir_at(dir: BorrowedFd, name: &[u8]) -> io::Result<OwnedFd> {
    let c = cstring(name)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is a live fd and `c` a valid C string; the result is owned.
    let fd = check(unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) })?;
    // SAFETY: `fd` was just returned by openat and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `lstat` of `name` inside `dir`.
pub fn stat_at(dir: BorrowedFd, name: &[u8]) -> io::Result<Stat> {
    let c = cstring(name)?;
    // SAFETY: zeroed `stat` is a valid out-parameter; `dir` and `c` are valid.
    unsafe {
        let mut s: libc::stat = std::mem::zeroed();
        check(libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut s, libc::AT_SYMLINK_NOFOLLOW))?;
        Ok(Stat::from_raw(&s))
    }
}

pub fn stat_fd(fd: BorrowedFd) -> io::Result<Stat> {
    // SAFETY: zeroed `stat` is a valid out-parameter and `fd` is live.
    unsafe {
        let mut s: libc::stat = std::mem::zeroed();
        check(libc::fstat(fd.as_raw_fd(), &mut s))?;
        Ok(Stat::from_raw(&s))
    }
}

pub fn lstat(path: &Path) -> io::Result<Stat> {
    let c = cstring(path.as_os_str().as_bytes())?;
    // SAFETY: zeroed `stat` is a valid out-parameter; `c` is a valid C string.
    unsafe {
        let mut s: libc::stat = std::mem::zeroed();
        check(libc::lstat(c.as_ptr(), &mut s))?;
        Ok(Stat::from_raw(&s))
    }
}

/// Entry names of an open directory, without `.` and `..`.
pub fn read_dir(dir: BorrowedFd) -> io::Result<Vec<Box<[u8]>>> {
    // fdopendir takes ownership of the fd it is given, so hand it a duplicate.
    // SAFETY: `dir` is a live fd.
    let dup = check(unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: `dup` is ours; on success the DIR stream owns it, on failure we close it.
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        return Err(e);
    }
    let mut out = Vec::new();
    let mut result = Ok(());
    loop {
        // readdir signals errors only through errno, so clear it first.
        set_errno(0);
        // SAFETY: `stream` is a valid DIR* until closedir below.
        let ent = unsafe { libc::readdir(stream) };
        if ent.is_null() {
            let e = io::Error::last_os_error();
            if e.raw_os_error().unwrap_or(0) != 0 {
                result = Err(e);
            }
            break;
        }
        // SAFETY: d_name is NUL-terminated inside the dirent readdir returned.
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            out.push(name.into());
        }
    }
    // SAFETY: `stream` came from fdopendir and is closed exactly once.
    unsafe { libc::closedir(stream) };
    result.map(|()| out)
}

fn set_errno(v: libc::c_int) {
    // SAFETY: the errno location is thread-local and always valid.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            *libc::__errno_location() = v;
        }
        #[cfg(target_os = "macos")]
        {
            *libc::__error() = v;
        }
    }
}

/// (total, used) bytes as `df` shows them.
pub fn space(path: &Path) -> Option<(u64, u64)> {
    let c = cstring(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed statvfs is a valid out-parameter; `c` is a valid C string.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        let bs = s.f_frsize as u64;
        let total = s.f_blocks as u64 * bs;
        // Used as `df` shows it: excludes root-reserved blocks from free.
        let used = (s.f_blocks as u64).saturating_sub(s.f_bfree as u64) * bs;
        Some((total, used))
    }
}

/// Raise the soft open-file limit to the hard limit. A parallel walk keeps
/// one directory open per level per thread, which can pass macOS' default 256.
pub fn raise_fd_limit() {
    // SAFETY: plain get/setrlimit on a zeroed, then filled, struct.
    unsafe {
        let mut r: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) == 0 && r.rlim_cur < r.rlim_max {
            // macOS rejects RLIM_INFINITY here; OPEN_MAX-ish values work.
            r.rlim_cur = r.rlim_max.min(1 << 16);
            libc::setrlimit(libc::RLIMIT_NOFILE, &r);
        }
    }
}

pub fn name_of(path: &Path) -> &[u8] {
    path.file_name().map(OsStr::as_bytes).unwrap_or(path.as_os_str().as_bytes())
}

pub fn euid() -> u32 {
    // SAFETY: geteuid has no failure modes.
    unsafe { libc::geteuid() }
}
