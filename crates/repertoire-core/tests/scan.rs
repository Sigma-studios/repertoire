//! M1: scanner totals against `du` on generated fixtures.

mod common;

use common::*;
use repertoire_core::{Kind, Tree, ROOT};
use std::collections::HashMap;
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(p: &Path, n: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, vec![b'x'; n]).unwrap();
}

/// A sparse file: 64 MiB apparent, a few KiB allocated.
fn sparse(p: &Path) {
    let mut f = fs::File::create(p).unwrap();
    f.set_len(64 << 20).unwrap();
    f.seek(SeekFrom::Start(32 << 20)).unwrap();
    f.write_all(&[1u8; 5000]).unwrap();
}

/// Every directory's size per `du`, in bytes. du reports unreadable
/// directories on stderr and exits 1; its numbers are still what we want.
fn du(root: &Path) -> HashMap<PathBuf, u64> {
    let out = if cfg!(target_os = "linux") {
        Command::new("du").arg("--block-size=1").arg(root).output().unwrap()
    } else {
        Command::new("du").arg("-k").arg(root).output().unwrap()
    };
    let unit = if cfg!(target_os = "linux") { 1 } else { 1024 };
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let (n, p) = l.split_once('\t')?;
            Some((PathBuf::from(p), n.parse::<u64>().ok()? * unit))
        })
        .collect()
}

fn fixture(root: &Path) {
    // Nested folders with a spread of sizes.
    for (i, d) in ["a", "a/b", "a/b/c", "a/b/c/d", "e", "e/f", "g"].iter().enumerate() {
        fs::create_dir_all(root.join(d)).unwrap();
        for j in 0..(i + 2) {
            write(&root.join(d).join(format!("f{j}")), (i * 7919 + j * 104_729) % 300_000);
        }
    }
    write(&root.join("empty-file"), 0);
    fs::create_dir_all(root.join("empty-dir")).unwrap();
    // Sparse file.
    sparse(&root.join("e/sparse.img"));
    // Hard links inside one folder.
    write(&root.join("g/original"), 123_456);
    fs::hard_link(root.join("g/original"), root.join("g/link1")).unwrap();
    fs::hard_link(root.join("g/original"), root.join("g/link2")).unwrap();
    // Symlinks, not followed: one to a big folder, one dangling.
    symlink(root.join("a"), root.join("e/to-a")).unwrap();
    symlink("/nonexistent/target", root.join("e/dangling")).unwrap();
    // A folder we can't read (as non-root).
    write(&root.join("locked/secret"), 50_000);
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    // Readable but not searchable: names listable, entries not stat-able.
    write(&root.join("noexec/inside"), 10_000);
    fs::set_permissions(root.join("noexec"), fs::Permissions::from_mode(0o644)).unwrap();
}

fn scanned(root: &Path) -> Tree {
    let t = fresh(root);
    t.check().unwrap();
    t
}

#[test]
fn totals_match_du_for_every_folder() {
    let root = scratch("du");
    fixture(&root);
    let tree = scanned(&root);
    let du = du(&root);
    let mut checked = 0;
    for (path, &expect) in &du {
        let id = tree.lookup(path).unwrap_or_else(|| panic!("{} not indexed", path.display()));
        assert_eq!(tree.size(id), expect, "size of {}", path.display());
        checked += 1;
    }
    assert!(checked >= 10, "du listed only {checked} folders");
    let mount = tree.mounts()[0];
    assert_eq!(tree.size(mount), du[&root]);

    // Hard links counted once; symlinks not followed.
    let g = tree.lookup(&root.join("g")).unwrap();
    let charged: u64 = tree.children(g).iter().filter(|&&c| tree.node(c).name.starts_with(b"link") || &*tree.node(c).name == b"original").map(|&c| tree.size(c)).sum();
    assert_eq!(charged, tree.node(tree.lookup(&root.join("g/original")).unwrap()).bytes);
    let to_a = tree.lookup(&root.join("e/to-a")).unwrap();
    assert_eq!(tree.node(to_a).kind, Kind::Symlink);
    // Sparse: allocated far below apparent.
    let sp = tree.node(tree.lookup(&root.join("e/sparse.img")).unwrap());
    assert_eq!(sp.apparent, 64 << 20);
    assert!(sp.bytes < 1 << 20, "sparse file charged {} bytes", sp.bytes);
    if !is_root() {
        assert!(tree.is_unreadable(tree.lookup(&root.join("locked")).unwrap()));
        assert!(tree.is_unreadable(tree.lookup(&root.join("noexec")).unwrap()));
    }
    remove_all(&root).unwrap();
}

#[test]
fn hard_links_across_folders_count_once() {
    let root = scratch("links");
    write(&root.join("x/big"), 400_000);
    fs::create_dir_all(root.join("y/z")).unwrap();
    fs::hard_link(root.join("x/big"), root.join("y/z/big-again")).unwrap();
    fs::hard_link(root.join("x/big"), root.join("y/third")).unwrap();
    let tree = scanned(&root);
    let du = du(&root);
    assert_eq!(tree.size(tree.mounts()[0]), du[&root]);
    // Exactly one of the three links is charged.
    let ids: Vec<_> = ["x/big", "y/z/big-again", "y/third"].iter().map(|p| tree.lookup(&root.join(p)).unwrap()).collect();
    let charged = ids.iter().filter(|&&i| tree.size(i) > 0).count();
    assert_eq!(charged, 1);
    remove_all(&root).unwrap();
}

#[test]
fn hard_link_charge_moves_to_a_survivor() {
    use repertoire_core::{ByPath, Ctx, Event};
    let root = scratch("relink");
    write(&root.join("x/big"), 400_000);
    fs::create_dir_all(root.join("y")).unwrap();
    fs::hard_link(root.join("x/big"), root.join("y/big")).unwrap();
    let mut tree = scanned(&root);
    let before = tree.size(tree.mounts()[0]);
    let charged_path = if tree.size(tree.lookup(&root.join("x/big")).unwrap()) > 0 { "x" } else { "y" };
    fs::remove_file(root.join(charged_path).join("big")).unwrap();
    let dir = tree.lookup(&root.join(charged_path)).unwrap();
    let s = scanner();
    let cx = Ctx { scanner: &s, access: &ByPath, handle_path: &|_| None };
    tree.apply(&Event::Removed { dir, name: b"big".to_vec().into() }, &cx);
    tree.flush_links_with(&cx);
    tree.check().unwrap();
    let survivor = tree.lookup(&root.join(if charged_path == "x" { "y/big" } else { "x/big" })).unwrap();
    assert_eq!(tree.size(survivor), tree.node(survivor).bytes);
    assert!(tree.size(survivor) > 0);
    // The file still exists once, so the total barely moves (dir mtimes).
    let after = tree.size(tree.mounts()[0]);
    assert!(before.abs_diff(after) < 64 << 10, "{before} -> {after}");
    assert_matches_fresh(&mut tree, &root, &cx, "after unlink");
    remove_all(&root).unwrap();
}

#[test]
fn mount_root_is_named_by_full_path() {
    let root = scratch("named");
    write(&root.join("sub/file"), 10);
    let tree = scanned(&root);
    assert_eq!(tree.path(tree.mounts()[0]), root);
    assert_eq!(tree.path(tree.lookup(&root.join("sub/file")).unwrap()), root.join("sub/file"));
    assert_eq!(tree.lookup(&root), Some(tree.mounts()[0]));
    assert_eq!(tree.node(ROOT).parent, repertoire_core::NONE);
    remove_all(&root).unwrap();
}
