//! M4: the daemon over its socket, every request through the client and
//! the CLI. Runs everywhere, unprivileged (scan-only mode).

#[path = "../../repertoire-core/tests/common/mod.rs"]
mod common;

use common::{remove_all, scratch};
use repertoire::{Config, Daemon};
use repertoire_proto::{Client, FsState, Kind, Rank, Response, Sort};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn fixture(root: &Path) {
    for (p, n) in [
        ("Games/steamapps/common/Big Game/data.pak", 3_000_000),
        ("Games/steamapps/common/Small/data.pak", 200_000),
        ("Videos/holiday.mkv", 2_000_000),
        ("projects/repertoire/target/debug/app", 900_000),
        ("projects/repertoire/src/main.rs", 4_000),
        ("notes/steam-keys.txt", 100),
    ] {
        let path = root.join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![7u8; n]).unwrap();
    }
}

fn start(root: &Path, state: &Path, socket: &Path) -> Daemon {
    let config = Config {
        roots: vec![root.to_path_buf()],
        state_dir: Some(state.to_path_buf()),
        socket: Some(socket.to_path_buf()),
        watch: false,
        ..Config::default()
    };
    let d = Daemon::start(config).unwrap();
    assert!(d.wait_idle(Duration::from_millis(50), Duration::from_secs(30)), "first scan didn't finish");
    d
}

fn cli(socket: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_repertoire")).arg("--socket").arg(socket).args(args).output().unwrap();
    assert!(out.status.success(), "repertoire {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn every_request_answers() {
    let base = scratch("daemon");
    let root = base.join("root");
    fixture(&root);
    let socket = base.join("s.sock");
    let daemon = start(&root, &base.join("state"), &socket);
    let mut c = Client::connect_to(&socket).unwrap();

    let hello = c.hello("test").unwrap();
    assert_eq!(hello.protocol, 1);
    assert!(hello.capabilities.contains(&"search".to_string()));

    let fss = c.filesystems().unwrap();
    assert_eq!(fss.len(), 1);
    let fs = &fss[0];
    assert_eq!(fs.mount_points[0].path.to_path(), root);
    assert_eq!(fs.state, FsState::Static);
    assert!(fs.indexed >= 6_000_000, "indexed {}", fs.indexed);
    assert_eq!(fs.files, 6);
    assert!(fs.last_full_scan.is_some());

    let t = c.tree(&root, 1_000_000, 10).unwrap();
    assert_eq!(t.size, fs.indexed);
    let names: Vec<PathBuf> = t.children.iter().map(|c| c.path.to_path()).collect();
    assert_eq!(names[0], root.join("Games"), "largest first");
    assert!(names.contains(&root.join("Videos")));
    assert!(!names.contains(&root.join("notes")), "small folders fold into their parent");
    fn sums(n: &repertoire_proto::TreeNode) {
        assert_eq!(n.size, n.own + n.children.iter().map(|c| c.size).sum::<u64>());
        n.children.iter().for_each(sums);
    }
    sums(&t);

    let l = c.children(&root.join("Videos"), true, Sort::Size, 100, 0).unwrap();
    assert_eq!(l.files.len(), 1);
    assert_eq!(l.files[0].name.lossy(), "holiday.mkv");
    assert_eq!(l.files[0].kind, Kind::File);
    assert!(l.files[0].bytes >= 2_000_000);
    let l = c.children(&root, false, Sort::Name, 2, 1).unwrap();
    assert_eq!(l.dir_count, 4);
    assert_eq!(l.dirs.iter().map(|d| d.name.lossy()).collect::<Vec<_>>(), ["Videos", "notes"]);

    let top = c.top(&root, 3).unwrap();
    assert_eq!(top[0].path.to_path(), root.join("Games/steamapps/common/Big Game"));
    assert_eq!(top.len(), 3);

    let hits = c.search("steam", None, 10, &[]).unwrap();
    assert_eq!(hits[0].path.to_path(), root.join("Games/steamapps"));
    assert_eq!(hits[0].rank, Rank::Prefix);
    assert!(hits.iter().any(|h| h.path.to_path() == root.join("notes/steam-keys.txt") && h.rank == Rank::Prefix));
    let files = c.search("data", Some(&root.join("Games")), 10, &[Kind::File]).unwrap();
    assert_eq!(files.len(), 2);
    assert!(files[0].bytes > files[1].bytes, "same rank and length: larger first");

    // Errors are answers, not disconnects.
    match c.tree(Path::new("/definitely/not/indexed"), 0, 1) {
        Err(repertoire_proto::Error::Daemon { code, .. }) => assert_eq!(code, repertoire_proto::ErrorCode::NotFound),
        r => panic!("{r:?}"),
    }
    match c.tree(Path::new("relative"), 0, 1) {
        Err(repertoire_proto::Error::Daemon { code, .. }) => assert_eq!(code, repertoire_proto::ErrorCode::BadRequest),
        r => panic!("{r:?}"),
    }
    assert!(c.hello("still connected").is_ok());

    // Rescan picks up a change (no watching in this mode).
    fs::write(root.join("Videos/new.mkv"), vec![1u8; 1_500_000]).unwrap();
    let started = c.rescan(None).unwrap();
    assert_eq!(started, vec![fs.id.clone()]);
    std::thread::sleep(Duration::from_millis(100));
    assert!(daemon.wait_idle(Duration::from_millis(50), Duration::from_secs(30)));
    let l = c.children(&root.join("Videos"), true, Sort::Size, 100, 0).unwrap();
    assert_eq!(l.files[0].name.lossy(), "holiday.mkv");
    assert_eq!(l.files.len(), 2);

    // Subscribe: the current totals and states come first.
    let sub = Client::connect_to(&socket).unwrap().subscribe(&[&root, &root.join("Videos")]).unwrap();
    sub.set_timeout(Some(Duration::from_secs(5))).unwrap();
    let first: Vec<Response> = sub.take(3).map(|r| r.unwrap()).collect();
    assert!(first.iter().any(|r| matches!(r, Response::Changed { path, .. } if path.to_path() == root)));
    assert!(first.iter().any(|r| matches!(r, Response::FsState { state: FsState::Static, .. })));

    // A raw, malformed line gets an error line back.
    {
        use std::io::{BufRead, BufReader, Write};
        let mut s = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        s.write_all(b"{\"v\":1,\"type\":\"Nope\"}\n{\"v\":9,\"type\":\"Hello\"}\n").unwrap();
        let mut r = BufReader::new(s);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.contains("bad_request"), "{line}");
        line.clear();
        r.read_line(&mut line).unwrap();
        assert!(line.contains("\"version\""), "{line}");
    }

    // The CLI, one command per request.
    let s = &socket;
    let root_s = root.to_str().unwrap();
    assert!(cli(s, &["status"]).contains("scanned (not watched)"));
    assert!(cli(s, &["tree", root_s, "--min", "1M"]).contains("Games"));
    assert!(cli(s, &["top", root_s, "-n", "2"]).contains("Big Game"));
    assert!(cli(s, &["ls", &format!("{root_s}/Videos"), "-a"]).contains("holiday.mkv"));
    let found = cli(s, &["search", "steam", "--kind", "dir"]);
    assert!(found.contains("steamapps/") && !found.contains("steam-keys"), "{found}");
    assert!(cli(s, &["rescan"]).contains("rescanning"));
    assert!(cli(s, &["--json", "status"]).contains("\"type\":\"Filesystems\""));
    let mut watch = Command::new(env!("CARGO_BIN_EXE_repertoire"))
        .args(["--socket", s.to_str().unwrap(), "watch", root_s])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(watch.stdout.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut out, &mut line).unwrap();
    assert!(line.contains(root_s) || line.contains(":"), "{line}");
    watch.kill().unwrap();
    let _ = watch.wait();

    daemon.shutdown();
    remove_all(&base).unwrap();
}

