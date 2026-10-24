//! Keeping indexes current with fanotify.
//!
//! A reader thread drains the fanotify fd and turns kernel records into
//! [`RawEvent`]s. An applier thread routes them by filesystem id: structure
//! changes (create, delete, rename, attributes) are applied right away;
//! content changes go through a debouncer, because a build rewrites the same
//! files thousands of times a second and re-stat'ing each time would cost
//! more than the build.

use crate::engine::{request_scan, Speed};
use crate::index::{Fs, Phase, Shared};
use crate::info;
use repertoire_core::debounce::Debouncer;
use repertoire_core::{Outcome, RawEvent};
use repertoire_linux::fanotify::{parse, translate, Fanotify};
use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

type Batch = Vec<(Option<[u8; 8]>, RawEvent)>;

/// Set up fanotify and its marks, then start the reader and applier.
/// Called before any scan.
pub fn start(shared: &Arc<Shared>) -> io::Result<()> {
    let fan = Arc::new(Fanotify::new(shared.config.unlimited_queue)?);
    let _ = shared.fanotify.set(fan.clone());
    let fss = shared.filesystems();
    for fs in &fss {
        mark(shared, fs);
    }
    if let Some(dir) = &shared.config.state_dir {
        std::fs::create_dir_all(dir)?;
        if let Err(e) = fan.ignore_dir(dir) {
            info(format!("can't ignore our own writes in {} ({e}); snapshots will show up as events", dir.display()));
        }
    }
    shared.watching.store(true, Ordering::Relaxed);
    // Bounded: if the applier falls behind, events wait in the kernel queue
    // rather than in our memory twice.
    let (tx, rx) = sync_channel::<Batch>(64);
    let reader_shared = shared.clone();
    std::thread::Builder::new()
        .name("fanotify".into())
        .spawn(move || {
            let mut buf = vec![0u8; 256 << 10];
            let fd = fan.fd().as_raw_fd();
            while !reader_shared.stop.load(Ordering::Relaxed) {
                if reader_shared.pause_reader.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                // Poll so the thread notices `stop`.
                let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                // SAFETY: one valid pollfd for the duration of the call.
                if unsafe { libc::poll(&mut p, 1, 200) } <= 0 {
                    continue;
                }
                let n = match fan.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) => {
                        info(format!("reading fanotify events failed: {e}"));
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                let mut batch = Vec::new();
                let mut count = 0;
                let r = parse(&buf[..n], |ev| {
                    count += 1;
                    translate(&ev, &mut batch);
                });
                if let Err(e) = r {
                    info(format!("bad fanotify data: {e}"));
                }
                reader_shared.activity.events.fetch_add(count, Ordering::Relaxed);
                *reader_shared.activity.last_event.lock().unwrap() = Instant::now();
                if tx.send(batch).is_err() {
                    return;
                }
            }
        })?;
    let applier_shared = shared.clone();
    std::thread::Builder::new().name("apply".into()).spawn(move || apply_loop(&applier_shared, rx))?;
    Ok(())
}

/// Watch one filesystem (every mount point: subvolumes may be separate
/// superblocks).
pub fn mark(shared: &Shared, fs: &Fs) {
    let Some(fan) = shared.fanotify.get() else { return };
    for mp in &fs.fs.mount_points {
        if let Err(e) = fan.mark_filesystem(mp) {
            info(format!("{}: can't watch {} ({e}); it will only be rescanned", fs.id(), mp.display()));
        }
    }
    shared.debug(format!("{}: watching (fsid {:02x?})", fs.id(), fs.fsid));
}

fn apply_loop(shared: &Shared, rx: Receiver<Batch>) {
    let mut debouncer: Debouncer<(String, RawEvent)> = Debouncer::new(shared.config.quiet, shared.config.max_delay);
    let mut last_flush: HashMap<String, Instant> = HashMap::new();
    let mut warned_fsid = false;
    loop {
        let now = Instant::now();
        let wait = debouncer.next_due().map_or(Duration::from_secs(1), |t| t.saturating_duration_since(now)).min(Duration::from_secs(1));
        let mut batch = match rx.recv_timeout(wait) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => Vec::new(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        while batch.len() < 100_000 {
            match rx.try_recv() {
                Ok(b) => batch.extend(b),
                Err(_) => break,
            }
        }

        let fss = shared.filesystems();
        let by_fsid = |fsid: &[u8; 8]| fss.iter().find(|f| f.fsid.as_ref() == Some(fsid));
        let mut todo: HashMap<String, (Arc<Fs>, Vec<RawEvent>)> = HashMap::new();
        let now = Instant::now();
        for (fsid, ev) in batch {
            let Some(fsid) = fsid else {
                overflow(shared, &fss);
                continue;
            };
            let Some(fs) = by_fsid(&fsid) else {
                if !warned_fsid {
                    warned_fsid = true;
                    shared.debug(format!("events for unknown fsid {fsid:02x?} ignored"));
                }
                continue;
            };
            if matches!(ev, RawEvent::Modified { .. }) {
                debouncer.touch((fs.id().to_string(), ev), now);
            } else {
                todo.entry(fs.id().to_string()).or_insert_with(|| (fs.clone(), Vec::new())).1.push(ev);
            }
        }
        for (id, ev) in debouncer.due(now) {
            if let Some(fs) = fss.iter().find(|f| f.id() == id) {
                todo.entry(id).or_insert_with(|| (fs.clone(), Vec::new())).1.push(ev);
            }
        }
        shared.activity.pending.store(debouncer.len(), Ordering::Relaxed);

        let mut changed = false;
        for (id, (fs, events)) in todo {
            changed = true;
            let flush = last_flush.get(&id).is_none_or(|t| t.elapsed() >= Duration::from_millis(250)) || debouncer.is_empty();
            if apply(fs.as_ref(), &events, flush) {
                request_scan(shared, &fs, Speed::Idle);
            }
            if flush {
                last_flush.insert(id, Instant::now());
            }
        }
        // Hard-link groups left pending by a rate-limited flush.
        if debouncer.is_empty() {
            for fs in &fss {
                let mut ix = fs.index.write().unwrap();
                if ix.tree.has_pending_links() {
                    let ix = &mut *ix;
                    fs.with_ctx(|cx| ix.tree.flush_links_with(cx));
                    changed = true;
                }
            }
        }
        if changed {
            shared.changes.bump();
        }
    }
}

/// Apply a batch to one filesystem under its lock. True if the tree asks
/// for a rescan.
fn apply(fs: &Fs, events: &[RawEvent], flush: bool) -> bool {
    let mut guard = fs.index.write().unwrap();
    let ix = &mut *guard;
    let mut rescan = false;
    fs.with_ctx(|cx| {
        for ev in events {
            rescan |= ix.tree.apply_raw(ev, cx) == Outcome::Rescan;
            if let Some(r) = &mut ix.replay {
                r.push(ev);
            }
        }
        if flush {
            ix.tree.flush_links_with(cx);
        }
    });
    rescan
}

/// Events were dropped: nothing can be trusted until a rescan. Keep serving
/// the old numbers, flagged.
fn overflow(shared: &Shared, fss: &[Arc<Fs>]) {
    info("fanotify queue overflowed; rescanning every filesystem");
    shared.activity.overflows.fetch_add(1, Ordering::Relaxed);
    for fs in fss {
        let mut ix = fs.index.write().unwrap();
        ix.phase = Phase::Stale;
        ix.overflowed = true;
        drop(ix);
        request_scan(shared, fs, Speed::Idle);
    }
    shared.changes.bump();
}
