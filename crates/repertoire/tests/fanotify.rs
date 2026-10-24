//! M3/M4 on a real kernel: the daemon watching with fanotify. Linux and
//! root only (filesystem marks need CAP_SYS_ADMIN); skipped otherwise.
#![cfg(target_os = "linux")]

#[path = "../../repertoire-core/tests/common/mod.rs"]
mod common;

use common::world::World;
use common::{fresh, is_root, remove_all, scratch};
use repertoire::{Config, Daemon, Phase};
use repertoire_core::ROOT;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

fn daemon(root: &Path, unlimited_queue: bool) -> Daemon {
    let config = Config {
        roots: vec![root.to_path_buf()],
        state_dir: None,
        socket: None,
        watch: true,
        quiet: Duration::from_millis(200),
        max_delay: Duration::from_secs(1),
        unlimited_queue,
        verbose: true,
        ..Config::default()
    };
    let d = Daemon::start(config).unwrap();
    assert!(d.shared.watching(), "fanotify didn't start");
    assert!(d.wait_idle(Duration::from_millis(300), Duration::from_secs(60)));
    d
}

/// Wait for the events to settle, then compare with a fresh scan.
fn assert_settled_matches(d: &Daemon, root: &Path, context: &str) {
    assert!(d.wait_idle(Duration::from_millis(1500), Duration::from_secs(120)), "{context}: never settled");
    let fs = d.shared.filesystems()[0].clone();
    let mut tree = fs.index.read().unwrap().tree.clone();
    assert_eq!(fs.index.read().unwrap().phase, Phase::Live, "{context}");
    fs.with_ctx(|cx| tree.flush_links_with(cx));
    tree.check().unwrap_or_else(|e| panic!("{context}: {e}"));
    let mut f = fresh(root);
    tree.normalize_links();
    f.normalize_links();
    let diff = tree.diff(&f);
    assert!(diff.is_empty(), "{context}: daemon tree differs from a fresh scan:\n{}", diff.join("\n"));
}

#[test]
fn random_operations_under_the_daemon() {
    if !is_root() {
        eprintln!("skipped: needs root");
        return;
    }
    let ops: usize = std::env::var("REPERTOIRE_OPS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
    for seed in 1..=2u64 {
        let base = scratch(&format!("fan-{seed}"));
        let (root, outside) = (base.join("indexed"), base.join("outside"));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let mut w = World::new(root.clone(), outside, seed);
        for _ in 0..40 {
            w.step();
        }
        let d = daemon(&root, true);
        let start = Instant::now();
        for i in 1..=ops {
            w.step();
            w.events.clear(); // the kernel reports them this time
            if w.rng.chance(2) {
                std::thread::sleep(Duration::from_millis(w.rng.below(300)));
            }
            if i % 700 == 0 {
                assert_settled_matches(&d, &root, &format!("seed {seed}, after {i} ops"));
            }
        }
        assert_settled_matches(&d, &root, &format!("seed {seed}, end"));
        let events = d.shared.activity.events.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("seed {seed}: {ops} ops, {events} kernel events, {:.1?}", start.elapsed());
        d.shutdown();
        remove_all(&base).unwrap();
    }
}

#[test]
fn overflow_rescans_and_recovers() {
    if !is_root() {
        eprintln!("skipped: needs root");
        return;
    }
    let base = scratch("overflow");
    let root = base.join("indexed");
    fs::create_dir_all(root.join("keep")).unwrap();
    fs::write(root.join("keep/file"), vec![1u8; 100_000]).unwrap();
    // Without FAN_UNLIMITED_QUEUE the kernel keeps 16384 events.
    let d = daemon(&root, false);
    d.shared.pause_reader.store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    for i in 0..12_000 {
        let p = root.join(format!("f{i}"));
        fs::write(&p, b"x").unwrap();
        if i % 2 == 0 {
            fs::remove_file(&p).unwrap();
        }
    }
    fs::create_dir_all(root.join("after/deep")).unwrap();
    fs::write(root.join("after/deep/blob"), vec![2u8; 300_000]).unwrap();
    d.shared.pause_reader.store(false, std::sync::atomic::Ordering::Relaxed);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) && d.shared.activity.overflows.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(d.shared.activity.overflows.load(std::sync::atomic::Ordering::Relaxed) > 0, "no overflow seen");
    let fs = d.shared.filesystems()[0].clone();
    assert_settled_matches(&d, &root, "after overflow");
    assert!(fs.index.read().unwrap().tree.lookup(&root.join("after/deep/blob")).is_some());
    assert_eq!(fs.index.read().unwrap().tree.dir(ROOT).unwrap().files, 6_000 + 2);
    d.shutdown();
    remove_all(&base).unwrap();
}

