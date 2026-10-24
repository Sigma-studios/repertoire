//! M2: thousands of random file operations on a real directory, each
//! reported as the events fanotify would send, applied to the tree late and
//! out of step with the operations (modifies are debounced across later
//! renames). After every settle the tree must equal a fresh scan.

mod common;

use common::world::World;
use common::*;
use repertoire_core::debounce::Debouncer;
use repertoire_core::{Ctx, Outcome, RawEvent, Tree};
use std::fs;
use std::time::{Duration, Instant};

fn describe(e: &RawEvent, access: &DiskAccess) -> String {
    let p = |h: &[u8]| access.path_of(h).map_or("?".into(), |p| p.display().to_string());
    let n = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    match e {
        RawEvent::Created { dir, name } => format!("created {}/{}", p(dir), n(name)),
        RawEvent::Removed { dir, name } => format!("removed {}/{}", p(dir), n(name)),
        RawEvent::Modified { dir, name, .. } => format!("modified {}/{}", p(dir), n(name)),
        RawEvent::Attrib { dir, name, .. } => format!("attrib {}/{}", p(dir), n(name)),
        RawEvent::Renamed { from, from_name, to, to_name } => format!("renamed {}/{} -> {}/{}", p(from), n(from_name), p(to), n(to_name)),
        e => format!("{e:?}"),
    }
}

fn run(seed: u64, ops: usize) {
    let base = scratch(&format!("events-{seed}"));
    let root = base.join("indexed");
    let outside = base.join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let mut w = World::new(root.clone(), outside, seed);
    // Start from something non-trivial.
    for _ in 0..40 {
        w.step();
    }
    w.events.clear();
    let mut tree: Tree = fresh(&root);
    let access = DiskAccess::new(vec![base.clone()]);
    let s = scanner();
    let path_of = |hd: &[u8]| access.path_of(hd);
    let cx = Ctx { scanner: &s, access: &access, handle_path: &path_of };

    // Modified events go through a debouncer driven by a fake clock, so
    // some are applied several batches (and renames) after they happened.
    let mut pending: Debouncer<RawEvent> = Debouncer::new(Duration::from_secs(1), Duration::from_secs(5));
    let t0 = Instant::now();
    let mut log: Vec<String> = Vec::new();
    let mut done = 0;
    let mut batch_no = 0u64;
    let mut rescans = 0;
    while done < ops {
        let n = 1 + w.rng.below(25) as usize;
        for _ in 0..n {
            log.push(w.step());
        }
        log.push(format!("-- batch {}", batch_no + 1));
        done += n;
        batch_no += 1;
        let now = t0 + Duration::from_millis(batch_no * 400);
        access.invalidate();
        for e in std::mem::take(&mut w.events) {
            if matches!(e, RawEvent::Modified { .. }) {
                pending.touch(e, now);
            } else {
                let o = tree.apply_raw(&e, &cx);
                if std::env::var_os("REPERTOIRE_TRACE").is_some() {
                    eprintln!("{o:?} {}", describe(&e, &access));
                }
                if o == Outcome::Rescan {
                    rescans += 1;
                    tree = fresh(&root);
                }
            }
        }
        for e in pending.due(now) {
            if tree.apply_raw(&e, &cx) == Outcome::Rescan {
                rescans += 1;
                tree = fresh(&root);
            }
        }
        if w.rng.chance(20) {
            tree.flush_links_with(&cx);
        }
        let every: u64 = std::env::var("REPERTOIRE_CHECK_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(15);
        if batch_no.is_multiple_of(every) || done >= ops {
            for e in pending.drain() {
                if tree.apply_raw(&e, &cx) == Outcome::Rescan {
                    rescans += 1;
                    tree = fresh(&root);
                }
            }
            if let Some(f) = std::env::var_os("REPERTOIRE_OPLOG") {
                fs::write(f, log.join("\n")).unwrap();
            }
            let tail = log[log.len().saturating_sub(30)..].join("\n  ");
            assert_matches_fresh(&mut tree, &root, &cx, &format!("seed {seed}, after {done} ops; last ops:\n  {tail}"));
        }
    }
    eprintln!("seed {seed}: {ops} ops, {batch_no} batches, {rescans} rescans");
    assert!(rescans <= ops / 500, "seed {seed}: {rescans} rescans");
    remove_all(&base).unwrap();
}

#[test]
fn random_operations_match_a_fresh_scan() {
    let ops: usize = std::env::var("REPERTOIRE_OPS").ok().and_then(|v| v.parse().ok()).unwrap_or(3000);
    match std::env::var("REPERTOIRE_SEED").ok().and_then(|v| v.parse().ok()) {
        Some(seed) => run(seed, ops),
        None => {
            for seed in 1..=3 {
                run(seed, ops);
            }
        }
    }
}
