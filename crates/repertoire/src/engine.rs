//! Full scans, snapshots and housekeeping.
//!
//! One worker thread runs full scans, one filesystem at a time so they
//! don't fight over the disk. A scan builds a new tree without holding any
//! lock; meanwhile the watcher keeps applying events to the old tree and
//! records them. Then, under the lock, the new tree replaces the old one and
//! the recorded events are applied to it again, so nothing that happened
//! during the scan is lost.

use crate::index::{Fs, Phase, Replay, Shared};
use crate::{info, Config};
use repertoire_core::snapshot::{self, Meta};
use repertoire_core::{Filesystem, Outcome, Tree, ROOT};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    /// Every core, normal priority: the very first scan, nothing to show yet.
    Fast,
    /// A few threads at idle CPU and I/O priority: must not make a game
    /// stutter.
    Idle,
}

/// The filesystems to index and every mount point.
pub fn discover(config: &Config) -> (Vec<Filesystem>, HashSet<PathBuf>) {
    #[cfg(target_os = "linux")]
    let (fss, mounts) = repertoire_linux::mounts::filesystems();
    #[cfg(not(target_os = "linux"))]
    let (fss, mounts) = (Vec::new(), HashSet::new());
    if !config.roots.is_empty() {
        let roots = config.roots.iter().map(|r| Filesystem::from_root(r.canonicalize().unwrap_or_else(|_| r.clone()))).collect();
        return (roots, mounts);
    }
    if cfg!(target_os = "linux") {
        (fss, mounts)
    } else {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into());
        (vec![Filesystem::from_root(home)], mounts)
    }
}

pub fn snapshot_path(config: &Config, fs: &Fs) -> Option<PathBuf> {
    let name: String = fs.id().chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect();
    Some(config.state_dir.as_ref()?.join(format!("{name}.snap")))
}

