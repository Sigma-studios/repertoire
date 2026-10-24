//! The in-memory index of one filesystem: an arena of directories and files.
//!
//! Totals are never recomputed: every change is applied as a delta up the
//! parent chain, so a file growing by N adds N to each ancestor (`O(depth)`).
//!
//! The root is virtual. Its children are the filesystem's mount points, named
//! by their full path (`/`, `/home`), so one filesystem mounted twice (btrfs
//! subvolumes) still forms one tree.
//!
//! Hard links count once: files with `st_nlink > 1` form a group keyed by
//! (device, inode) and exactly one member, the *charged* one, contributes its
//! bytes. When it disappears another surviving member is charged.

use crate::scan::{ScanDir, ScanFile};
use crate::sys::Stat;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

pub type Id = u32;
pub const NONE: Id = u32::MAX;
pub const ROOT: Id = 0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Dir,
    File,
    Symlink,
    /// Sockets, fifos, device nodes.
    Other,
}

const LINKED: u8 = 1;
const CHARGED: u8 = 2;
const UNREADABLE: u8 = 4;
const FREE: u8 = 8;

/// One entry. Kept small: there are hundreds of thousands of them.
#[derive(Clone, Debug)]
pub struct Node {
    pub parent: Id,
    /// Index into `Tree::dirs` for directories, `NONE` otherwise.
    dir: u32,
    pub kind: Kind,
    flags: u8,
    pub name: Box<[u8]>,
    pub ino: u64,
    /// Allocated bytes. For a directory, the directory inode itself.
    pub bytes: u64,
    /// `st_size`, for display.
    pub apparent: u64,
    pub mtime: i64,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    pub fn name(&self) -> &OsStr {
        OsStr::from_bytes(&self.name)
    }

    /// Bytes this entry adds to its directory: 0 for an uncharged hard link.
    fn contrib(&self) -> u64 {
        if self.flags & LINKED != 0 && self.flags & CHARGED == 0 {
            0
        } else {
            self.bytes
        }
    }
}

/// Directory-only data.
#[derive(Clone, Debug, Default)]
pub struct Dir {
    /// Sorted by name, so lookups are a binary search.
    pub children: Vec<Id>,
    /// The directory inode plus the files directly inside.
    pub own: u64,
    /// `own` plus every subdirectory's total.
    pub total: u64,
    /// Non-directories anywhere below.
    pub files: u64,
    /// Directories anywhere below (not counting this one).
    pub dirs: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub dev: u64,
    /// Opaque, stable identity used by event sources (a Linux file handle).
    pub handle: Option<Box<[u8]>>,
}

/// Hard-link group key: (device, inode).
type LinkKey = (u64, u64);

fn group_first(links: &HashMap<LinkKey, Vec<Id>>, key: LinkKey) -> Id {
    links[&key][0]
}

#[derive(Clone, Default)]
pub struct Tree {
    nodes: Vec<Node>,
    free: Vec<Id>,
    dirs: Vec<Dir>,
    free_dirs: Vec<u32>,
    handles: HashMap<Box<[u8]>, Id>,
    /// Members of each group; the charged one is flagged, not positional.
    links: HashMap<LinkKey, Vec<Id>>,
    /// Groups that may be missing members indexed before the link was
    /// made: found by one pass over the arena in [`Tree::flush_links`].
    pending_scan: HashSet<LinkKey>,
    /// Groups whose known members should be stat'ed again (one of their
    /// links went away, and the inode may have changed through it).
    pending_restat: HashSet<LinkKey>,
    /// Older copies of directories a graft just found elsewhere.
    stale: Vec<Id>,
    /// Set when the tree got into a state only a rescan can sort out.
    needs_rescan: bool,
    generation: u64,
}

impl Tree {
    pub fn new() -> Tree {
        let mut t = Tree::default();
        t.dirs.push(Dir::default());
        t.nodes.push(Node {
            parent: NONE,
            dir: 0,
            kind: Kind::Dir,
            flags: 0,
            name: Box::default(),
            ino: 0,
            bytes: 0,
            apparent: 0,
            mtime: 0,
        });
        t
    }

    // ------------------------------------------------------------ reading

    pub fn node(&self, id: Id) -> &Node {
        &self.nodes[id as usize]
    }

    pub fn dir(&self, id: Id) -> Option<&Dir> {
        let n = self.nodes.get(id as usize)?;
        (n.dir != NONE && n.flags & FREE == 0).then(|| &self.dirs[n.dir as usize])
    }

