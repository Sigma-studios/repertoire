//! fanotify in "report fid" mode: one filesystem-wide mark per indexed
//! filesystem, events that name the parent directory by file handle plus
//! the entry name, and renames as one event with both ends.
//!
//! Raw kernel records are turned into [`RawEvent`]s here; resolving the
//! handles and re-stat'ing is `repertoire-core`'s job.

use repertoire_core::RawEvent;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// From <linux/fanotify.h>; not all are in the libc crate.
pub const FAN_MODIFY: u64 = 0x2;
pub const FAN_ATTRIB: u64 = 0x4;
pub const FAN_CLOSE_WRITE: u64 = 0x8;
pub const FAN_CREATE: u64 = 0x100;
pub const FAN_DELETE: u64 = 0x200;
pub const FAN_DELETE_SELF: u64 = 0x400;
pub const FAN_MOVE_SELF: u64 = 0x800;
pub const FAN_Q_OVERFLOW: u64 = 0x4000;
pub const FAN_RENAME: u64 = 0x1000_0000;
pub const FAN_EVENT_ON_CHILD: u64 = 0x0800_0000;
pub const FAN_ONDIR: u64 = 0x4000_0000;

const FAN_CLOEXEC: u32 = 0x1;
const FAN_CLASS_NOTIF: u32 = 0x0;
const FAN_UNLIMITED_QUEUE: u32 = 0x10;
const FAN_REPORT_FID: u32 = 0x200;
const FAN_REPORT_DIR_FID: u32 = 0x400;
const FAN_REPORT_NAME: u32 = 0x800;
const FAN_REPORT_TARGET_FID: u32 = 0x1000;
pub const FAN_REPORT_DFID_NAME_TARGET: u32 = FAN_REPORT_FID | FAN_REPORT_DIR_FID | FAN_REPORT_NAME | FAN_REPORT_TARGET_FID;

const FAN_MARK_ADD: u32 = 0x1;
const FAN_MARK_IGNORED_SURV_MODIFY: u32 = 0x40;
const FAN_MARK_FILESYSTEM: u32 = 0x100;
const FAN_MARK_IGNORE: u32 = 0x400;

const INFO_FID: u8 = 1;
const INFO_DFID_NAME: u8 = 2;
const INFO_DFID: u8 = 3;
const INFO_OLD_DFID_NAME: u8 = 10;
const INFO_NEW_DFID_NAME: u8 = 12;

const METADATA_VERSION: u8 = 3;

/// What every filesystem mark asks for.
pub const MASK: u64 = FAN_CREATE | FAN_DELETE | FAN_RENAME | FAN_MODIFY | FAN_CLOSE_WRITE | FAN_ATTRIB | FAN_ONDIR;

pub struct Fanotify {
    fd: OwnedFd,
}

