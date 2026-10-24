//! What the running kernel actually reports. Root only (fanotify
//! filesystem marks need CAP_SYS_ADMIN); skipped otherwise.
#![cfg(target_os = "linux")]

use repertoire_core::RawEvent;
use repertoire_linux::fanotify::{parse, translate, Fanotify};
use repertoire_linux::handle::handle_at;
use std::fs;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn root() -> bool {
    // SAFETY: geteuid can't fail.
    unsafe { libc::geteuid() == 0 }
}

fn scratch(tag: &str) -> PathBuf {
    let d = PathBuf::from(format!("/var/tmp/repertoire-kernel-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn handle(p: &Path) -> Box<[u8]> {
    let fd = repertoire_core::sys::open_dir(p).unwrap();
    handle_at(fd.as_fd(), b"").unwrap()
}

/// Read events until `done` says so or 3 s pass.
fn collect(fan: &Fanotify, mut done: impl FnMut(&[RawEvent]) -> bool) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1 << 16];
    let start = Instant::now();
    // Non-blocking polling keeps the test from hanging.
    let fd = fan.fd();
    while start.elapsed() < Duration::from_secs(3) {
        let mut p = libc::pollfd { fd: std::os::fd::AsRawFd::as_raw_fd(&fd), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut p, 1, 100) } > 0 {
            let n = fan.read(&mut buf).unwrap();
            parse(&buf[..n], |ev| {
                let mut v = Vec::new();
                translate(&ev, &mut v);
                out.extend(v.into_iter().map(|(_, e)| e));
            })
            .unwrap();
        }
        if done(&out) {
            break;
        }
    }
    out
}

#[test]
fn kernel_reports_what_we_rely_on() {
    if !root() {
        eprintln!("skipped: needs root");
        return;
    }
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-parameter.
    unsafe { libc::uname(&mut uts) };
    let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }.to_string_lossy().into_owned();
    eprintln!("kernel {release}");

    let fan = Fanotify::new(true).expect("fanotify_init with FAN_REPORT_DFID_NAME_TARGET (needs 5.17+)");
    let dir = scratch("probe");
    fan.mark_filesystem(&dir).expect("FAN_MARK_FILESYSTEM with FAN_RENAME");
    let (dh, sub) = (handle(&dir), dir.join("sub"));

    // Create, write, close, rename across directories, hard link, unlink.
    fs::create_dir(&sub).unwrap();
    let subh = handle(&sub);
    fs::write(dir.join("a"), b"hello").unwrap();
    fs::rename(dir.join("a"), sub.join("b")).unwrap();
    fs::hard_link(sub.join("b"), dir.join("c")).unwrap();
    fs::remove_file(dir.join("c")).unwrap();
    let evs = collect(&fan, |e| e.iter().filter(|x| matches!(x, RawEvent::Removed { .. })).count() >= 1);
    for e in &evs {
        eprintln!("{e:?}");
    }
    let named = |e: &RawEvent, d: &[u8], n: &[u8]| match e {
        RawEvent::Created { dir, name } | RawEvent::Removed { dir, name } => &**dir == d && &**name == n,
        RawEvent::Modified { dir, name, .. } | RawEvent::Attrib { dir, name, .. } => &**dir == d && &**name == n,
        _ => false,
    };
    // The scanner's handles (name_to_handle_at) are what events carry.
    assert!(evs.iter().any(|e| matches!(e, RawEvent::Created { .. }) && named(e, &dh, b"sub")), "mkdir");
    assert!(evs.iter().any(|e| matches!(e, RawEvent::Created { .. }) && named(e, &dh, b"a")), "create");
    assert!(evs.iter().any(|e| matches!(e, RawEvent::Modified { obj: Some(_), .. }) && named(e, &dh, b"a")), "modify with object fid");
    assert!(
        evs.iter().any(|e| matches!(e, RawEvent::Renamed { from, from_name, to, to_name }
            if **from == *dh && &**from_name == b"a" && **to == *subh && &**to_name == b"b")),
        "one rename event with both ends"
    );
    assert!(evs.iter().any(|e| matches!(e, RawEvent::Created { .. }) && named(e, &dh, b"c")), "link");
    assert!(evs.iter().any(|e| matches!(e, RawEvent::Removed { .. }) && named(e, &dh, b"c")), "unlink");
    // Link count changes on the inode: reported by object handle only?
    let inode_events = evs.iter().filter(|e| matches!(e, RawEvent::Inode { .. })).count();
    let attrib_named = evs.iter().filter(|e| matches!(e, RawEvent::Attrib { .. })).count();
    eprintln!("inode-only events: {inode_events}, named attrib/close events: {attrib_named}");
    fs::remove_dir_all(&dir).unwrap();
}