#[test]
fn other_users_cannot_see_names_in_a_private_home() {
    if !is_root() {
        eprintln!("skipped: needs root");
        return;
    }
    use std::os::unix::fs::{chown, PermissionsExt};
    use std::os::unix::process::CommandExt;
    let base = scratch("users");
    let root = base.join("home");
    let alice = root.join("alice");
    fs::create_dir_all(alice.join("Documents")).unwrap();
    fs::write(alice.join("Documents/secret-plans.txt"), b"x").unwrap();
    fs::create_dir_all(root.join("bob")).unwrap();
    fs::write(root.join("bob/public-notes.txt"), b"x").unwrap();
    for p in [&alice, &alice.join("Documents"), &alice.join("Documents/secret-plans.txt")] {
        chown(p, Some(1001), Some(1001)).unwrap();
    }
    fs::set_permissions(&alice, fs::Permissions::from_mode(0o700)).unwrap();
    for p in [&base, &root, &root.join("bob")] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let socket = base.join("s.sock");
    let config = Config { roots: vec![root.clone()], state_dir: None, socket: Some(socket.clone()), ..Config::default() };
    let d = Daemon::start(config).unwrap();
    assert!(d.wait_idle(Duration::from_millis(300), Duration::from_secs(60)));

    // The build directory may not be readable by other users.
    let cli = base.join("repertoire");
    fs::copy(env!("CARGO_BIN_EXE_repertoire"), &cli).unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
    let run_as = |uid: u32, args: &[&str]| -> String {
        let out = std::process::Command::new(&cli)
            .arg("--socket")
            .arg(&socket)
            .args(args)
            .uid(uid)
            .gid(uid)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
    };
    let root_s = root.to_str().unwrap();
    // nobody (65534): bob's file yes, anything inside alice's home no.
    let found = run_as(65534, &["search", "plans"]);
    assert!(!found.contains("secret"), "{found}");
    assert!(run_as(65534, &["search", "notes"]).contains("public-notes.txt"));
    assert!(!run_as(65534, &["search", "Documents"]).contains("Documents"));
    // Can't get into alice's home: same answer as for a path that isn't there.
    let ls = run_as(65534, &["ls", "-a", &format!("{root_s}/alice")]);
    assert!(!ls.contains("Documents") && ls.contains("not indexed (or not visible to you)"), "{ls}");
    let ls = run_as(65534, &["ls", "-a", &format!("{root_s}/alice/Documents")]);
    assert!(!ls.contains("secret"), "{ls}");
    let tree = run_as(65534, &["tree", root_s, "--min", "0"]);
    assert!(tree.contains("alice") && !tree.contains("Documents"), "{tree}");
    let top = run_as(65534, &["top", root_s]);
    assert!(!top.contains("Documents"), "{top}");
    // Alice herself (uid 1001) and root see it.
    assert!(run_as(1001, &["search", "plans"]).contains("secret-plans.txt"));
    assert!(run_as(0, &["search", "plans"]).contains("secret-plans.txt"));
    d.shutdown();
    remove_all(&base).unwrap();
}
