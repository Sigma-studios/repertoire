//! Applying change events to a tree.
//!
//! Every event boils down to "look at this entry again": the applier stats
//! the entry *now* and makes the tree agree with what it finds. That makes
//! events idempotent and tolerant of arriving late: a create followed by a
//! delete leaves nothing behind, and a modify for a file that is gone since
//! removes it. Only renames carry information a stat can't recover (that a
//! known subtree moved), so they move nodes when the destination still holds
//! the same inode, and fall back to "look at both names again" otherwise.
//!
//! Event sources (fanotify on Linux, synthetic events in tests) identify a
//! directory by an opaque handle; [`Tree::apply_raw`] resolves it.

use crate::scan::{ScanDir, Scanner};
use crate::sys::{self, Stat};
use crate::tree::{Id, Kind, Tree, ROOT};
use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// A change, with directories already resolved to tree ids.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Created { dir: Id, name: Box<[u8]> },
    Removed { dir: Id, name: Box<[u8]> },
    /// Content changed (sizes): sources should debounce these.
    Modified { dir: Id, name: Box<[u8]> },
    /// An inode we only know by identity changed; see [`Tree::inode_changed`].
    Inode { stat: Stat, path: Option<PathBuf> },
    /// Mode, owner, times or link count changed.
    Attrib { dir: Id, name: Box<[u8]> },
    Renamed { from: Id, from_name: Box<[u8]>, to: Id, to_name: Box<[u8]> },
    /// Something about the directory itself changed.
    DirChanged { dir: Id },
    /// Events were lost: only a rescan can recover.
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    /// Refers to something we don't index or that is gone.
    Ignored,
    /// The tree can't be trusted any more; rescan the filesystem.
    Rescan,
}

/// How the applier reaches a directory of the tree on disk.
pub trait DirAccess {
    /// Open the directory `dir` as it is now.
    fn open(&self, tree: &Tree, dir: Id) -> io::Result<OwnedFd>;

    /// Where the directory `dir` is now, if it still exists. Lets a rename
    /// that was overtaken by a later one still move the subtree instead of
    /// rescanning it. Linux answers through the directory's file handle.
    fn locate(&self, _tree: &Tree, _dir: Id) -> Option<PathBuf> {
        None
    }

    /// `stat` of an object known only by its handle, and one of its paths
    /// if known (Linux: `open_by_handle_at` and `/proc/self/fd`). `None`
    /// when it's gone or unsupported.
    fn find_handle(&self, _handle: &[u8]) -> Option<(Stat, Option<PathBuf>)> {
        None
    }
}

/// Opens directories by their path in the tree and checks the inode. Right
/// as long as events are applied promptly; a directory renamed since is
/// reported as stale rather than guessed.
pub struct ByPath;

impl DirAccess for ByPath {
    fn open(&self, tree: &Tree, dir: Id) -> io::Result<OwnedFd> {
        let fd = sys::open_dir(&tree.path(dir))?;
        let st = sys::stat_fd(fd.as_fd())?;
        if st.ino != tree.node(dir).ino {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(fd)
    }
}

/// What an event source reports: directories by handle, entries by name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RawEvent {
    Created { dir: Box<[u8]>, name: Box<[u8]> },
    Removed { dir: Box<[u8]>, name: Box<[u8]> },
    /// `obj` is the file's own handle, when the source reports it. It's
    /// used when the entry can't be reached by the time the event is
    /// applied: the inode may live on through a hard link elsewhere.
    Modified { dir: Box<[u8]>, name: Box<[u8]>, obj: Option<Box<[u8]>> },
    Attrib { dir: Box<[u8]>, name: Box<[u8]>, obj: Option<Box<[u8]>> },
    Renamed { from: Box<[u8]>, from_name: Box<[u8]>, to: Box<[u8]>, to_name: Box<[u8]> },
    DirChanged { dir: Box<[u8]> },
    /// Something about an inode changed (link count, attributes) and the
    /// source only knows it by handle.
    Inode { obj: Box<[u8]> },
    Overflow,
}