/// Serve a snapshot until the reconcile scan is done. A snapshot of another
/// filesystem, another layout, or a corrupt one is discarded.
pub fn load_snapshot(shared: &Shared, fs: &Fs) {
    let Some(path) = snapshot_path(&shared.config, fs) else { return };
    if !path.exists() {
        return;
    }
    let t = Instant::now();
    match snapshot::load(&path) {
        Ok((tree, meta)) if meta.fs_id == fs.fs.id && meta.mounts == fs.fs.mount_points => {
            info(format!("{}: loaded snapshot ({} entries) in {:.0?}", fs.id(), tree.len(), t.elapsed()));
            let mut ix = fs.index.write().unwrap();
            ix.saved_generation = tree.generation();
            ix.tree = tree;
            ix.phase = Phase::Reconciling;
            ix.last_full_scan = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(meta.saved_at.max(0) as u64));
        }
        Ok(_) => info(format!("{}: snapshot is for another filesystem layout, ignored", fs.id())),
        Err(e) => {
            info(format!("{}: discarding snapshot {}: {e}", fs.id(), path.display()));
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub fn save_snapshot(shared: &Shared, fs: &Fs) {
    let Some(path) = snapshot_path(&shared.config, fs) else { return };
    let (data, generation) = {
        let ix = fs.index.read().unwrap();
        // Only trees that are complete and changed since the last save.
        if !matches!(ix.phase, Phase::Live | Phase::Static) || ix.tree.generation() == ix.saved_generation {
            return;
        }
        let saved_at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        let meta = Meta { fs_id: fs.fs.id.clone(), saved_at, mounts: fs.fs.mount_points.clone() };
        (snapshot::encode(&ix.tree, &meta), ix.tree.generation())
    };
    let tmp = path.with_extension("snap.tmp");
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    };
    match write() {
        Ok(()) => {
            fs.index.write().unwrap().saved_generation = generation;
            shared.debug(format!("{}: snapshot saved ({} KiB)", fs.id(), data.len() >> 10));
        }
        Err(e) => info(format!("{}: can't save snapshot {}: {e}", fs.id(), path.display())),
    }
}

/// Queue a full scan unless one is already queued for that filesystem.
pub fn request_scan(shared: &Shared, fs: &Arc<Fs>, speed: Speed) {
    if fs.queued.swap(true, Ordering::Relaxed) {
        return;
    }
    shared.activity.scans.fetch_add(1, Ordering::Relaxed);
    shared.scan_queue.lock().unwrap().push_back((fs.clone(), speed));
    shared.scan_wake.notify_all();
}

pub fn spawn_scanner(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("scan".into())
        .spawn(move || {
            let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
            let fast = rayon::ThreadPoolBuilder::new().num_threads(cores).thread_name(|i| format!("scan-fast-{i}")).build().expect("thread pool");
            let idle = rayon::ThreadPoolBuilder::new()
                .num_threads(shared.config.idle_threads.max(1))
                .thread_name(|i| format!("scan-idle-{i}"))
                .start_handler(|_| idle_priority())
                .build()
                .expect("thread pool");
            loop {
                let job = {
                    let mut q = shared.scan_queue.lock().unwrap();
                    loop {
                        if shared.stop.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(j) = q.pop_front() {
                            break j;
                        }
                        q = shared.scan_wake.wait_timeout(q, Duration::from_millis(500)).unwrap().0;
                    }
                };
                let (fs, speed) = job;
                fs.queued.store(false, Ordering::Relaxed);
                full_scan(&shared, &fs, if speed == Speed::Fast { &fast } else { &idle });
                shared.activity.scans.fetch_sub(1, Ordering::Relaxed);
            }
        })
        .expect("spawn scanner");
}

fn idle_priority() {
    #[cfg(target_os = "linux")]
    repertoire_linux::prio::idle_current_thread();
}

fn full_scan(shared: &Shared, fs: &Arc<Fs>, pool: &rayon::ThreadPool) {
    {
        let mut ix = fs.index.write().unwrap();
        ix.phase = match ix.phase {
            _ if ix.tree.is_empty() => Phase::Scanning,
            Phase::Stale => Phase::Stale,
            _ => Phase::Reconciling,
        };
        ix.replay = Some(Replay::default());
        ix.overflowed = false;
    }
    fs.scanner.progress.store(0, Ordering::Relaxed);
    shared.changes.bump();
    let start = Instant::now();
    let mut tree = Tree::new();
    pool.install(|| {
        for mp in &fs.fs.mount_points {
            match fs.scanner.scan_root(mp) {
                Ok(scan) => {
                    tree.graft(ROOT, scan, false);
                }
                Err(e) => info(format!("{}: can't scan {}: {e}", fs.id(), mp.display())),
            }
        }
    });
    let scanned_in = start.elapsed();
    let (entries, bytes, replayed, again) = {
        let mut guard = fs.index.write().unwrap();
        let ix = &mut *guard;
        let replay = ix.replay.take().unwrap_or_default();
        ix.tree = tree;
        let mut again = false;
        fs.with_ctx(|cx| {
            for ev in &replay.events {
                again |= ix.tree.apply_raw(ev, cx) == Outcome::Rescan;
            }
            ix.tree.flush_links_with(cx);
        });
        ix.phase = match () {
            _ if ix.overflowed => Phase::Stale, // another scan is queued
            _ if shared.watching() => Phase::Live,
            _ => Phase::Static,
        };
        ix.last_full_scan = Some(SystemTime::now());
        (ix.tree.len(), ix.tree.size(ROOT), replay.events.len(), again)
    };
    shared.changes.bump();
    info(format!(
        "{}: scanned {entries} entries, {} MiB, in {scanned_in:.2?}; replayed {replayed} events",
        fs.id(),
        bytes >> 20
    ));
    if again {
        request_scan(shared, fs, Speed::Idle);
    }
    save_snapshot(shared, fs);
}

/// Periodic work: snapshots, rescans when not watching, new or removed
/// filesystems.
pub fn spawn_housekeeping(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("housekeeping".into())
        .spawn(move || {
            let mut last_snapshot = Instant::now();
            let mut last_mounts = Instant::now();
            while !shared.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if last_snapshot.elapsed() >= shared.config.snapshot_every {
                    last_snapshot = Instant::now();
                    for fs in shared.filesystems() {
                        save_snapshot(&shared, &fs);
                    }
                }
                if !shared.watching() {
                    for fs in shared.filesystems() {
                        let due = fs.index.read().unwrap().last_full_scan.and_then(|t| t.elapsed().ok()).is_some_and(|age| age >= shared.config.rescan_every);
                        if due {
                            request_scan(&shared, &fs, Speed::Idle);
                        }
                    }
                }
                if shared.config.roots.is_empty() && last_mounts.elapsed() >= Duration::from_secs(30) {
                    last_mounts = Instant::now();
                    refresh_mounts(&shared);
                }
            }
        })
        .expect("spawn housekeeping");
}

/// Pick up filesystems mounted or unmounted since start.
fn refresh_mounts(shared: &Arc<Shared>) {
    let (now, mounts) = discover(&shared.config);
    let current: Vec<Filesystem> = shared.filesystems().iter().map(|f| f.fs.clone()).collect();
    if now == current {
        return;
    }
    *shared.mounts.write().unwrap() = mounts;
    let mut list = shared.filesystems.write().unwrap();
    list.retain(|f| {
        let keep = now.contains(&f.fs);
        if !keep {
            info(format!("{}: unmounted, dropped", f.id()));
        }
        keep
    });
    for fs in now.into_iter().filter(|f| !current.contains(f)) {
        info(format!("{}: new filesystem at {}", fs.id, fs.mount_points[0].display()));
        let fs = Arc::new(Fs::new(shared, fs));
        #[cfg(target_os = "linux")]
        crate::watch::mark(shared, &fs);
        request_scan(shared, &fs, Speed::Idle);
        list.push(fs);
    }
    drop(list);
    shared.changes.bump();
}