    fn dir_mut(&mut self, id: Id) -> &mut Dir {
        let d = self.nodes[id as usize].dir;
        &mut self.dirs[d as usize]
    }

    pub fn is_live(&self, id: Id) -> bool {
        self.nodes.get(id as usize).is_some_and(|n| n.flags & FREE == 0)
    }

    pub fn is_unreadable(&self, id: Id) -> bool {
        self.nodes[id as usize].flags & UNREADABLE != 0
    }

    /// Size of an entry as its parent counts it: a directory's total, a
    /// file's bytes (0 for an uncharged hard link).
    pub fn size(&self, id: Id) -> u64 {
        match self.dir(id) {
            Some(d) => d.total,
            None => self.nodes[id as usize].contrib(),
        }
    }

    pub fn children(&self, id: Id) -> &[Id] {
        self.dir(id).map(|d| d.children.as_slice()).unwrap_or(&[])
    }

    pub fn child(&self, dir: Id, name: &[u8]) -> Option<Id> {
        let d = self.dir(dir)?;
        d.children
            .binary_search_by(|&c| self.nodes[c as usize].name.as_ref().cmp(name))
            .ok()
            .map(|i| d.children[i])
    }

    /// Mount point nodes (children of the virtual root).
    pub fn mounts(&self) -> &[Id] {
        self.children(ROOT)
    }

    pub fn mount_of(&self, mut id: Id) -> Id {
        while self.nodes[id as usize].parent != ROOT && id != ROOT {
            id = self.nodes[id as usize].parent;
        }
        id
    }