/// Turns a handle the tree doesn't know into a path (Linux:
/// `open_by_handle_at` + `/proc/self/fd`). `None` when it can't.
pub type HandlePath<'a> = &'a dyn Fn(&[u8]) -> Option<PathBuf>;

pub struct Ctx<'a> {
    pub scanner: &'a Scanner,
    pub access: &'a dyn DirAccess,
    pub handle_path: HandlePath<'a>,
}

impl Tree {
    /// Resolve a raw event's handles and apply it.
    pub fn apply_raw(&mut self, ev: &RawEvent, cx: &Ctx) -> Outcome {
        let out = self.apply_raw_inner(ev, cx);
        if self.take_needs_rescan() {
            Outcome::Rescan
        } else {
            out
        }
    }

    fn apply_raw_inner(&mut self, ev: &RawEvent, cx: &Ctx) -> Outcome {
        let inode = |obj: &[u8]| cx.access.find_handle(obj).map(|(stat, path)| Event::Inode { stat, path });
        let ev = match ev {
            RawEvent::Overflow => Event::Overflow,
            RawEvent::Inode { obj } => match inode(obj) {
                Some(ev) => ev,
                None => return Outcome::Ignored,
            },
            RawEvent::Created { dir, name } => match self.resolve(dir, cx) {
                Some(dir) => Event::Created { dir, name: name.clone() },
                None => return Outcome::Ignored,
            },
            RawEvent::Removed { dir, name } => match self.resolve(dir, cx) {
                Some(dir) => Event::Removed { dir, name: name.clone() },
                None => return Outcome::Ignored,
            },
            RawEvent::Modified { dir, name, obj } | RawEvent::Attrib { dir, name, obj } => {
                let found = match self.resolve(dir, cx) {
                    Some(dir) => self.refresh(dir, name, cx) == Outcome::Applied && self.child(dir, name).is_some(),
                    None => false,
                };
                if found {
                    return Outcome::Applied;
                }
                match obj.as_deref().and_then(inode) {
                    Some(ev) => ev,
                    None => return Outcome::Ignored,
                }
            }
            RawEvent::DirChanged { dir } => match self.resolve(dir, cx) {
                Some(dir) => Event::DirChanged { dir },
                None => return Outcome::Ignored,
            },
            RawEvent::Renamed { from, from_name, to, to_name } => {
                let (f, t) = (self.resolve(from, cx), self.resolve(to, cx));
                match (f, t) {
                    (Some(from), Some(to)) => {
                        Event::Renamed { from, from_name: from_name.clone(), to, to_name: to_name.clone() }
                    }
                    // Moved in from somewhere we don't index.
                    (None, Some(to)) => Event::Created { dir: to, name: to_name.clone() },
                    // Moved out.
                    (Some(from), None) => Event::Removed { dir: from, name: from_name.clone() },
                    (None, None) => return Outcome::Ignored,
                }
            }
        };
        self.apply(&ev, cx)
    }

    /// The directory a handle names: from the handle map, or else through
    /// the platform fallback and a path walk that indexes missing parts.
    pub fn resolve(&mut self, handle: &[u8], cx: &Ctx) -> Option<Id> {
        if let Some(id) = self.resolve_handle(handle) {
            return Some(id);
        }
        let path = (cx.handle_path)(handle)?;
        self.ensure_dir(&path, cx)
    }

