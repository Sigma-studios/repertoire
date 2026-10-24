//! Read-only questions asked of a tree: pruned size trees, listings, the
//! largest folders, and name search. All of them filter through a
//! [`Visibility`] so non-root clients only see what they could see anyway.

use crate::access::Visibility;
use crate::tree::{Id, Kind, Tree};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// A folder of a pruned tree. Folders below the threshold are folded into
/// their parent's `own`, so `size == own + sum(children sizes)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pruned {
    pub id: Id,
    pub size: u64,
    pub own: u64,
    pub children: Vec<Pruned>,
}

impl Tree {
    /// Folders of at least `min_bytes`, at most `max_depth` levels below `id`.
    pub fn pruned(&self, id: Id, min_bytes: u64, max_depth: u32, vis: &mut Visibility) -> Pruned {
        let size = self.size(id);
        let mut node = Pruned { id, size, own: size, children: Vec::new() };
        if max_depth == 0 || !vis.listable(id) {
            return node;
        }
        for &c in self.children(id) {
            if self.node(c).is_dir() && self.size(c) >= min_bytes.max(1) {
                let child = self.pruned(c, min_bytes, max_depth - 1, vis);
                node.own -= child.size;
                node.children.push(child);
            }
        }
        node.children.sort_by_key(|c| Reverse(c.size));
        node
    }

    /// Children of `id` split into directories and other entries, each
    /// sorted. Entries the viewer can't see are left out.
    pub fn listing(&self, id: Id, sort: Sort, vis: &mut Visibility) -> (Vec<Id>, Vec<Id>) {
        if !vis.listable(id) {
            return (Vec::new(), Vec::new());
        }
        let (mut dirs, mut files): (Vec<Id>, Vec<Id>) = self.children(id).iter().partition(|&&c| self.node(c).is_dir());
        for list in [&mut dirs, &mut files] {
            match sort {
                Sort::Name => {} // children are kept sorted by name
                Sort::Size => list.sort_by_key(|&c| Reverse(self.size(c))),
                Sort::Mtime => list.sort_by_key(|&c| Reverse(self.node(c).mtime)),
            }
        }
        (dirs, files)
    }

    /// The `n` largest folders under `id` that don't contain one another.
    ///
    /// Starting from the children of `id`: a folder whose largest subfolder
    /// holds at least half of it is just a wrapper, so it's replaced by its
    /// subfolders; then, while fewer than `n` folders are found, the largest
    /// one that has subfolders is split into them. What a split folder holds
    /// directly (its files) isn't a folder and isn't listed.
    pub fn top(&self, id: Id, n: usize, vis: &mut Visibility) -> Vec<Id> {
        let subdirs = |vis: &mut Visibility, d: Id| -> Vec<Id> {
            if !vis.listable(d) {
                return Vec::new();
            }
            self.children(d).iter().copied().filter(|&c| self.node(c).is_dir() && self.size(c) > 0).collect()
        };
        let mut frontier: Vec<Id> = Vec::new();
        let mut todo = subdirs(vis, id);
        // Unwrap wrappers.
        while let Some(d) = todo.pop() {
            let kids = subdirs(vis, d);
            let biggest = kids.iter().map(|&c| self.size(c)).max().unwrap_or(0);
            if biggest > 0 && biggest * 2 >= self.size(d) {
                todo.extend(kids);
            } else {
                frontier.push(d);
            }
        }
        // Split the largest until there are enough.
        let mut leaves = Vec::new();
        while frontier.len() + leaves.len() < n && !frontier.is_empty() {
            let (i, _) = frontier.iter().enumerate().max_by_key(|(_, &d)| self.size(d)).unwrap();
            let d = frontier.swap_remove(i);
            let kids = subdirs(vis, d);
            if kids.is_empty() {
                leaves.push(d);
            } else {
                let mut todo = kids;
                while let Some(k) = todo.pop() {
                    let gk = subdirs(vis, k);
                    let biggest = gk.iter().map(|&c| self.size(c)).max().unwrap_or(0);
                    if biggest > 0 && biggest * 2 >= self.size(k) {
                        todo.extend(gk);
                    } else {
                        frontier.push(k);
                    }
                }
            }
        }
        frontier.extend(leaves);
        frontier.sort_by_key(|&d| Reverse(self.size(d)));
        frontier.truncate(n);
        frontier
    }

