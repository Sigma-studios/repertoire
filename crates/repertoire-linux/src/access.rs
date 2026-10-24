//! Reaching tree directories through their file handles, so an event
//! applied late still finds a directory that was renamed since. Needs
//! `CAP_DAC_READ_SEARCH`; without it, falls back to paths.

use crate::handle::{fd_path, open_by_handle};
use repertoire_core::sys::{self, Stat};
use repertoire_core::{ByPath, DirAccess, Id, Tree};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

pub struct HandleAccess {
    /// Any open directory on the filesystem (its first mount point).
    pub mount: OwnedFd,
    /// Whether `open_by_handle_at` works for us.
    pub privileged: bool,
}

impl HandleAccess {
    pub fn new(mount: OwnedFd) -> HandleAccess {
        // Probe with the mount root's own handle.
        let privileged = crate::handle::handle_at(mount.as_fd(), b"")
            .is_some_and(|h| open_by_handle(mount.as_fd(), &h, libc::O_PATH).is_ok());
        HandleAccess { mount, privileged }
    }

    /// Path of whatever a handle names now (for handles the tree doesn't know).
    pub fn path_of(&self, handle: &[u8]) -> Option<PathBuf> {
        if !self.privileged {
            return None;
        }
        let fd = open_by_handle(self.mount.as_fd(), handle, libc::O_PATH).ok()?;
        fd_path(fd.as_fd())
    }
}

impl DirAccess for HandleAccess {
    fn open(&self, tree: &Tree, dir: Id) -> io::Result<OwnedFd> {
        match tree.dir(dir).and_then(|d| d.handle.as_deref()) {
            Some(h) if self.privileged => open_by_handle(self.mount.as_fd(), h, libc::O_RDONLY | libc::O_DIRECTORY),
            _ => ByPath.open(tree, dir),
        }
    }

    fn locate(&self, tree: &Tree, dir: Id) -> Option<PathBuf> {
        self.path_of(tree.dir(dir)?.handle.as_deref()?)
    }

    fn find_handle(&self, handle: &[u8]) -> Option<(Stat, Option<PathBuf>)> {
        if !self.privileged {
            return None;
        }
        let fd = open_by_handle(self.mount.as_fd(), handle, libc::O_PATH).ok()?;
        let st = sys::stat_fd(fd.as_fd()).ok()?;
        Some((st, fd_path(fd.as_fd())))
    }
}

/// [`HandleAccess`] with the directories opened during one batch of events
/// kept open: a build writes thousands of files into a handful of folders,
/// and `open_by_handle_at` is the expensive part of applying an event.
/// Keyed by tree id and inode, so a reused id never gets a stale fd.
pub struct BatchAccess<'a> {
    pub inner: &'a HandleAccess,
    open: RefCell<HashMap<(Id, u64), OwnedFd>>,
}

impl<'a> BatchAccess<'a> {
    pub fn new(inner: &'a HandleAccess) -> Self {
        BatchAccess { inner, open: RefCell::new(HashMap::new()) }
    }
}

fn dup(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: duplicating a live fd; the result is a new fd we own.
    let n = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a fresh fd nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(n) })
}

impl DirAccess for BatchAccess<'_> {
    fn open(&self, tree: &Tree, dir: Id) -> io::Result<OwnedFd> {
        let key = (dir, tree.node(dir).ino);
        if let Some(fd) = self.open.borrow().get(&key) {
            return dup(fd);
        }
        let fd = self.inner.open(tree, dir)?;
        let mut open = self.open.borrow_mut();
        if open.len() >= 256 {
            open.clear();
        }
        let copy = dup(&fd)?;
        open.insert(key, fd);
        Ok(copy)
    }

    fn locate(&self, tree: &Tree, dir: Id) -> Option<PathBuf> {
        self.inner.locate(tree, dir)
    }

    fn find_handle(&self, handle: &[u8]) -> Option<(Stat, Option<PathBuf>)> {
        self.inner.find_handle(handle)
    }
}
