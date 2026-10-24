//! M2: snapshots round-trip exactly and corrupt ones are rejected.

mod common;

use common::*;
use repertoire_core::snapshot::{self, Meta};
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn meta(root: &std::path::Path) -> Meta {
    Meta { fs_id: "/dev/test".into(), saved_at: 1_700_000_000, mounts: vec![root.to_path_buf()] }
}

fn build() -> (std::path::PathBuf, repertoire_core::Tree) {
    let root = scratch("snap");
    for d in ["a/b/c", "a/é tė", "x"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    for (i, f) in ["a/f1", "a/b/f2", "a/b/c/f3", "a/é tė/ünï", "x/big"].iter().enumerate() {
        fs::write(root.join(f), vec![0u8; 1000 + i * 70_000]).unwrap();
    }
    fs::hard_link(root.join("x/big"), root.join("a/b/big-link")).unwrap();
    std::os::unix::fs::symlink("a", root.join("ln")).unwrap();
    fs::create_dir(root.join("locked")).unwrap();
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
    let t = fresh(&root);
    (root, t)
}

#[test]
fn round_trip_is_exact() {
    let (root, tree) = build();
    let bytes = snapshot::encode(&tree, &meta(&root));
    let (back, m) = snapshot::decode(&bytes).unwrap();
    assert_eq!(m, meta(&root));
    back.check().unwrap();
    // Same entries, same sizes, same charged hard link, same handles.
    assert!(tree.diff(&back).is_empty(), "{:?}", tree.diff(&back));
    assert!(back.diff(&tree).is_empty());
    for id in tree.ids() {
        let p = tree.path(id);
        let b = back.lookup(&p).unwrap();
        assert_eq!(tree.size(id), back.size(b), "{}", p.display());
        assert_eq!(tree.dir(id).map(|d| d.handle.clone()), back.dir(b).map(|d| d.handle.clone()));
        assert_eq!(tree.is_unreadable(id), back.is_unreadable(b));
    }
    // Encoding is deterministic.
    assert_eq!(snapshot::encode(&back, &meta(&root)), bytes);
    // Through a file, atomically.
    let file = root.join("t.snap");
    snapshot::save(&file, &tree, &meta(&root)).unwrap();
    assert!(!root.join("t.snap.tmp").exists());
    let (from_file, _) = snapshot::load(&file).unwrap();
    assert!(tree.diff(&from_file).is_empty());
    remove_all(&root).unwrap();
}

#[test]
fn corrupt_snapshots_are_rejected() {
    let (root, tree) = build();
    let good = snapshot::encode(&tree, &meta(&root));
    // Every truncation.
    for n in 0..good.len() {
        assert!(snapshot::decode(&good[..n]).is_err(), "truncated at {n} accepted");
    }
    // Every single bit flip (checksum catches them).
    let mut flipped = 0;
    for i in 0..good.len() {
        for bit in [0, 3, 7] {
            let mut bad = good.clone();
            bad[i] ^= 1 << bit;
            assert!(snapshot::decode(&bad).is_err(), "flip at byte {i} bit {bit} accepted");
            flipped += 1;
        }
    }
    assert!(flipped > 100);
    // A different version, even with a valid checksum.
    let mut v2 = good[..good.len() - 8].to_vec();
    v2[16] = 2;
    let sum = fnv(&v2);
    v2.extend_from_slice(&sum.to_le_bytes());
    assert!(snapshot::decode(&v2).err().unwrap().contains("version"));
    // Garbage with a valid checksum: structure checks still reject it.
    let mut junk = good[..20].to_vec();
    junk.extend_from_slice(&[0xff; 64]);
    let sum = fnv(&junk);
    junk.extend_from_slice(&sum.to_le_bytes());
    assert!(snapshot::decode(&junk).is_err());
    remove_all(&root).unwrap();
}

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