#[test]
fn restart_serves_the_snapshot_first() {
    let base = scratch("restart");
    let root = base.join("root");
    fixture(&root);
    let (state, socket) = (base.join("state"), base.join("s.sock"));
    let d = start(&root, &state, &socket);
    let before = Client::connect_to(&socket).unwrap().filesystems().unwrap()[0].indexed;
    d.shutdown();
    let snaps: Vec<_> = fs::read_dir(&state).unwrap().flatten().collect();
    assert_eq!(snaps.len(), 1, "one snapshot per filesystem");

    // Restart on the snapshot, without letting the reconcile scan run first.
    let config = Config {
        roots: vec![root.clone()],
        state_dir: Some(state.clone()),
        socket: None,
        watch: false,
        idle_threads: 1,
        ..Config::default()
    };
    let d2 = Daemon::start(config).unwrap();
    let fs = d2.shared.filesystems()[0].clone();
    {
        let ix = fs.index.read().unwrap();
        assert!(matches!(ix.phase, repertoire::Phase::Reconciling | repertoire::Phase::Static));
        assert_eq!(ix.tree.size(repertoire_core::ROOT), before, "snapshot served as is");
    }
    assert!(d2.wait_idle(Duration::from_millis(50), Duration::from_secs(30)));
    assert_eq!(fs.index.read().unwrap().phase, repertoire::Phase::Static);
    d2.shutdown();

    // A corrupt snapshot is discarded, not trusted.
    let snap = fs::read_dir(&state).unwrap().flatten().next().unwrap().path();
    let mut bytes = fs::read(&snap).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x55;
    fs::write(&snap, bytes).unwrap();
    let config = Config { roots: vec![root.clone()], state_dir: Some(state.clone()), socket: None, watch: false, ..Config::default() };
    let d3 = Daemon::start(config).unwrap();
    let fs = d3.shared.filesystems()[0].clone();
    assert!(d3.wait_idle(Duration::from_millis(50), Duration::from_secs(30)));
    assert_eq!(fs.index.read().unwrap().tree.size(repertoire_core::ROOT), before);
    d3.shutdown();
    remove_all(&base).unwrap();
}
