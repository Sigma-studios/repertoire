//! Queries and the permission filter.

mod common;

use common::*;
use repertoire_core::{Kind, Rank, Sort, Viewer, Visibility};
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn pruned_tree_folds_small_folders() {
    let root = scratch("pruned");
    fs::create_dir_all(root.join("big/inner")).unwrap();
    fs::create_dir_all(root.join("small")).unwrap();
    fs::write(root.join("big/inner/blob"), vec![1u8; 2_000_000]).unwrap();
    fs::write(root.join("big/file"), vec![1u8; 500_000]).unwrap();
    fs::write(root.join("small/file"), vec![1u8; 10_000]).unwrap();
    let t = fresh(&root);
    let v = Viewer::root();
    let above = |_| true;
    let mut vis = Visibility::new(&t, &v, &above);
    let m = t.mounts()[0];
    let p = t.pruned(m, 100_000, 8, &mut vis);
    assert_eq!(p.size, t.size(m));
    assert_eq!(p.children.len(), 1, "only big survives");
    let big = &p.children[0];
    assert_eq!(t.path(big.id), root.join("big"));
    assert_eq!(big.children.len(), 1);
    // Sizes add up at every level.
    fn sums(p: &repertoire_core::Pruned) {
        assert_eq!(p.size, p.own + p.children.iter().map(|c| c.size).sum::<u64>());
        p.children.iter().for_each(sums);
    }
    sums(&p);
    // Depth limit.
    let shallow = t.pruned(m, 1, 1, &mut vis);
    assert!(shallow.children.iter().all(|c| c.children.is_empty()));
    remove_all(&root).unwrap();
}

#[test]
fn top_skips_wrappers_and_never_nests() {
    let root = scratch("top");
    // .local/share/Steam/steamapps/common/{g1,g2,g3}: wrappers down to common.
    for (g, mb) in [("g1", 3), ("g2", 2), ("g3", 1)] {
        let d = root.join(".local/share/Steam/steamapps/common").join(g);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("data"), vec![1u8; mb << 20]).unwrap();
    }
    fs::create_dir_all(root.join("Videos")).unwrap();
    fs::write(root.join("Videos/a.mkv"), vec![1u8; 4 << 20]).unwrap();
    let t = fresh(&root);
    let v = Viewer::root();
    let above = |_| true;
    let mut vis = Visibility::new(&t, &v, &above);
    let top = t.top(t.mounts()[0], 3, &mut vis);
    let names: Vec<_> = top.iter().map(|&i| t.path(i).strip_prefix(&root).unwrap().display().to_string()).collect();
    assert_eq!(names[0], "Videos");
    assert!(names.contains(&".local/share/Steam/steamapps/common/g1".to_string()), "{names:?}");
    for a in &top {
        for b in &top {
            assert!(a == b || !t.is_ancestor(*a, *b), "nested results");
        }
    }
    remove_all(&root).unwrap();
}

#[test]
fn search_ranks_and_filters() {
    let root = scratch("search");
    for p in ["Steam", "steamapps", "my-steam-notes.txt", "mysteamy", "other/SteamLibrary", "other/unrelated"] {
        let path = root.join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if p.ends_with(".txt") || p.ends_with("mysteamy") {
            fs::write(&path, b"x").unwrap();
        } else {
            fs::create_dir_all(&path).unwrap();
        }
    }
    let t = fresh(&root);
    let v = Viewer::root();
    let above = |_| true;
    let mut vis = Visibility::new(&t, &v, &above);
    let hits = t.search("steam", None, &[], 10, &mut vis);
    let got: Vec<_> = hits.iter().map(|&(i, r)| (t.node(i).name().to_string_lossy().into_owned(), r)).collect();
    assert_eq!(got[0], ("Steam".into(), Rank::Exact));
    assert_eq!(got.last().unwrap(), &("mysteamy".to_string(), Rank::Substring));
    assert!(got.contains(&("my-steam-notes.txt".into(), Rank::WordStart)));
    assert_eq!(got.len(), 5);
    // Kinds and scope.
    let dirs = t.search("steam", None, &[Kind::Dir], 10, &mut vis);
    assert_eq!(dirs.len(), 3);
    let other = t.lookup(&root.join("other")).unwrap();
    let under = t.search("steam", Some(other), &[], 10, &mut vis);
    assert_eq!(under.len(), 1);
    // Limit keeps the best.
    let one = t.search("steam", None, &[], 1, &mut vis);
    assert_eq!(t.node(one[0].0).name().to_str(), Some("Steam"));
    // Listing.
    let (d, f) = t.listing(t.mounts()[0], Sort::Name, &mut vis);
    assert_eq!(d.len(), 3);
    assert_eq!(f.len(), 2);
    remove_all(&root).unwrap();
}

#[test]
fn other_users_see_only_what_they_could_list() {
    let root = scratch("perm");
    fs::create_dir_all(root.join("home/alice/private/deep")).unwrap();
    fs::create_dir_all(root.join("home/bob")).unwrap();
    fs::create_dir_all(root.join("traverse-only/known")).unwrap();
    fs::write(root.join("home/alice/private/deep/secret-plans.txt"), b"x").unwrap();
    fs::write(root.join("home/alice/public.txt"), b"x").unwrap();
    fs::write(root.join("traverse-only/known/file"), b"x").unwrap();
    fs::set_permissions(root.join("home/alice/private"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(root.join("traverse-only"), fs::Permissions::from_mode(0o711)).unwrap();
    let mut t = fresh(&root);
    // Pretend the owner is uid 1000 (the scan ran as whoever runs tests).
    let owner = t.dir(t.mounts()[0]).unwrap().uid;
    let stranger = Viewer { uid: owner.wrapping_add(4242), gids: vec![4242] };
    let me = Viewer { uid: owner, gids: vec![] };
    let above = |_| true;

    let mut vis = Visibility::new(&t, &stranger, &above);
    let hits = t.search("secret", None, &[], 10, &mut vis);
    assert!(hits.is_empty(), "stranger found a name inside a 0700 folder");
    let hits = t.search("public", None, &[], 10, &mut vis);
    assert_eq!(hits.len(), 1);
    let private = t.lookup(&root.join("home/alice/private")).unwrap();
    assert!(vis.visible(private), "the 0700 folder itself is listed in its parent");
    let (d, f) = t.listing(private, Sort::Name, &mut vis);
    assert!(d.is_empty() && f.is_empty());
    // 0711: can pass through, can't list.
    let tro = t.lookup(&root.join("traverse-only")).unwrap();
    assert!(vis.reachable(tro));
    assert!(!vis.listable(tro));
    assert!(t.search("known", None, &[], 10, &mut vis).is_empty());
    // Pruned trees fold what the viewer can't see into the parent.
    let p = t.pruned(t.lookup(&root.join("home/alice")).unwrap(), 1, 5, &mut vis);
    let pv = p.children.iter().find(|c| c.id == private).unwrap();
    assert!(pv.children.is_empty());
    // Top doesn't descend into it either.
    let top = t.top(t.mounts()[0], 20, &mut vis);
    assert!(top.iter().all(|&i| !t.is_ancestor(private, i) || i == private));
    drop(vis);

    let mut vis = Visibility::new(&t, &me, &above);
    assert_eq!(t.search("secret", None, &[], 10, &mut vis).len(), 1);
    drop(vis);
    // Directories above the mount point count too.
    let closed = |_| false;
    let mut vis = Visibility::new(&t, &stranger, &closed);
    assert!(t.search("public", None, &[], 10, &mut vis).is_empty());
    drop(vis);
    let _ = &mut t;
    remove_all(&root).unwrap();
}