    /// Bumped on every change; lets readers skip work when nothing moved.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Live entries, the virtual root excluded.
    pub fn len(&self) -> usize {
        self.nodes.len() - self.free.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Rough heap use in bytes (arena, names, child lists, maps).
    pub fn memory(&self) -> usize {
        let names: usize = self.nodes.iter().map(|n| if n.name.is_empty() { 0 } else { n.name.len().next_multiple_of(16) }).sum();
        let kids: usize = self.dirs.iter().map(|d| d.children.capacity() * 4 + d.handle.as_ref().map_or(0, |h| h.len() + 16)).sum();
        self.nodes.capacity() * std::mem::size_of::<Node>()
            + self.dirs.capacity() * std::mem::size_of::<Dir>()
            + names
            + kids
            + self.handles.capacity() * 40
            + self.links.capacity() * 48
    }

    pub fn path(&self, mut id: Id) -> PathBuf {
        let mut parts: Vec<&[u8]> = Vec::new();
        while id != ROOT && id != NONE {
            let n = &self.nodes[id as usize];
            parts.push(&n.name);
            id = n.parent;
        }
        let mut p = PathBuf::new();
        for part in parts.iter().rev() {
            p.push(OsStr::from_bytes(part));
        }
        p
    }

    /// Find an entry by absolute path, starting from the deepest mount point
    /// that contains it.
    pub fn lookup(&self, path: &Path) -> Option<Id> {
        let (mount, rest) = self
            .mounts()
            .iter()
            .filter_map(|&m| Some((m, path.strip_prefix(OsStr::from_bytes(&self.nodes[m as usize].name)).ok()?)))
            .max_by_key(|(m, _)| self.nodes[*m as usize].name.len())?;
        let mut id = mount;
        for c in rest.components() {
            match c {
                Component::Normal(name) => id = self.child(id, name.as_bytes())?,
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(id)
    }

    pub fn resolve_handle(&self, handle: &[u8]) -> Option<Id> {
        self.handles.get(handle).copied()
    }

    /// Every live directory with a recorded handle (for the event source).
    pub fn handles(&self) -> impl Iterator<Item = (&[u8], Id)> {
        self.handles.iter().map(|(h, &id)| (h.as_ref(), id))
    }

    /// Live node ids, in arena order. Used for name search.
    pub fn ids(&self) -> impl Iterator<Item = Id> + '_ {
        self.nodes.iter().enumerate().filter(|(i, n)| *i != 0 && n.flags & FREE == 0).map(|(i, _)| i as Id)
    }

    pub fn dev_of(&self, id: Id) -> u64 {
        let n = &self.nodes[id as usize];
        let dir = if n.is_dir() { id } else { n.parent };
        self.dir(dir).map_or(0, |d| d.dev)
    }

    // ------------------------------------------------------------ allocation

    fn alloc(&mut self, node: Node) -> Id {
        if let Some(id) = self.free.pop() {
            self.nodes[id as usize] = node;
            id
        } else {
            self.nodes.push(node);
            (self.nodes.len() - 1) as Id
        }
    }

    fn alloc_dir(&mut self, dir: Dir) -> u32 {
        if let Some(i) = self.free_dirs.pop() {
            self.dirs[i as usize] = dir;
            i
        } else {
            self.dirs.push(dir);
            (self.dirs.len() - 1) as u32
        }
    }

    fn release(&mut self, id: Id) {
        let n = &mut self.nodes[id as usize];
        n.flags = FREE;
        n.name = Box::default();
        if n.dir != NONE {
            let di = n.dir;
            n.dir = NONE;
            let d = std::mem::take(&mut self.dirs[di as usize]);
            if let Some(h) = d.handle {
                if self.handles.get(&h) == Some(&id) {
                    self.handles.remove(&h);
                }
            }
            self.free_dirs.push(di);
        }
        self.free.push(id);
    }

    // ------------------------------------------------------------ deltas

    /// Add to `total`, `files` and `dirs` of `dir` and all its ancestors.
    fn bubble(&mut self, mut dir: Id, bytes: i64, files: i64, dirs: i64) {
        self.generation += 1;
        if bytes == 0 && files == 0 && dirs == 0 {
            return;
        }
        while dir != NONE {
            let n = &self.nodes[dir as usize];
            let parent = n.parent;
            let d = &mut self.dirs[n.dir as usize];
            d.total = d.total.wrapping_add_signed(bytes);
            d.files = d.files.wrapping_add_signed(files);
            d.dirs = d.dirs.wrapping_add_signed(dirs);
            dir = parent;
        }
    }

    /// Change a file's flags or bytes and propagate the change in what it
    /// contributes.
    fn set_file(&mut self, id: Id, flags: u8, bytes: u64) {
        let n = &mut self.nodes[id as usize];
        let before = n.contrib();
        n.flags = flags;
        n.bytes = bytes;
        let after = n.contrib();
        let parent = n.parent;
        if before != after {
            let delta = after as i64 - before as i64;
            let d = self.dir_mut(parent);
            d.own = d.own.wrapping_add_signed(delta);
            self.bubble(parent, delta, 0, 0);
        } else {
            self.generation += 1;
        }
    }

    // ------------------------------------------------------------ children

    fn attach(&mut self, parent: Id, id: Id) {
        let name = self.nodes[id as usize].name.clone();
        let pos = match self.dir(parent).unwrap().children.binary_search_by(|&c| self.nodes[c as usize].name.as_ref().cmp(&name)) {
            Ok(i) | Err(i) => i,
        };
        self.dir_mut(parent).children.insert(pos, id);
    }

    fn detach(&mut self, parent: Id, id: Id) {
        let kids = &mut self.dir_mut(parent).children;
        if let Some(i) = kids.iter().position(|&c| c == id) {
            kids.remove(i);
        }
    }

    /// (bytes, files, dirs) an entry adds to each ancestor.
    fn weight(&self, id: Id) -> (u64, u64, u64) {
        match self.dir(id) {
            Some(d) => (d.total, d.files, d.dirs + 1),
            None => (self.nodes[id as usize].contrib(), 1, 0),
        }
    }

    // ------------------------------------------------------------ mutation

    /// Add a non-directory entry. `pending` marks a new hard-link group as
    /// possibly incomplete (see [`Tree::flush_links`]).
    pub fn insert_file(&mut self, parent: Id, name: &[u8], st: &Stat, pending: bool) -> Id {
        if let Some(old) = self.child(parent, name) {
            self.remove(old);
        }
        let dev = self.dir(parent).map_or(0, |d| d.dev);
        let id = self.new_file(parent, name.into(), st, dev, pending);
        self.attach(parent, id);
        let contrib = self.nodes[id as usize].contrib();
        self.dir_mut(parent).own += contrib;
        self.bubble(parent, contrib as i64, 1, 0);
        id
    }

    fn new_file(&mut self, parent: Id, name: Box<[u8]>, st: &Stat, dev: u64, pending: bool) -> Id {
        let id = self.alloc(Node {
            parent,
            dir: NONE,
            kind: st.kind(),
            flags: 0,
            name,
            ino: st.ino,
            bytes: st.bytes,
            apparent: st.size,
            mtime: st.mtime,
        });
        if st.nlink > 1 {
            let key = (dev, st.ino);
            let group = self.links.entry(key).or_default();
            let flags = if group.is_empty() { LINKED | CHARGED } else { LINKED };
            group.push(id);
            let complete = group.len() as u64 >= st.nlink;
            self.nodes[id as usize].flags = flags;
            if complete {
                // Every link is known: nothing to look for.
                self.pending_scan.remove(&key);
            } else if flags & CHARGED != 0 && pending {
                self.pending_scan.insert(key);
            }
        }
        id
    }

    /// Add a scanned directory (and everything below it) under `parent`.
    /// Totals are summed bottom-up, then added to the ancestors once.
    pub fn graft(&mut self, parent: Id, scan: ScanDir, pending: bool) -> Id {
        if let Some(old) = self.child(parent, &scan.name) {
            self.remove(old);
        }
        let dev = scan.stat.dev;
        let id = self.build(parent, scan, dev, pending);
        self.attach(parent, id);
        let (bytes, files, dirs) = self.weight(id);
        self.bubble(parent, bytes as i64, files as i64, dirs as i64);
        // A directory can only be in one place. If the scan met one we had
        // indexed somewhere else, that copy is out of date (its rename
        // hasn't been applied yet): drop it.
        for old in std::mem::take(&mut self.stale) {
            if !self.is_live(old) {
                continue;
            }
            if self.is_ancestor(old, id) {
                self.needs_rescan = true;
            } else {
                self.remove(old);
            }
        }
        id
    }

    pub fn is_ancestor(&self, ancestor: Id, mut id: Id) -> bool {
        while id != NONE {
            if id == ancestor {
                return true;
            }
            id = self.nodes[id as usize].parent;
        }
        false
    }

    /// Whether something happened that only a rescan can fix; clears it.
    pub fn take_needs_rescan(&mut self) -> bool {
        std::mem::take(&mut self.needs_rescan)
    }

    fn build(&mut self, parent: Id, scan: ScanDir, dev: u64, pending: bool) -> Id {
        let ScanDir { name, stat, handle, unreadable, files, dirs } = scan;
        let di = self.alloc_dir(Dir { mode: stat.mode, uid: stat.uid, gid: stat.gid, dev, handle: handle.clone(), ..Dir::default() });
        let id = self.alloc(Node {
            parent,
            dir: di,
            kind: Kind::Dir,
            flags: if unreadable { UNREADABLE } else { 0 },
            name,
            ino: stat.ino,
            bytes: stat.bytes,
            apparent: stat.size,
            mtime: stat.mtime,
        });
        if let Some(h) = handle {
            if let Some(old) = self.handles.insert(h, id) {
                self.stale.push(old);
            }
        }
        let mut children = Vec::with_capacity(files.len() + dirs.len());
        let (mut own, mut nfiles, mut ndirs) = (stat.bytes, files.len() as u64, dirs.len() as u64);
        for ScanFile { name, stat } in files {
            let c = self.new_file(id, name, &stat, dev, pending);
            own += self.nodes[c as usize].contrib();
            children.push(c);
        }
        let mut total = own;
        for d in dirs {
            let c = self.build(id, d, dev, pending);
            let cd = &self.dirs[self.nodes[c as usize].dir as usize];
            total += cd.total;
            nfiles += cd.files;
            ndirs += cd.dirs;
            children.push(c);
        }
        children.sort_unstable_by(|&a, &b| self.nodes[a as usize].name.cmp(&self.nodes[b as usize].name));
        let d = &mut self.dirs[di as usize];
        d.children = children;
        d.own = own;
        d.total = total;
        d.files = nfiles;
        d.dirs = ndirs;
        id
    }

    /// Remove an entry and everything below it.
    pub fn remove(&mut self, id: Id) {
        assert!(id != ROOT, "can't remove the root");
        let parent = self.nodes[id as usize].parent;
        let (bytes, files, dirs) = self.weight(id);
        if !self.nodes[id as usize].is_dir() {
            self.dir_mut(parent).own -= bytes;
        }
        self.bubble(parent, -(bytes as i64), -(files as i64), -(dirs as i64));
        self.detach(parent, id);

        // Free the subtree, remembering hard-link groups that lost their
        // charged member. Groups are fixed before anything is released, since
        // a file's device is read from its parent directory.
        let mut all = Vec::new();
        let mut stack = vec![id];
        while let Some(x) = stack.pop() {
            if let Some(d) = self.dir(x) {
                stack.extend_from_slice(&d.children);
            }
            all.push(x);
        }
        let mut orphaned = Vec::new();
        for &x in &all {
            let n = &self.nodes[x as usize];
            if n.flags & LINKED == 0 || n.is_dir() {
                continue;
            }
            let (charged, key) = (n.flags & CHARGED != 0, (self.dev_of(x), n.ino));
            if let Some(group) = self.links.get_mut(&key) {
                group.retain(|&m| m != x);
                // The inode may have been written through the link that's
                // going away: re-stat the survivors. (An incomplete group
                // stays queued for a scan, which also finds links indexed
                // before they became links.)
                if group.is_empty() {
                    self.links.remove(&key);
                } else {
                    self.pending_restat.insert(key);
                    if charged {
                        orphaned.push(key);
                    }
                }
            }
        }
        for x in all {
            self.release(x);
        }
        for key in orphaned {
            if let Some(&next) = self.links.get(&key).and_then(|g| g.first()) {
                let bytes = self.nodes[next as usize].bytes;
                self.set_file(next, LINKED | CHARGED, bytes);
            }
        }
    }

    /// Move an entry to `new_parent` under `new_name`, replacing whatever was
    /// there. Returns false (and does nothing) if that would put a directory
    /// inside itself.
    pub fn move_to(&mut self, id: Id, new_parent: Id, new_name: &[u8]) -> bool {
        if self.is_ancestor(id, new_parent) {
            return false;
        }
        if let Some(old) = self.child(new_parent, new_name) {
            if old == id {
                return true;
            }
            self.remove(old);
        }
        let old_parent = self.nodes[id as usize].parent;
        let (bytes, files, dirs) = self.weight(id);
        let is_dir = self.nodes[id as usize].is_dir();
        if !is_dir {
            self.dir_mut(old_parent).own -= bytes;
        }
        self.bubble(old_parent, -(bytes as i64), -(files as i64), -(dirs as i64));
        self.detach(old_parent, id);

        let n = &mut self.nodes[id as usize];
        n.parent = new_parent;
        n.name = new_name.into();
        self.attach(new_parent, id);
        if !is_dir {
            self.dir_mut(new_parent).own += bytes;
        }
        self.bubble(new_parent, bytes as i64, files as i64, dirs as i64);
        true
    }

    /// Refresh a file from a new `stat`. Hard-link siblings share the inode,
    /// so they get the same sizes.
    pub fn update_file(&mut self, id: Id, st: &Stat) {
        let flags = self.nodes[id as usize].flags;
        let members = if flags & LINKED != 0 {
            let key = (self.dev_of(id), self.nodes[id as usize].ino);
            self.links.get(&key).cloned().unwrap_or_else(|| vec![id])
        } else if st.nlink > 1 {
            // Became a hard link (made elsewhere): join or start a group.
            let key = (self.dev_of(id), st.ino);
            let group = self.links.entry(key).or_default();
            let f = if group.is_empty() { LINKED | CHARGED } else { LINKED };
            group.push(id);
            if group.len() as u64 >= st.nlink {
                self.pending_scan.remove(&key);
            } else {
                self.pending_scan.insert(key);
            }
            let bytes = self.nodes[id as usize].bytes;
            self.set_file(id, f, bytes);
            self.links[&key].clone()
        } else {
            vec![id]
        };
        for m in members {
            let n = &mut self.nodes[m as usize];
            n.apparent = st.size;
            n.mtime = st.mtime;
            let f = n.flags;
            self.set_file(m, f, st.bytes);
        }
    }

    /// Refresh a directory's own metadata (its inode size, mode, owner, mtime).
    pub fn update_dir(&mut self, id: Id, st: &Stat) {
        let n = &mut self.nodes[id as usize];
        let delta = st.bytes as i64 - n.bytes as i64;
        n.bytes = st.bytes;
        n.apparent = st.size;
        n.mtime = st.mtime;
        let d = self.dir_mut(id);
        d.mode = st.mode;
        d.uid = st.uid;
        d.gid = st.gid;
        d.own = d.own.wrapping_add_signed(delta);
        self.bubble(id, delta, 0, 0);
    }

    pub fn set_handle(&mut self, id: Id, handle: Option<Box<[u8]>>) {
        if let Some(old) = self.dir_mut(id).handle.take() {
            self.handles.remove(&old);
        }
        if let Some(h) = &handle {
            self.handles.insert(h.clone(), id);
        }
        self.dir_mut(id).handle = handle;
    }

    pub fn set_unreadable(&mut self, id: Id, unreadable: bool) {
        let n = &mut self.nodes[id as usize];
        if unreadable {
            n.flags |= UNREADABLE;
        } else {
            n.flags &= !UNREADABLE;
        }
        self.generation += 1;
    }

    // ------------------------------------------------------------ hard links

    /// An inode changed and we only know it by identity (the event's
    /// directory is gone, or it was a link count change), plus maybe one of
    /// its paths. Directories are reached through their own events.
    pub fn inode_changed(&mut self, st: &Stat, path: Option<&Path>) {
        if st.is_dir() {
            return;
        }
        let at = path.and_then(|p| self.lookup(p)).filter(|&id| {
            let n = &self.nodes[id as usize];
            !n.is_dir() && n.ino == st.ino && self.dev_of(id) == st.dev
        });
        let key = (st.dev, st.ino);
        if let Some(id) = at {
            self.update_file(id, st); // also joins or updates its group
        } else if let Some(&first) = self.links.get(&key).and_then(|g| g.first()) {
            self.update_file(first, st);
        } else if st.nlink > 1 {
            self.pending_scan.insert(key);
        }
    }

    pub fn has_pending_links(&self) -> bool {
        !self.pending_scan.is_empty() || !self.pending_restat.is_empty()
    }

    /// Some group needs the (linear) arena pass.
    pub fn has_pending_scan(&self) -> bool {
        !self.pending_scan.is_empty()
    }

    /// Complete hard-link groups created by events: a new link to a file
    /// that was indexed while it had a single link. One pass over the arena
    /// handles every pending group at once. The members may have drifted
    /// apart while they weren't grouped, so the inode is stat'ed again
    /// through `restat` (any member will do) and every member takes that.
    ///
    /// With `scan` false only the cheap part runs: re-stat'ing known groups.
    pub fn flush_links(&mut self, restat: &dyn Fn(&Tree, Id) -> Option<Stat>, scan: bool) {
        for key in std::mem::take(&mut self.pending_restat) {
            let Some(group) = self.links.get(&key) else { continue };
            if let Some(st) = group.clone().iter().find_map(|&m| restat(self, m).filter(|st| st.ino == key.1)) {
                self.update_file(group_first(&self.links, key), &st);
                // Fewer links known than the inode has: a group counted as
                // complete may have counted a stale copy of a member.
                if (self.links.get(&key).map_or(0, Vec::len) as u64) < st.nlink {
                    self.pending_scan.insert(key);
                }
            }
        }
        if !scan || self.pending_scan.is_empty() {
            return;
        }
        let keys = std::mem::take(&mut self.pending_scan);
        let inos: HashSet<u64> = keys.iter().map(|k| k.1).collect();
        let mut found: HashMap<LinkKey, Vec<Id>> = HashMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if n.flags & FREE != 0 || n.dir != NONE || i == 0 || !inos.contains(&n.ino) {
                continue;
            }
            let key = (self.dev_of(i as Id), n.ino);
            if keys.contains(&key) {
                found.entry(key).or_default().push(i as Id);
            }
        }
        for (key, mut members) in found {
            members.sort_unstable();
            let charged = members
                .iter()
                .copied()
                .find(|&m| self.nodes[m as usize].flags & (LINKED | CHARGED) == LINKED | CHARGED)
                .unwrap_or(members[0]);
            for &m in &members {
                let f = if m == charged { LINKED | CHARGED } else { LINKED };
                let bytes = self.nodes[m as usize].bytes;
                self.set_file(m, f, bytes);
            }
            self.links.insert(key, members.clone());
            if let Some(st) = members.iter().find_map(|&m| restat(self, m).filter(|st| st.ino == key.1)) {
                self.update_file(members[0], &st);
            }
        }
    }

    /// Charge each hard-link group to its member with the smallest path.
    /// Which link is charged depends on the order entries were seen; tests
    /// call this on both trees before comparing them.
    pub fn normalize_links(&mut self) {
        let groups: Vec<Vec<Id>> = self.links.values().cloned().collect();
        for g in groups {
            let Some(&best) = g.iter().min_by_key(|&&m| self.path(m)) else { continue };
            for m in g {
                let f = if m == best { LINKED | CHARGED } else { LINKED };
                let bytes = self.nodes[m as usize].bytes;
                self.set_file(m, f, bytes);
            }
        }
    }

    // ------------------------------------------------------------ checking

    /// Verify every invariant the incremental updates rely on. For tests.
    pub fn check(&self) -> Result<(), String> {
        let mut seen_links: HashSet<Id> = HashSet::new();
        for (key, g) in &self.links {
            let charged = g.iter().filter(|&&m| self.nodes[m as usize].flags & CHARGED != 0).count();
            if charged != 1 {
                return Err(format!("link group {key:?} has {charged} charged members"));
            }
            for &m in g {
                let n = &self.nodes[m as usize];
                if n.flags & (LINKED | FREE) != LINKED || n.ino != key.1 || self.dev_of(m) != key.0 {
                    return Err(format!("bad link member {}", self.path(m).display()));
                }
                if n.bytes != self.nodes[g[0] as usize].bytes {
                    return Err(format!("link sizes differ at {}", self.path(m).display()));
                }
                seen_links.insert(m);
            }
        }
        if self.has_pending_links() {
            return Err("pending hard links not flushed".into());
        }
        for (h, &id) in &self.handles {
            if self.dir(id).and_then(|d| d.handle.as_deref()) != Some(&**h) {
                return Err(format!("stale handle for {}", self.path(id).display()));
            }
        }
        self.check_dir(ROOT, &seen_links).map(|_| ())
    }

    fn check_dir(&self, id: Id, links: &HashSet<Id>) -> Result<(u64, u64, u64), String> {
        let d = self.dir(id).ok_or("not a dir")?;
        let path = || self.path(id).display().to_string();
        let mut own = self.nodes[id as usize].bytes;
        let (mut total, mut files, mut dirs) = (0, 0, 0);
        for w in d.children.windows(2) {
            if self.nodes[w[0] as usize].name >= self.nodes[w[1] as usize].name {
                return Err(format!("children of {} not sorted", path()));
            }
        }
        for &c in &d.children {
            let n = &self.nodes[c as usize];
            if n.parent != id || n.flags & FREE != 0 {
                return Err(format!("bad child {:?} of {}", n.name(), path()));
            }
            if n.is_dir() {
                let (t, f, s) = self.check_dir(c, links)?;
                total += t;
                files += f;
                dirs += s + 1;
            } else {
                if n.flags & LINKED != 0 && !links.contains(&c) {
                    return Err(format!("{} linked but in no group", self.path(c).display()));
                }
                own += n.contrib();
                files += 1;
            }
        }
        if id == ROOT {
            own = 0;
        }
        total += own;
        if (own, total, files, dirs) != (d.own, d.total, d.files, d.dirs) {
            return Err(format!(
                "{}: stored own/total/files/dirs {:?}, recomputed {:?}",
                path(),
                (d.own, d.total, d.files, d.dirs),
                (own, total, files, dirs)
            ));
        }
        Ok((total, files, dirs))
    }

    /// Differences with another tree, by path. Empty when they index the same
    /// thing. Hard-link charging is compared as is: normalize both first.
    pub fn diff(&self, other: &Tree) -> Vec<String> {
        let mut out = Vec::new();
        self.diff_dir(ROOT, other, ROOT, &mut out);
        out
    }

    fn diff_dir(&self, a: Id, other: &Tree, b: Id, out: &mut Vec<String>) {
        let (da, db) = (self.dir(a).unwrap(), other.dir(b).unwrap());
        let path = self.path(a);
        let (na, nb) = (self.node(a), other.node(b));
        if (na.ino, na.bytes, na.mtime, na.flags & UNREADABLE) != (nb.ino, nb.bytes, nb.mtime, nb.flags & UNREADABLE) {
            out.push(format!("{}: dir ino/bytes/mtime/unreadable {:?} vs {:?}", path.display(), (na.ino, na.bytes, na.mtime, na.flags & UNREADABLE), (nb.ino, nb.bytes, nb.mtime, nb.flags & UNREADABLE)));
        }
        let sa = (da.own, da.total, da.files, da.dirs, da.mode, da.uid, da.gid);
        let sb = (db.own, db.total, db.files, db.dirs, db.mode, db.uid, db.gid);
        if sa != sb {
            out.push(format!("{}: own/total/files/dirs/mode/uid/gid {sa:?} vs {sb:?}", path.display()));
        }
        let (mut i, mut j) = (0, 0);
        while i < da.children.len() || j < db.children.len() {
            let ca = da.children.get(i).map(|&c| (c, &self.nodes[c as usize]));
            let cb = db.children.get(j).map(|&c| (c, &other.nodes[c as usize]));
            match (ca, cb) {
                (Some((x, nx)), Some((y, ny))) if nx.name == ny.name => {
                    if nx.kind != ny.kind {
                        out.push(format!("{}: kind {:?} vs {:?}", self.path(x).display(), nx.kind, ny.kind));
                    } else if nx.is_dir() {
                        self.diff_dir(x, other, y, out);
                    } else {
                        let fa = (nx.ino, nx.bytes, nx.apparent, nx.mtime, nx.contrib());
                        let fb = (ny.ino, ny.bytes, ny.apparent, ny.mtime, ny.contrib());
                        if fa != fb {
                            out.push(format!("{}: ino/bytes/apparent/mtime/charged {fa:?} vs {fb:?}", self.path(x).display()));
                        }
                    }
                    i += 1;
                    j += 1;
                }
                (Some((x, nx)), Some((_, ny))) if nx.name < ny.name => {
                    out.push(format!("{}: only in left", self.path(x).display()));
                    i += 1;
                }
                (Some((x, _)), None) => {
                    out.push(format!("{}: only in left", self.path(x).display()));
                    i += 1;
                }
                (_, Some((y, _))) => {
                    out.push(format!("{}: only in right", other.path(y).display()));
                    j += 1;
                }
                (None, None) => unreachable!(),
            }
            if out.len() > 50 {
                return;
            }
        }
    }

    // ------------------------------------------------------------ raw access
    // Used by snapshots, which rebuild a tree node by node.

    pub(crate) fn raw_flags(&self, id: Id) -> u8 {
        self.nodes[id as usize].flags & (LINKED | CHARGED | UNREADABLE)
    }

    /// Append a node without any bookkeeping; [`Tree::finish_raw`] then
    /// recomputes totals and rebuilds the maps.
    pub(crate) fn push_raw(&mut self, parent: Id, mut node: Node, flags: u8, dir: Option<Dir>) -> Id {
        node.parent = parent;
        node.flags = flags & (LINKED | CHARGED | UNREADABLE);
        node.dir = match dir {
            Some(d) => self.alloc_dir(d),
            None => NONE,
        };
        let id = self.alloc(node);
        self.dir_mut(parent).children.push(id);
        id
    }

    pub(crate) fn finish_raw(&mut self) -> Result<(), String> {
        // Groups from flags.
        self.links.clear();
        self.handles.clear();
        for i in 1..self.nodes.len() {
            let n = &self.nodes[i];
            if n.flags & FREE != 0 {
                continue;
            }
            if n.flags & LINKED != 0 {
                if n.is_dir() {
                    return Err("linked directory".into());
                }
                let key = (self.dev_of(i as Id), n.ino);
                self.links.entry(key).or_default().push(i as Id);
            }
            if let Some(h) = self.dir(i as Id).and_then(|d| d.handle.clone()) {
                self.handles.insert(h, i as Id);
            }
        }
        for g in self.links.values_mut() {
            g.sort_unstable();
        }
        self.recompute(ROOT);
        self.check()
    }

    fn recompute(&mut self, id: Id) -> (u64, u64, u64) {
        let kids = std::mem::take(&mut self.dir_mut(id).children);
        let mut own = if id == ROOT { 0 } else { self.nodes[id as usize].bytes };
        let (mut total, mut files, mut dirs) = (0, 0, 0);
        for &c in &kids {
            if self.nodes[c as usize].is_dir() {
                let (t, f, s) = self.recompute(c);
                total += t;
                files += f;
                dirs += s + 1;
            } else {
                own += self.nodes[c as usize].contrib();
                files += 1;
            }
        }
        total += own;
        let mut kids = kids;
        kids.sort_unstable_by(|&a, &b| self.nodes[a as usize].name.cmp(&self.nodes[b as usize].name));
        let d = self.dir_mut(id);
        d.children = kids;
        (d.own, d.total, d.files, d.dirs) = (own, total, files, dirs);
        (total, files, dirs)
    }

    pub(crate) fn new_node(kind: Kind, name: Box<[u8]>, ino: u64, bytes: u64, apparent: u64, mtime: i64) -> Node {
        Node { parent: NONE, dir: NONE, kind, flags: 0, name, ino, bytes, apparent, mtime }
    }
}