    /// Find a directory by path, indexing any missing component on the way
    /// (it was created between the scan and the first event about it).
    pub fn ensure_dir(&mut self, path: &Path, cx: &Ctx) -> Option<Id> {
        if let Some(id) = self.lookup(path) {
            return self.node(id).is_dir().then_some(id);
        }
        let mount = self
            .mounts()
            .iter()
            .copied()
            .filter(|&m| path.starts_with(self.node(m).name()))
            .max_by_key(|&m| self.node(m).name.len())?;
        let rest = path.strip_prefix(self.node(mount).name()).ok()?.to_path_buf();
        let mut id = mount;
        for c in rest.components() {
            let Component::Normal(name) = c else { return None };
            let name = name.as_bytes();
            if self.child(id, name).is_none() {
                self.refresh(id, name, cx);
            }
            id = self.child(id, name).filter(|&c| self.node(c).is_dir())?;
        }
        Some(id)
    }

    pub fn apply(&mut self, ev: &Event, cx: &Ctx) -> Outcome {
        match ev {
            Event::Overflow => Outcome::Rescan,
            Event::Created { dir, name }
            | Event::Removed { dir, name }
            | Event::Modified { dir, name }
            | Event::Attrib { dir, name } => self.refresh(*dir, name, cx),
            Event::Inode { stat, path } => {
                self.inode_changed(stat, path.as_deref());
                Outcome::Applied
            }
            Event::DirChanged { dir } => {
                if !self.is_dir_id(*dir) {
                    return Outcome::Ignored;
                }
                self.update_dir_from_disk(*dir, cx);
                Outcome::Applied
            }
            Event::Renamed { from, from_name, to, to_name } => self.rename(*from, from_name, *to, to_name, cx),
        }
    }

    /// [`Tree::flush_links`], re-stat'ing members through `cx`.
    pub fn flush_links_with(&mut self, cx: &Ctx) {
        self.flush_links(&|tree, id| {
            let fd = cx.access.open(tree, tree.node(id).parent).ok()?;
            sys::stat_at(fd.as_fd(), &tree.node(id).name).ok()
        });
    }

    fn update_dir_from_disk(&mut self, dir: Id, cx: &Ctx) {
        if let Ok(st) = cx.access.open(self, dir).and_then(|fd| sys::stat_fd(fd.as_fd())) {
            self.sync_dir(dir, &st, cx);
        }
    }

    /// Bring a directory's own metadata up to date. When not running as
    /// root, a permission change can change what we're able to list, so the
    /// directory is scanned again (through its own handle: its parent may
    /// not be where the tree thinks yet); it then gets a new id, which is
    /// returned. Root reads everything and only needs the new metadata.
    fn sync_dir(&mut self, dir: Id, st: &Stat, cx: &Ctx) -> Id {
        let old = self.dir(dir).map(|d| (d.mode, d.uid, d.gid));
        if old == Some((st.mode, st.uid, st.gid)) || sys::euid() == 0 {
            self.update_dir(dir, st);
            return dir;
        }
        let (parent, name) = (self.node(dir).parent, self.node(dir).name.clone());
        let scan = match cx.access.open(self, dir) {
            Ok(fd) => cx.scanner.scan_fd(fd, self.path(dir), name, *st),
            Err(_) => {
                let handle = self.dir(dir).and_then(|d| d.handle.clone());
                ScanDir { handle, ..crate::scan::unreadable(name, *st) }
            }
        };
        self.graft(parent, scan, true)
    }

    fn is_dir_id(&self, id: Id) -> bool {
        id != ROOT && self.is_live(id) && self.node(id).is_dir()
    }

    /// Look at `dir/name` again and make the tree match it.
    pub fn refresh(&mut self, dir: Id, name: &[u8], cx: &Ctx) -> Outcome {
        if !self.is_dir_id(dir) {
            return Outcome::Ignored;
        }
        let Ok(fd) = cx.access.open(self, dir) else { return Outcome::Ignored };
        if let Ok(st) = sys::stat_fd(fd.as_fd()) {
            // Its mtime moved, and a directory's own blocks grow with entries.
            if self.sync_dir(dir, &st, cx) != dir {
                return Outcome::Applied; // scanned again, `name` included
            }
        }
        self.refresh_in(dir, &fd, name, cx);
        Outcome::Applied
    }

