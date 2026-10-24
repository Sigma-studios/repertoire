//! Who may see what. The daemon indexes as root; a client only gets the
//! entries it could find itself with `ls`/`find`: every ancestor directory
//! searchable (`x`) and the parent directory readable (`r`).
//!
//! ACLs, capabilities and LSMs are ignored: only owner/group/mode bits count.

use crate::tree::{Id, Tree, NONE, ROOT};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Viewer {
    pub uid: u32,
    /// Primary and supplementary groups.
    pub gids: Vec<u32>,
}

impl Viewer {
    pub fn root() -> Viewer {
        Viewer { uid: 0, gids: vec![0] }
    }

    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    /// The rwx bits that apply to this viewer, as 0..=7.
    pub fn bits(&self, mode: u32, uid: u32, gid: u32) -> u32 {
        if self.is_root() {
            7
        } else if uid == self.uid {
            (mode >> 6) & 7
        } else if self.gids.contains(&gid) {
            (mode >> 3) & 7
        } else {
            mode & 7
        }
    }
}

const R: u32 = 4;
const X: u32 = 1;

/// Visibility checks for one query, memoized per directory.
pub struct Visibility<'a> {
    tree: &'a Tree,
    viewer: &'a Viewer,
    /// Whether the directories above each mount point (in other
    /// filesystems) let the viewer through.
    above: &'a dyn Fn(Id) -> bool,
    reachable: HashMap<Id, bool>,
}

impl<'a> Visibility<'a> {
    pub fn new(tree: &'a Tree, viewer: &'a Viewer, above: &'a dyn Fn(Id) -> bool) -> Self {
        Visibility { tree, viewer, above, reachable: HashMap::new() }
    }

    pub fn all(&self) -> bool {
        self.viewer.is_root()
    }

    fn bits(&self, dir: Id) -> u32 {
        self.tree.dir(dir).map_or(0, |d| self.viewer.bits(d.mode, d.uid, d.gid))
    }

    /// The viewer can get into `dir`: it and every ancestor are searchable.
    pub fn reachable(&mut self, dir: Id) -> bool {
        if self.all() {
            return true;
        }
        if let Some(&v) = self.reachable.get(&dir) {
            return v;
        }
        // Walk up until a cached answer, then fill the chain in.
        let mut chain = Vec::new();
        let mut id = dir;
        let mut ok = loop {
            if let Some(&v) = self.reachable.get(&id) {
                break v;
            }
            chain.push(id);
            let parent = self.tree.node(id).parent;
            if parent == ROOT || parent == NONE {
                break (self.above)(id);
            }
            id = parent;
        };
        for &d in chain.iter().rev() {
            ok = ok && self.bits(d) & X != 0;
            self.reachable.insert(d, ok);
        }
        ok
    }

    /// The viewer can list `dir`.
    pub fn listable(&mut self, dir: Id) -> bool {
        self.all() || (self.reachable(dir) && self.bits(dir) & R != 0)
    }

    /// The viewer can see the entry `id` (its name and size).
    pub fn visible(&mut self, id: Id) -> bool {
        if self.all() {
            return true;
        }
        let parent = self.tree.node(id).parent;
        if parent == ROOT {
            return (self.above)(id);
        }
        self.listable(parent)
    }
}
