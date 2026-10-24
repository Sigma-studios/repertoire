//! A world of random file operations, reported as the events fanotify
//! would send. Shared by the event test here and the fanotify test of the
//! daemon (which ignores the events and lets the kernel report them).

use super::*;
use repertoire_core::RawEvent;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct World {
    pub root: PathBuf,
    /// Outside the indexed root: things move in from and out to here.
    pub outside: PathBuf,
    pub rng: Rng,
    pub events: Vec<RawEvent>,
    pub counter: u64,
}

pub fn b(s: &[u8]) -> Box<[u8]> {
    s.into()
}

fn name_of(p: &Path) -> Box<[u8]> {
    use std::os::unix::ffi::OsStrExt;
    b(p.file_name().unwrap().as_bytes())
}

fn h(p: &Path) -> Box<[u8]> {
    handle_of_path(p).unwrap_or_else(|| b(b"gone"))
}

impl World {
    pub fn new(root: PathBuf, outside: PathBuf, seed: u64) -> World {
        World { root, outside, rng: Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1), events: Vec::new(), counter: 0 }
    }

    /// Every directory (root included) and every other entry under `base`
    /// that we can get into.
    pub fn listing(base: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let (mut dirs, mut files) = (vec![base.to_path_buf()], Vec::new());
        let mut i = 0;
        while i < dirs.len() {
            if let Ok(rd) = fs::read_dir(&dirs[i]) {
                for e in rd.flatten() {
                    match e.file_type() {
                        Ok(t) if t.is_dir() => dirs.push(e.path()),
                        _ => files.push(e.path()),
                    }
                }
            }
            i += 1;
        }
        (dirs, files)
    }

    fn accessible(p: &Path) -> bool {
        fs::read_dir(p).is_ok() && fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o700 == 0o700)
    }

    fn fresh_name(&mut self) -> String {
        self.counter += 1;
        // Mixed case and separators exercise ordering; a few are reused.
        let stems = ["data", "Cache", "build-out", "x", "notes.txt", "été", "tmp_1"];
        let s = stems[self.rng.below(stems.len() as u64) as usize];
        if self.rng.chance(15) {
            s.to_string()
        } else {
            format!("{s}{}", self.counter)
        }
    }

    fn ev(&mut self, e: RawEvent) {
        self.events.push(e);
    }

    fn created(&mut self, p: &Path) {
        let e = RawEvent::Created { dir: h(p.parent().unwrap()), name: name_of(p) };
        self.ev(e);
    }

    fn modified(&mut self, p: &Path) {
        let e = RawEvent::Modified { dir: h(p.parent().unwrap()), name: name_of(p), obj: handle_of_entry(p) };
        self.ev(e);
    }

    /// The kernel reports a link count change on the inode itself, by
    /// handle only (link, unlink, rename over). Nothing to report when the
    /// inode is gone with its last link.
    fn link_count(&mut self, p: &Path) {
        use std::os::unix::fs::MetadataExt;
        if fs::symlink_metadata(p).is_ok_and(|m| !m.is_dir() && m.nlink() > 1) {
            let obj = handle_of_entry(p).unwrap();
            self.ev(RawEvent::Inode { obj });
        }
    }

    fn attrib(&mut self, p: &Path) {
        let e = RawEvent::Attrib { dir: h(p.parent().unwrap()), name: name_of(p), obj: handle_of_entry(p) };
        self.ev(e);
    }

    /// `rm -rf` as the kernel reports it: children first, each from its
    /// own parent directory.
    fn remove_tree(&mut self, p: &Path) {
        let md = fs::symlink_metadata(p).unwrap();
        if md.is_dir() {
            if md.permissions().mode() & 0o700 != 0o700 {
                fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
                self.attrib(p);
            }
            let kids: Vec<_> = fs::read_dir(p).unwrap().flatten().map(|e| e.path()).collect();
            for k in kids {
                self.remove_tree(&k);
            }
            let e = RawEvent::Removed { dir: h(p.parent().unwrap()), name: name_of(p) };
            fs::remove_dir(p).unwrap();
            self.ev(e);
        } else {
            let e = RawEvent::Removed { dir: h(p.parent().unwrap()), name: name_of(p) };
            let other = self.other_link(p);
            fs::remove_file(p).unwrap();
            self.ev(e);
            if let Some(o) = other {
                self.link_count(&o);
            }
        }
    }

    fn rename(&mut self, from: &Path, to: &Path) -> bool {
        let (fd, td) = (h(from.parent().unwrap()), h(to.parent().unwrap()));
        let same = |a: &Path, b: &Path| {
            use std::os::unix::fs::MetadataExt;
            matches!((fs::symlink_metadata(a), fs::symlink_metadata(b)), (Ok(x), Ok(y)) if x.ino() == y.ino())
        };
        if same(from, to) {
            return false; // rename between links of one inode does nothing
        }
        let replaced = if to.is_dir() { None } else { self.other_link(to) };
        if fs::rename(from, to).is_err() {
            return false;
        }
        self.ev(RawEvent::Renamed { from: fd, from_name: name_of(from), to: td, to_name: name_of(to) });
        if let Some(o) = replaced {
            self.link_count(&o);
        }
        true
    }

    /// Another path to the same inode as `p`, if it has more links and one
    /// can be found (anywhere in the scratch area).
    fn other_link(&self, p: &Path) -> Option<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        let md = fs::symlink_metadata(p).ok().filter(|m| !m.is_dir() && m.nlink() > 1)?;
        let base = self.root.parent().unwrap();
        let (_, files) = World::listing(base);
        files.into_iter().find(|f| f != p && fs::symlink_metadata(f).is_ok_and(|m| m.ino() == md.ino()))
    }

    fn write_bytes(&mut self, p: &Path, n: usize, append: bool) {
        let existed = fs::symlink_metadata(p).is_ok();
        let mut f = fs::OpenOptions::new().create(true).append(append).write(true).truncate(!append).open(p).unwrap();
        if !existed {
            self.created(p);
        }
        f.write_all(&vec![b'r'; n]).unwrap();
        self.modified(p);
    }

    /// One random operation. Returns a short description for failures.
    pub fn step(&mut self) -> String {
        let (dirs, files) = World::listing(&self.root);
        let dirs: Vec<PathBuf> = dirs.into_iter().filter(|d| World::accessible(d)).collect();
        let files: Vec<PathBuf> = files.into_iter().filter(|f| World::accessible(f.parent().unwrap())).collect();
        let subdirs: Vec<PathBuf> = dirs.iter().filter(|d| **d != self.root).cloned().collect();
        let dir = self.rng.pick(&dirs).unwrap().clone();
        let roll = self.rng.below(100);
        match roll {
            0..=17 => {
                let p = dir.join(self.fresh_name());
                if fs::symlink_metadata(&p).is_ok() {
                    return "skip".into();
                }
                let n = self.rng.below(200_000) as usize;
                self.write_bytes(&p, n, false);
                format!("create {}", p.display())
            }
            18..=29 => {
                let Some(f) = self.rng.pick(&files).cloned() else { return "skip".into() };
                if !fs::symlink_metadata(&f).unwrap().is_file() {
                    return "skip".into();
                }
                let n = self.rng.below(100_000) as usize;
                let append = self.rng.chance(60);
                self.write_bytes(&f, n, append);
                format!("write {}", f.display())
            }
            30..=34 => {
                let Some(f) = self.rng.pick(&files).cloned() else { return "skip".into() };
                if !fs::symlink_metadata(&f).unwrap().is_file() {
                    return "skip".into();
                }
                let file = fs::OpenOptions::new().write(true).open(&f).unwrap();
                if self.rng.chance(50) {
                    file.set_len(self.rng.below(20_000)).unwrap();
                } else {
                    // Sparse growth: a far hole, then a little data.
                    let mut file = file;
                    let at = (1 << 24) + self.rng.below(1 << 24);
                    file.seek(SeekFrom::Start(at)).unwrap();
                    file.write_all(&[7u8; 3000]).unwrap();
                }
                self.modified(&f);
                format!("truncate/sparse {}", f.display())
            }
            35..=44 => {
                let p = dir.join(self.fresh_name());
                if fs::create_dir(&p).is_err() {
                    return "skip".into();
                }
                self.created(&p);
                format!("mkdir {}", p.display())
            }
            45..=54 => {
                // Rename a file, maybe over another, maybe to another folder.
                let Some(f) = self.rng.pick(&files).cloned() else { return "skip".into() };
                let to = if self.rng.chance(30) {
                    match self.rng.pick(&files).cloned() {
                        Some(t) if t != f => t,
                        _ => dir.join(self.fresh_name()),
                    }
                } else {
                    dir.join(self.fresh_name())
                };
                if to.is_dir() || !self.rename(&f, &to) {
                    return "skip".into();
                }
                format!("mv {} {}", f.display(), to.display())
            }
            55..=62 => {
                // Move a folder somewhere else (never into itself).
                let Some(d) = self.rng.pick(&subdirs).cloned() else { return "skip".into() };
                if dir.starts_with(&d) {
                    return "skip".into();
                }
                let to = dir.join(self.fresh_name());
                if !self.rename(&d, &to) {
                    return "skip".into();
                }
                format!("mv dir {} {}", d.display(), to.display())
            }
            63..=69 => {
                let Some(f) = self.rng.pick(&files).cloned() else { return "skip".into() };
                self.remove_tree(&f);
                format!("rm {}", f.display())
            }
            70..=73 => {
                let Some(d) = self.rng.pick(&subdirs).cloned() else { return "skip".into() };
                self.remove_tree(&d);
                format!("rm -r {}", d.display())
            }
            74..=81 => {
                // Hard link, in the same folder or across folders.
                let Some(f) = self.rng.pick(&files).cloned() else { return "skip".into() };
                if !fs::symlink_metadata(&f).unwrap().is_file() {
                    return "skip".into();
                }
                let to = dir.join(self.fresh_name());
                if fs::hard_link(&f, &to).is_err() {
                    return "skip".into();
                }
                self.created(&to);
                self.link_count(&to);
                format!("ln {} {}", f.display(), to.display())
            }
            82..=84 => {
                let to = dir.join(self.fresh_name());
                let target = self.rng.pick(&files).cloned().unwrap_or_else(|| PathBuf::from("/nowhere"));
                if std::os::unix::fs::symlink(&target, &to).is_err() {
                    return "skip".into();
                }
                self.created(&to);
                format!("symlink {}", to.display())
            }
            85..=88 => {
                // Take away (or give back) read/search permission.
                let Some(d) = self.rng.pick(&subdirs).cloned() else { return "skip".into() };
                let mode = [0o000, 0o311, 0o644, 0o700][self.rng.below(4) as usize];
                fs::set_permissions(&d, fs::Permissions::from_mode(mode)).unwrap();
                self.attrib(&d);
                format!("chmod {mode:o} {}", d.display())
            }
            89..=91 => {
                let Some(f) = self.rng.pick(&files).cloned() else { return "skip".into() };
                if !fs::symlink_metadata(&f).unwrap().is_file() {
                    return "skip".into();
                }
                let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + self.rng.below(1 << 28));
                fs::File::options().write(true).open(&f).unwrap().set_modified(t).unwrap();
                self.attrib(&f);
                format!("touch {}", f.display())
            }
            92..=95 => {
                // Move something out of the indexed area.
                let pick = if self.rng.chance(50) { self.rng.pick(&files) } else { self.rng.pick(&subdirs) };
                let Some(p) = pick.cloned() else { return "skip".into() };
                self.counter += 1;
                let to = self.outside.join(format!("out{}", self.counter));
                if !self.rename(&p, &to) {
                    return "skip".into();
                }
                format!("mv out {}", p.display())
            }
            _ => {
                // Move something in from outside (built there first).
                self.counter += 1;
                let src = self.outside.join(format!("in{}", self.counter));
                fs::create_dir_all(src.join("inner/deeper")).unwrap();
                fs::write(src.join("inner/deeper/blob"), vec![1u8; 70_000]).unwrap();
                fs::write(src.join("top"), vec![2u8; 9_000]).unwrap();
                let to = dir.join(self.fresh_name());
                if !self.rename(&src, &to) {
                    return "skip".into();
                }
                format!("mv in {}", to.display())
            }
        }
    }
}