    /// Entries whose name matches `query`, best first: exact name, then
    /// prefix, then a match at a word start, then any substring. Ties go to
    /// shorter names, then larger sizes. Case-insensitive.
    pub fn search(&self, query: &str, under: Option<Id>, kinds: &[Kind], limit: usize, vis: &mut Visibility) -> Vec<(Id, Rank)> {
        let q = Query::new(query);
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        // Max-heap of the worst kept result, so it can be evicted.
        let mut best: BinaryHeap<(Rank, usize, Reverse<u64>, Id)> = BinaryHeap::new();
        let mut consider = |id: Id, vis: &mut Visibility| {
            let n = self.node(id);
            if !kinds.is_empty() && !kinds.contains(&n.kind) {
                return;
            }
            let Some(rank) = q.rank(&n.name) else { return };
            let key = (rank, n.name.len(), Reverse(self.size(id)), id);
            if best.len() >= limit && best.peek().is_some_and(|w| key >= *w) {
                return;
            }
            if !vis.visible(id) {
                return;
            }
            best.push(key);
            if best.len() > limit {
                best.pop();
            }
        };
        match under {
            Some(root) => {
                let mut stack: Vec<Id> = self.children(root).to_vec();
                while let Some(id) = stack.pop() {
                    consider(id, vis);
                    stack.extend_from_slice(self.children(id));
                }
            }
            None => {
                for id in self.ids() {
                    if self.node(id).parent != crate::tree::ROOT {
                        consider(id, vis);
                    }
                }
            }
        }
        let mut out: Vec<_> = best.into_vec();
        out.sort();
        out.into_iter().map(|(rank, _, _, id)| (id, rank)).collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Name,
    /// Largest first.
    Size,
    /// Newest first.
    Mtime,
}

/// How well a name matches; lower is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    Exact,
    Prefix,
    WordStart,
    Substring,
}

impl Rank {
    pub fn as_str(self) -> &'static str {
        match self {
            Rank::Exact => "exact",
            Rank::Prefix => "prefix",
            Rank::WordStart => "word",
            Rank::Substring => "substring",
        }
    }
}

/// How well `name` matches `query` (case-insensitive), the way
/// [`Tree::search`] ranks it. `None` when it doesn't match. For clients that
/// search without the daemon and want the same order.
pub fn rank(query: &str, name: &[u8]) -> Option<Rank> {
    let q = Query::new(query);
    if q.is_empty() {
        return None;
    }
    q.rank(name)
}

/// A lowercased query. ASCII is folded byte by byte, which is what makes a
/// linear scan over every name fast; non-ASCII names are folded through
/// `str::to_lowercase` only when the query itself isn't ASCII.
struct Query {
    q: Vec<u8>,
    ascii: bool,
}

impl Query {
    fn new(s: &str) -> Query {
        Query { q: s.to_lowercase().into_bytes(), ascii: s.is_ascii() }
    }

    fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    fn rank(&self, name: &[u8]) -> Option<Rank> {
        if self.ascii {
            rank_folded(name, &self.q, |b| b.to_ascii_lowercase())
        } else {
            let lower = String::from_utf8_lossy(name).to_lowercase();
            // Word starts are judged on the original, which lowercasing
            // can shift for non-ASCII; good enough for ranking.
            rank_folded(lower.as_bytes(), &self.q, |b| b)
        }
    }
}

fn rank_folded(name: &[u8], q: &[u8], fold: impl Fn(u8) -> u8) -> Option<Rank> {
    if q.len() > name.len() {
        return None;
    }
    let mut found = false;
    let first = q[0];
    for p in 0..=name.len() - q.len() {
        if fold(name[p]) != first || !name[p + 1..p + q.len()].iter().zip(&q[1..]).all(|(&a, &b)| fold(a) == b) {
            continue;
        }
        if p == 0 {
            return Some(if q.len() == name.len() { Rank::Exact } else { Rank::Prefix });
        }
        if word_start(name, p) {
            return Some(Rank::WordStart);
        }
        found = true;
    }
    found.then_some(Rank::Substring)
}

/// After a separator, or a lower-to-upper case change (`camelCase`).
fn word_start(name: &[u8], p: usize) -> bool {
    let (a, b) = (name[p - 1], name[p]);
    !a.is_ascii_alphanumeric() && a < 0x80 || (a.is_ascii_lowercase() && b.is_ascii_uppercase()) || (a.is_ascii_alphabetic() && b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, q: &str) -> Option<Rank> {
        Query::new(q).rank(name.as_bytes())
    }

    #[test]
    fn public_rank_matches() {
        assert_eq!(rank("steam", b"steamapps"), Some(Rank::Prefix));
        assert_eq!(rank("", b"x"), None);
    }

    #[test]
    fn ranks() {
        assert_eq!(r("Steam", "steam"), Some(Rank::Exact));
        assert_eq!(r("steamapps", "steam"), Some(Rank::Prefix));
        assert_eq!(r("my-steam-games", "steam"), Some(Rank::WordStart));
        assert_eq!(r("MySteam", "steam"), Some(Rank::WordStart));
        assert_eq!(r("mysteam", "steam"), Some(Rank::Substring));
        assert_eq!(r("mysteam_steam", "steam"), Some(Rank::WordStart));
        assert_eq!(r("stea", "steam"), None);
        assert_eq!(r("Été 2024.mp4", "été"), Some(Rank::Prefix));
        assert_eq!(r("photos-ÉTÉ", "été"), Some(Rank::WordStart));
    }
}
