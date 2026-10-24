//! Saving a tree to disk and loading it back, so a restart can serve numbers
//! right away while the reconcile scan runs.
//!
//! Format (all integers LEB128 varints unless noted):
//!
//! ```text
//! magic    "REPERTOIRE-SNAP\n"      16 bytes
//! version  u32 little endian
//! meta     fs id (bytes), saved_at (zigzag), mount count, mount paths (bytes)
//! nodes    pre-order from the virtual root's children:
//!            kind u8, flags u8, name, ino, bytes, apparent, mtime (zigzag)
//!            directories add: mode, uid, gid, dev, handle (len, 0 = none),
//!            child count, then the children
//! checksum u64 little endian, FNV-1a of everything before it
//! ```
//!
//! Totals aren't stored: they're recomputed on load, which also checks
//! every invariant. Anything that doesn't parse, doesn't check out, or has a
//! different version is rejected, never half-trusted.

use crate::tree::{Dir, Id, Kind, Tree, ROOT};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 16] = b"REPERTOIRE-SNAP\n";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    pub fs_id: String,
    /// Unix seconds.
    pub saved_at: i64,
    pub mounts: Vec<PathBuf>,
}

pub fn encode(tree: &Tree, meta: &Meta) -> Vec<u8> {
    let mut w = Vec::with_capacity(tree.len() * 40 + 64);
    w.extend_from_slice(MAGIC);
    w.extend_from_slice(&VERSION.to_le_bytes());
    put_bytes(&mut w, meta.fs_id.as_bytes());
    put(&mut w, zigzag(meta.saved_at));
    put(&mut w, meta.mounts.len() as u64);
    for m in &meta.mounts {
        put_bytes(&mut w, m.as_os_str().as_bytes());
    }
    put(&mut w, tree.mounts().len() as u64);
    // Explicit stack: trees can be deep.
    let mut stack: Vec<Id> = tree.mounts().iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        let n = tree.node(id);
        w.push(match n.kind {
            Kind::Dir => 0,
            Kind::File => 1,
            Kind::Symlink => 2,
            Kind::Other => 3,
        });
        w.push(tree.raw_flags(id));
        put_bytes(&mut w, &n.name);
        put(&mut w, n.ino);
        put(&mut w, n.bytes);
        put(&mut w, n.apparent);
        put(&mut w, zigzag(n.mtime));
        if let Some(d) = tree.dir(id) {
            put(&mut w, d.mode as u64);
            put(&mut w, d.uid as u64);
            put(&mut w, d.gid as u64);
            put(&mut w, d.dev);
            put_bytes(&mut w, d.handle.as_deref().unwrap_or(&[]));
            put(&mut w, d.children.len() as u64);
            stack.extend(d.children.iter().rev());
        }
    }
    let sum = fnv1a(&w);
    w.extend_from_slice(&sum.to_le_bytes());
    w
}

pub fn decode(data: &[u8]) -> Result<(Tree, Meta), String> {
    if data.len() < MAGIC.len() + 4 + 8 || &data[..16] != MAGIC {
        return Err("not a snapshot".into());
    }
    let (body, sum) = data.split_at(data.len() - 8);
    if fnv1a(body) != u64::from_le_bytes(sum.try_into().unwrap()) {
        return Err("checksum mismatch".into());
    }
    let version = u32::from_le_bytes(body[16..20].try_into().unwrap());
    if version != VERSION {
        return Err(format!("version {version}, expected {VERSION}"));
    }
    let mut r = Reader { b: body, at: 20 };
    let fs_id = String::from_utf8(r.bytes()?.to_vec()).map_err(|_| "bad fs id")?;
    let saved_at = unzigzag(r.u()?);
    let n = r.u()?;
    let mut mounts = Vec::new();
    for _ in 0..n {
        mounts.push(PathBuf::from(std::ffi::OsStr::from_bytes(r.bytes()?)));
    }

    let mut tree = Tree::new();
    // (parent, children still to read)
    let mut stack: Vec<(Id, u64)> = vec![(ROOT, r.u()?)];
    while let Some(top) = stack.last_mut() {
        if top.1 == 0 {
            stack.pop();
            continue;
        }
        top.1 -= 1;
        let parent = top.0;
        let kind = match r.byte()? {
            0 => Kind::Dir,
            1 => Kind::File,
            2 => Kind::Symlink,
            3 => Kind::Other,
            k => return Err(format!("bad kind {k}")),
        };
        let flags = r.byte()?;
        let name: Box<[u8]> = r.bytes()?.into();
        if name.is_empty() || (parent != ROOT && name.contains(&b'/')) {
            return Err("bad name".into());
        }
        let node = Tree::new_node(kind, name, r.u()?, r.u()?, r.u()?, unzigzag(r.u()?));
        if kind == Kind::Dir {
            let dir = Dir {
                mode: r.u32()?,
                uid: r.u32()?,
                gid: r.u32()?,
                dev: r.u()?,
                handle: Some(r.bytes()?).filter(|h| !h.is_empty()).map(Into::into),
                ..Dir::default()
            };
            let children = r.u()?;
            let id = tree.push_raw(parent, node, flags, Some(dir));
            stack.push((id, children));
        } else {
            if parent == ROOT {
                return Err("file at the root".into());
            }
            tree.push_raw(parent, node, flags, None);
        }
    }
    if r.at != body.len() {
        return Err("trailing data".into());
    }
    tree.finish_raw()?;
    Ok((tree, Meta { fs_id, saved_at, mounts }))
}

/// Write atomically: a temporary file in the same directory, synced, then
/// renamed over the old snapshot.
pub fn save(path: &Path, tree: &Tree, meta: &Meta) -> io::Result<()> {
    let data = encode(tree, meta);
    let tmp = path.with_extension("snap.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn load(path: &Path) -> Result<(Tree, Meta), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    decode(&data)
}

fn put(w: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            w.push(b);
            return;
        }
        w.push(b | 0x80);
    }
}

fn put_bytes(w: &mut Vec<u8>, b: &[u8]) {
    put(w, b.len() as u64);
    w.extend_from_slice(b);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.at).ok_or("truncated")?;
        self.at += 1;
        Ok(v)
    }

    fn u(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("bad varint".into())
    }

    fn u32(&mut self) -> Result<u32, String> {
        u32::try_from(self.u()?).map_err(|_| "value out of range".into())
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = usize::try_from(self.u()?).map_err(|_| "length out of range")?;
        let end = self.at.checked_add(n).filter(|&e| e <= self.b.len()).ok_or("truncated")?;
        let s = &self.b[self.at..end];
        self.at = end;
        Ok(s)
    }
}
