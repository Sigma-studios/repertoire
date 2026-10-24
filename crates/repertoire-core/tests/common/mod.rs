//! Shared test helpers: scratch directories on a real filesystem, a seeded
//! RNG, and a directory access that plays the part of Linux file handles.
#![allow(dead_code)]

pub mod world;

use repertoire_core::sys;
use repertoire_core::{DirAccess, Id, Scanner, Tree, ROOT};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh directory on a real disk filesystem (`/var/tmp` on Linux, which
/// is the root ext4 in the VM and on CI; the user temp dir on macOS).
pub fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::var_os("REPERTOIRE_TEST_DIR").map(PathBuf::from).unwrap_or_else(|| {
        if cfg!(target_os = "linux") {
            PathBuf::from("/var/tmp")
        } else {
            std::env::temp_dir()
        }
    });
    let dir = base.join(format!("repertoire-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = remove_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// `rm -rf` that first restores permissions the tests took away.
pub fn remove_all(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.is_dir() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
            for e in std::fs::read_dir(path)?.flatten() {
                remove_all(&e.path())?;
            }
            return std::fs::remove_dir(path);
        }
        return std::fs::remove_file(path);
    }
    Ok(())
}

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> Option<&'a T> {
        (!v.is_empty()).then(|| &v[self.below(v.len() as u64) as usize])
    }
}

/// Test "file handle". On Linux, the real thing (it carries the inode
/// generation, so a reused inode number gets a new handle). Elsewhere,
/// device and inode, which is fine where inode numbers aren't reused soon
/// (APFS).
pub fn ino_handle(dir: BorrowedFd, name: &[u8]) -> Option<Box<[u8]>> {
    #[cfg(target_os = "linux")]
    {
        linux_handle(dir, name)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let st = if name.is_empty() { sys::stat_fd(dir).ok()? } else { sys::stat_at(dir, name).ok()? };
        let mut h = st.dev.to_le_bytes().to_vec();
        h.extend_from_slice(&st.ino.to_le_bytes());
        Some(h.into())
    }
}

#[cfg(target_os = "linux")]
fn linux_handle(dir: BorrowedFd, name: &[u8]) -> Option<Box<[u8]>> {
    use std::os::fd::AsRawFd;
    let c = std::ffi::CString::new(if name.is_empty() { &b""[..] } else { name }).ok()?;
    let mut buf = [0u8; 8 + 128];
    buf[..4].copy_from_slice(&128u32.to_ne_bytes());
    let mut mount_id: libc::c_int = 0;
    let flags = if name.is_empty() { libc::AT_EMPTY_PATH } else { 0 };
    // SAFETY: buf holds a file_handle header with handle_bytes = 128 and room
    // for that many bytes; the other pointers are valid for the call.
    let r = unsafe {
        libc::syscall(libc::SYS_name_to_handle_at, dir.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr(), &mut mount_id, flags)
    };
    if r != 0 {
        return None;
    }
    let n = u32::from_ne_bytes(buf[..4].try_into().unwrap()) as usize;
    Some(buf[..8 + n].into())
}

pub fn handle_of_path(p: &Path) -> Option<Box<[u8]>> {
    let fd = sys::open_dir(p).ok()?;
    ino_handle(fd.as_fd(), b"")
}

pub fn scanner() -> Scanner {
    Scanner { handle_of: Some(ino_handle), ..Scanner::default() }
}

pub fn fresh(root: &Path) -> Tree {
    let mut t = Tree::new();
    let scan = scanner().scan_root(root).unwrap();
    t.graft(ROOT, scan, false);
    t
}

/// Finds directories by handle the way `open_by_handle_at` does: wherever
/// they are now, by walking the disk. Rebuilt on demand.
#[derive(Default)]
pub struct DiskAccess {
    pub roots: Vec<PathBuf>,
    map: RefCell<HashMap<Box<[u8]>, PathBuf>>,
}

impl DiskAccess {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        DiskAccess { roots, map: RefCell::default() }
    }

    pub fn invalidate(&self) {
        self.map.borrow_mut().clear();
    }

    pub fn path_of(&self, handle: &[u8]) -> Option<PathBuf> {
        if self.map.borrow().is_empty() {
            let mut m = HashMap::new();
            for r in &self.roots {
                walk_dirs(r, &mut m);
            }
            *self.map.borrow_mut() = m;
        }
        self.map.borrow().get(handle).cloned()
    }
}

/// Handle of any entry (files too), as fanotify's object FID.
pub fn handle_of_entry(p: &Path) -> Option<Box<[u8]>> {
    use std::os::unix::ffi::OsStrExt;
    let parent = sys::open_dir(p.parent()?).ok()?;
    ino_handle(parent.as_fd(), p.file_name()?.as_bytes())
}

fn walk_dirs(p: &Path, out: &mut HashMap<Box<[u8]>, PathBuf>) {
    if let Some(h) = handle_of_path(p) {
        out.insert(h, p.to_path_buf());
    }
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                walk_dirs(&e.path(), out);
            } else if let Some(h) = handle_of_entry(&e.path()) {
                out.insert(h, e.path());
            }
        }
    }
}

impl DirAccess for DiskAccess {
    fn open(&self, tree: &Tree, dir: Id) -> std::io::Result<OwnedFd> {
        let not_found = || std::io::Error::from(std::io::ErrorKind::NotFound);
        let h = tree.dir(dir).and_then(|d| d.handle.clone()).ok_or_else(not_found)?;
        let p = self.path_of(&h).ok_or_else(not_found)?;
        let fd = sys::open_dir(&p)?;
        if sys::stat_fd(fd.as_fd())?.ino != tree.node(dir).ino {
            return Err(not_found());
        }
        Ok(fd)
    }

    fn locate(&self, tree: &Tree, dir: Id) -> Option<PathBuf> {
        let h = tree.dir(dir)?.handle.clone()?;
        self.path_of(&h)
    }

    fn find_handle(&self, handle: &[u8]) -> Option<(sys::Stat, Option<PathBuf>)> {
        let p = self.path_of(handle)?;
        let st = sys::lstat(&p).ok()?;
        (handle_of_entry(&p).as_deref() == Some(handle)).then_some((st, Some(p)))
    }
}

/// Compare a tree maintained by events with a fresh scan of the same root.
pub fn assert_matches_fresh(tree: &mut Tree, root: &Path, cx: &repertoire_core::Ctx, context: &str) {
    tree.flush_links_with(cx);
    if let Err(e) = tree.check() {
        panic!("{context}: invariants broken: {e}");
    }
    let mut f = fresh(root);
    f.check().unwrap();
    tree.normalize_links();
    f.normalize_links();
    tree.check().unwrap();
    let diff = tree.diff(&f);
    assert!(diff.is_empty(), "{context}: tree differs from a fresh scan:\n{}", diff.join("\n"));
}

pub fn is_root() -> bool {
    sys::euid() == 0
}