    fn refresh_in(&mut self, dir: Id, fd: &OwnedFd, name: &[u8], cx: &Ctx) {
        let existing = self.child(dir, name);
        let st = match sys::stat_at(fd.as_fd(), name) {
            Ok(st) => st,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if let Some(x) = existing {
                    self.remove(x);
                }
                return;
            }
            // Can't look (permissions): leave what we have.
            Err(_) => return,
        };
        if let Some(x) = existing {
            let n = self.node(x);
            if n.ino == st.ino && n.kind == st.kind() {
                if n.kind == Kind::Dir {
                    self.sync_dir(x, &st, cx);
                } else {
                    self.update_file(x, &st);
                }
                return;
            }
            self.remove(x);
        }
        self.insert(dir, fd, name, &st, cx);
    }

    fn insert(&mut self, dir: Id, fd: &OwnedFd, name: &[u8], st: &Stat, cx: &Ctx) {
        let dev = self.dev_of(dir);
        if st.dev != dev {
            return; // a mount point of another filesystem
        }
        if st.is_dir() {
            if let Some(scan) = cx.scanner.scan_at(fd.as_fd(), &self.path(dir), name, dev) {
                self.graft(dir, scan, true);
            }
        } else {
            self.insert_file(dir, name, st, true);
        }
    }

    fn rename(&mut self, from: Id, from_name: &[u8], to: Id, to_name: &[u8], cx: &Ctx) -> Outcome {
        let moving = if self.is_dir_id(from) { self.child(from, from_name) } else { None };
        if let (Some(x), true) = (moving, self.is_dir_id(to)) {
            if let Ok(tfd) = cx.access.open(self, to) {
                if let Ok(st) = sys::stat_at(tfd.as_fd(), to_name) {
                    let (ino, kind) = (self.node(x).ino, self.node(x).kind);
                    let there = self.child(to, to_name).filter(|&c| c != x);
                    if kind == Kind::Dir && there.is_some_and(|c| self.node(c).ino == ino) {
                        // A scan already indexed it there, more recently.
                        self.remove(x);
                        self.update_dir_from_disk(to, cx);
                        return Outcome::Applied;
                    }
                    if ino == st.ino && kind == st.kind() && self.move_to(x, to, to_name) {
                        if kind == Kind::Dir {
                            self.sync_dir(x, &st, cx);
                        } else {
                            self.update_file(x, &st);
                        }
                        self.update_dir_from_disk(to, cx);
                        // Normally gone; not when the event was stale.
                        self.refresh(from, from_name, cx);
                        return Outcome::Applied;
                    }
                }
            }
        }
        // The destination doesn't hold what we know as the source: it moved
        // again since. If it's a directory still alive somewhere we index,
        // move it there rather than dropping and rescanning the subtree.
        if let Some(x) = moving.filter(|&x| self.node(x).is_dir()) {
            if let Some(now) = cx.access.locate(self, x) {
                let parent = now.parent().and_then(|p| self.lookup(p)).filter(|&p| self.is_dir_id(p));
                if let (Some(p), Some(n)) = (parent, now.file_name()) {
                    let ino = self.node(x).ino;
                    match self.child(p, n.as_bytes()) {
                        Some(c) if c != x && self.node(c).ino == ino => self.remove(x), // fresher copy there
                        _ => {
                            self.move_to(x, p, n.as_bytes());
                        }
                    }
                }
            }
        }
        let a = self.refresh(from, from_name, cx);
        let b = self.refresh(to, to_name, cx);
        if a == Outcome::Applied || b == Outcome::Applied {
            Outcome::Applied
        } else {
            Outcome::Ignored
        }
    }
}

/// Path of `name` inside the directory `dir` of `tree`.
pub fn entry_path(tree: &Tree, dir: Id, name: &[u8]) -> PathBuf {
    tree.path(dir).join(OsStr::from_bytes(name))
}