impl Fanotify {
    /// Needs `CAP_SYS_ADMIN`. `unlimited_queue` is off only in tests that
    /// force an overflow.
    pub fn new(unlimited_queue: bool) -> io::Result<Fanotify> {
        let mut flags = FAN_CLOEXEC | FAN_CLASS_NOTIF | FAN_REPORT_DFID_NAME_TARGET;
        if unlimited_queue {
            flags |= FAN_UNLIMITED_QUEUE;
        }
        // SAFETY: plain syscall; the result is a new fd or -1.
        let fd = unsafe { libc::fanotify_init(flags, (libc::O_RDONLY | libc::O_LARGEFILE) as u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fanotify_init returned a fresh fd nothing else owns.
        Ok(Fanotify { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    fn mark(&self, flags: u32, mask: u64, path: &Path) -> io::Result<()> {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: valid fd and C string.
        let r = unsafe { libc::fanotify_mark(self.fd.as_raw_fd(), flags, mask, libc::AT_FDCWD, c.as_ptr()) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Watch the whole filesystem `path` is on.
    pub fn mark_filesystem(&self, path: &Path) -> io::Result<()> {
        self.mark(FAN_MARK_ADD | FAN_MARK_FILESYSTEM, MASK, path)
    }

    /// Ignore everything that happens in one directory (our own state
    /// directory). `FAN_MARK_IGNORE` needs Linux 6.0.
    pub fn ignore_dir(&self, path: &Path) -> io::Result<()> {
        self.mark(FAN_MARK_ADD | FAN_MARK_IGNORE | FAN_MARK_IGNORED_SURV_MODIFY, MASK | FAN_EVENT_ON_CHILD, path)
    }

    /// Blocking read of a batch of events.
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: buf is valid for writes of its length.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// One fid record: which filesystem, which object, and for directory
/// records the entry name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fid<'a> {
    pub fsid: [u8; 8],
    /// `struct file_handle` bytes.
    pub handle: &'a [u8],
    pub name: Option<&'a [u8]>,
}

#[derive(Debug, Default)]
pub struct KernelEvent<'a> {
    pub mask: u64,
    /// The object itself (`FAN_REPORT_FID` / `TARGET_FID`).
    pub fid: Option<Fid<'a>>,
    /// Parent directory and name, or the directory itself.
    pub dfid: Option<Fid<'a>>,
    pub old: Option<Fid<'a>>,
    pub new: Option<Fid<'a>>,
}

/// Split a buffer read from the fanotify fd into events. Malformed tails
/// are dropped; a version mismatch is an error.
pub fn parse(buf: &[u8], mut f: impl FnMut(KernelEvent)) -> io::Result<()> {
    let mut at = 0;
    while at + 24 <= buf.len() {
        let len = u32::from_ne_bytes(buf[at..at + 4].try_into().unwrap()) as usize;
        if buf[at + 4] != METADATA_VERSION {
            return Err(io::Error::other(format!("fanotify metadata version {}", buf[at + 4])));
        }
        let meta_len = u16::from_ne_bytes(buf[at + 6..at + 8].try_into().unwrap()) as usize;
        if len < meta_len || at + len > buf.len() || meta_len < 24 {
            break;
        }
        let mask = u64::from_ne_bytes(buf[at + 8..at + 16].try_into().unwrap());
        let fd = i32::from_ne_bytes(buf[at + 16..at + 20].try_into().unwrap());
        if fd >= 0 {
            // Not expected in fid mode, but never leak one.
            // SAFETY: the kernel handed us this fd; we own it.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let mut ev = KernelEvent { mask, ..Default::default() };
        let mut info = &buf[at + meta_len..at + len];
        while info.len() >= 4 {
            let (kind, rlen) = (info[0], u16::from_ne_bytes([info[2], info[3]]) as usize);
            if rlen < 4 || rlen > info.len() {
                break;
            }
            let rec = &info[..rlen];
            if let Some(fid) = parse_fid(rec, matches!(kind, INFO_DFID_NAME | INFO_OLD_DFID_NAME | INFO_NEW_DFID_NAME)) {
                match kind {
                    INFO_FID => ev.fid = Some(fid),
                    INFO_DFID_NAME | INFO_DFID => ev.dfid = Some(fid),
                    INFO_OLD_DFID_NAME => ev.old = Some(fid),
                    INFO_NEW_DFID_NAME => ev.new = Some(fid),
                    _ => {}
                }
            }
            info = &info[rlen..];
        }
        f(ev);
        at += len;
    }
    Ok(())
}

fn parse_fid(rec: &[u8], named: bool) -> Option<Fid<'_>> {
    // header (4) + fsid (8) + file_handle header (8) + f_handle
    let fsid: [u8; 8] = rec.get(4..12)?.try_into().ok()?;
    let hbytes = u32::from_ne_bytes(rec.get(12..16)?.try_into().ok()?) as usize;
    let end = 20 + hbytes;
    let handle = rec.get(12..end)?;
    let name = if named {
        let rest = rec.get(end..)?;
        let n = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        Some(&rest[..n])
    } else {
        None
    };
    Some(Fid { fsid, handle, name })
}

/// What an event means for the index, per filesystem id. Overflow has no
/// filesystem: events were lost everywhere.
pub fn translate(ev: &KernelEvent, out: &mut Vec<(Option<[u8; 8]>, RawEvent)>) {
    let b = |s: &[u8]| -> Box<[u8]> { s.into() };
    if ev.mask & FAN_Q_OVERFLOW != 0 {
        out.push((None, RawEvent::Overflow));
        return;
    }
    let obj = ev.fid.map(|f| b(f.handle));
    if ev.mask & FAN_RENAME != 0 {
        if let (Some(o), Some(n)) = (ev.old, ev.new) {
            out.push((
                Some(o.fsid),
                RawEvent::Renamed {
                    from: b(o.handle),
                    from_name: b(o.name.unwrap_or_default()),
                    to: b(n.handle),
                    to_name: b(n.name.unwrap_or_default()),
                },
            ));
        }
    }
    match (ev.dfid, ev.fid) {
        (Some(d), _) if d.name.is_some_and(|n| !n.is_empty() && n != b".") => {
            let (dir, name, fs) = (b(d.handle), b(d.name.unwrap()), Some(d.fsid));
            if ev.mask & FAN_CREATE != 0 {
                out.push((fs, RawEvent::Created { dir: dir.clone(), name: name.clone() }));
            }
            if ev.mask & FAN_DELETE != 0 {
                out.push((fs, RawEvent::Removed { dir: dir.clone(), name: name.clone() }));
            }
            if ev.mask & FAN_MODIFY != 0 {
                out.push((fs, RawEvent::Modified { dir: dir.clone(), name: name.clone(), obj: obj.clone() }));
            }
            if ev.mask & (FAN_ATTRIB | FAN_CLOSE_WRITE) != 0 {
                out.push((fs, RawEvent::Attrib { dir, name, obj }));
            }
        }
        // An event on a directory itself (a mount root has no parent).
        (Some(d), _) if ev.mask & (FAN_ATTRIB | FAN_MODIFY | FAN_CLOSE_WRITE | FAN_DELETE_SELF | FAN_MOVE_SELF) != 0 => {
            out.push((Some(d.fsid), RawEvent::DirChanged { dir: b(d.handle) }));
        }
        // Only the object: a link count or attribute change on an inode
        // the kernel reports without a parent.
        (None, Some(f)) if ev.mask & FAN_RENAME == 0 => {
            if ev.mask & FAN_ONDIR != 0 {
                out.push((Some(f.fsid), RawEvent::DirChanged { dir: b(f.handle) }));
            } else {
                out.push((Some(f.fsid), RawEvent::Inode { obj: b(f.handle) }));
            }
        }
        _ => {}
    }
}
