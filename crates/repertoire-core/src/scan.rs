//! Full and partial scans: a parallel walk (rayon) that never leaves the
//! filesystem it started on.
//!
//! The walk produces a plain [`ScanDir`] tree, which [`Tree::graft`] turns
//! into arena nodes in one sequential pass. Directories are opened with
//! `openat` relative to their parent and entries are `fstatat`ed, so no full
//! path is resolved per entry.
//!
//! [`Tree::graft`]: crate::Tree::graft

use crate::sys::{self, Stat};
use rayon::prelude::*;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub struct ScanFile {
    pub name: Box<[u8]>,
    pub stat: Stat,
}

#[derive(Debug)]
pub struct ScanDir {
    pub name: Box<[u8]>,
    pub stat: Stat,
    pub handle: Option<Box<[u8]>>,
    /// Couldn't be listed (or some entries couldn't be stat'ed).
    pub unreadable: bool,
    pub files: Vec<ScanFile>,
    pub dirs: Vec<ScanDir>,
}

/// An opaque, stable identity for the directory `name` inside `dir`, or for
/// `dir` itself when `name` is empty. On Linux this is the file handle
/// fanotify reports.
pub type HandleFn = fn(BorrowedFd, &[u8]) -> Option<Box<[u8]>>;

/// What not to index. Empty by default: the point is to see everything.
#[derive(Clone, Debug, Default)]
pub struct Excludes {
    /// Absolute directory paths.
    pub paths: Vec<PathBuf>,
    /// Directory names, anywhere.
    pub names: HashSet<Box<[u8]>>,
}

impl Excludes {
    pub fn dir(&self, path: &Path, name: &[u8]) -> bool {
        self.names.contains(name) || self.paths.iter().any(|p| p == path)
    }
}

#[derive(Default)]
pub struct Scanner {
    pub excludes: Excludes,
    /// Other mount points. Never entered, even when they're on the same
    /// device (bind mounts would be counted twice).
    pub skip: HashSet<PathBuf>,
    pub handle_of: Option<HandleFn>,
    /// Bytes seen so far, for progress reporting. Shared so a status
    /// request can read it while the walk runs.
    pub progress: Arc<AtomicU64>,
}

impl Scanner {
    /// Scan a mount point (or any directory given as a root). The result is
    /// named by its full path, as the tree names mount nodes.
    pub fn scan_root(&self, path: &Path) -> io::Result<ScanDir> {
        let fd = sys::open_dir(path)?;
        let stat = sys::stat_fd(fd.as_fd())?;
        Ok(self.walk(fd, path.to_path_buf(), path.as_os_str().as_bytes().into(), stat))
    }

    /// Scan the directory `name` inside the open directory `parent`, whose
    /// full path is `parent_path`. `None` if it isn't a directory on `dev`
    /// any more, or is excluded.
    pub fn scan_at(&self, parent: BorrowedFd, parent_path: &Path, name: &[u8], dev: u64) -> Option<ScanDir> {
        let path = parent_path.join(OsStr::from_bytes(name));
        if self.skip.contains(&path) || self.excludes.dir(&path, name) {
            return None;
        }
        match sys::open_dir_at(parent, name) {
            Ok(fd) => {
                let stat = sys::stat_fd(fd.as_fd()).ok()?;
                (stat.dev == dev).then(|| self.walk(fd, path, name.into(), stat))
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let stat = sys::stat_at(parent, name).ok().filter(|s| s.is_dir() && s.dev == dev)?;
                self.progress.fetch_add(stat.bytes, Ordering::Relaxed);
                let handle = self.handle_of.and_then(|f| f(parent, name));
                Some(ScanDir { handle, ..unreadable(name.into(), stat) })
            }
            Err(_) => None,
        }
    }

    /// Scan an already open directory, known as `name` at `path`.
    pub fn scan_fd(&self, fd: OwnedFd, path: PathBuf, name: Box<[u8]>, stat: Stat) -> ScanDir {
        self.walk(fd, path, name, stat)
    }

    fn walk(&self, fd: OwnedFd, path: PathBuf, name: Box<[u8]>, stat: Stat) -> ScanDir {
        self.progress.fetch_add(stat.bytes, Ordering::Relaxed);
        let handle = self.handle_of.and_then(|f| f(fd.as_fd(), b""));
        let Ok(names) = sys::read_dir(fd.as_fd()) else {
            return ScanDir { handle, ..unreadable(name, stat) };
        };
        let mut files = Vec::new();
        let mut subdirs = Vec::new();
        let mut denied = false;
        for n in names {
            match sys::stat_at(fd.as_fd(), &n) {
                Ok(st) if st.is_dir() => {
                    // A different device is another mount: never cross.
                    if st.dev == stat.dev {
                        subdirs.push(n);
                    }
                }
                Ok(st) => {
                    self.progress.fetch_add(st.bytes, Ordering::Relaxed);
                    files.push(ScanFile { name: n, stat: st });
                }
                Err(e) => denied |= e.kind() == io::ErrorKind::PermissionDenied, // vanished: skip
            }
        }
        let dirs = subdirs
            .into_par_iter()
            .filter_map(|n| self.scan_at(fd.as_fd(), &path, &n, stat.dev))
            .collect();
        ScanDir { name, stat, handle, unreadable: denied, files, dirs }
    }
}

/// A directory we can't open: only its own size counts.
pub fn unreadable(name: Box<[u8]>, stat: Stat) -> ScanDir {
    ScanDir { name, stat, handle: None, unreadable: true, files: Vec::new(), dirs: Vec::new() }
}
