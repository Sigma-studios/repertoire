//! The daemon's shared state: one index per filesystem, each behind its own
//! lock. Nothing ever holds two of those locks at once, so there is no lock
//! order to get wrong.

use crate::config::Config;
use repertoire_core::{ByPath, Ctx, Filesystem, RawEvent, Scanner, Tree};
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// First scan, nothing to serve yet.
    Scanning,
    /// Serving a snapshot or an older scan while a new scan runs.
    Reconciling,
    /// Kept current by events.
    Live,
    /// Events were lost; a rescan is due or running.
    Stale,
    /// Scanned, not watched.
    Static,
}

pub struct FsIndex {
    pub tree: Tree,
    pub phase: Phase,
    pub last_full_scan: Option<SystemTime>,
    /// While a full scan runs, every event applied to the old tree is kept
    /// here and applied again to the new one.
    pub replay: Option<Replay>,
    /// Tree generation at the last snapshot.
    pub saved_generation: u64,
    /// Events were lost after the running scan started: its result can't
    /// be trusted either.
    pub overflowed: bool,
}

/// Events seen during a scan. Structural ones are kept in order; content
/// changes are only kept once each (re-applying one re-stats, so once is
/// enough), which bounds the buffer during a long build.
#[derive(Default)]
pub struct Replay {
    pub events: Vec<RawEvent>,
    seen: HashSet<RawEvent>,
}

impl Replay {
    pub fn push(&mut self, ev: &RawEvent) {
        let structural = matches!(ev, RawEvent::Created { .. } | RawEvent::Removed { .. } | RawEvent::Renamed { .. });
        if structural || self.seen.insert(ev.clone()) {
            self.events.push(ev.clone());
        }
    }
}

/// One indexed filesystem.
pub struct Fs {
    pub fs: Filesystem,
    pub index: RwLock<FsIndex>,
    pub scanner: Scanner,
    /// Handles that resolved to nothing we index (excluded or outside a
    /// root): skipped without asking the kernel again.
    pub unknown: Mutex<HashSet<Box<[u8]>>>,
    pub queued: AtomicBool,
    #[cfg(target_os = "linux")]
    pub fsid: Option<[u8; 8]>,
    #[cfg(target_os = "linux")]
    pub access: Option<repertoire_linux::access::HandleAccess>,
}

impl Fs {
    pub fn new(shared: &Shared, fs: Filesystem) -> Fs {
        let mounts = shared.mounts.read().unwrap();
        let skip = mounts.iter().filter(|m| !fs.mount_points.contains(m)).cloned().collect();
        let scanner = Scanner {
            excludes: shared.config.excludes.clone(),
            skip,
            #[cfg(target_os = "linux")]
            handle_of: Some(repertoire_linux::handle::handle_at),
            ..Scanner::default()
        };
        #[cfg(target_os = "linux")]
        let (fsid, access) = match repertoire_core::sys::open_dir(&fs.mount_points[0]) {
            Ok(fd) => {
                use std::os::fd::AsFd;
                let fsid = repertoire_linux::handle::fsid(fd.as_fd()).ok();
                (fsid, Some(repertoire_linux::access::HandleAccess::new(fd)))
            }
            Err(_) => (None, None),
        };
        Fs {
            fs,
            index: RwLock::new(FsIndex {
                tree: Tree::new(),
                phase: Phase::Scanning,
                last_full_scan: None,
                replay: None,
                saved_generation: 0,
                overflowed: false,
            }),
            scanner,
            unknown: Mutex::new(HashSet::new()),
            queued: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            fsid,
            #[cfg(target_os = "linux")]
            access,
        }
    }

    pub fn id(&self) -> &str {
        &self.fs.id
    }

    /// The deepest of our mount points that contains `path`.
    pub fn mount_for(&self, path: &Path) -> Option<&PathBuf> {
        self.fs.mount_points.iter().filter(|m| path.starts_with(m)).max_by_key(|m| m.as_os_str().len())
    }

    /// Run `f` with what the event applier needs for this filesystem.
    pub fn with_ctx<R>(&self, f: impl FnOnce(&Ctx) -> R) -> R {
        #[cfg(target_os = "linux")]
        if let Some(access) = &self.access {
            let path_of = |h: &[u8]| {
                if self.unknown.lock().unwrap().contains(h) {
                    return None;
                }
                let p = access.path_of(h);
                let ours = p.as_deref().is_some_and(|p| self.mount_for(p).is_some() && !self.excluded(p));
                if !ours {
                    let mut u = self.unknown.lock().unwrap();
                    if u.len() > 100_000 {
                        u.clear();
                    }
                    u.insert(h.into());
                    return None;
                }
                p
            };
            return f(&Ctx { scanner: &self.scanner, access, handle_path: &path_of });
        }
        let none = |_: &[u8]| None;
        f(&Ctx { scanner: &self.scanner, access: &ByPath, handle_path: &none })
    }

    /// Inside an excluded folder, or inside another mount below ours.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn excluded(&self, p: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Some(m) = self.mount_for(p) else { return true };
        let ex = &self.scanner.excludes;
        let below = p.strip_prefix(m).unwrap_or(p);
        p.ancestors().any(|a| ex.paths.iter().any(|x| x == a))
            || below.components().any(|c| ex.names.contains(c.as_os_str().as_bytes()))
            || self.scanner.skip.iter().any(|s| s != m && s.starts_with(m) && p.starts_with(s))
    }
}

/// A counter that wakes waiters when it moves.
#[derive(Default)]
pub struct Notify {
    generation: Mutex<u64>,
    cv: Condvar,
}

impl Notify {
    pub fn bump(&self) {
        *self.generation.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    /// Wait until the counter differs from `seen` or `timeout` passes.
    pub fn wait(&self, seen: u64, timeout: Duration) -> u64 {
        let g = self.generation.lock().unwrap();
        let (g, _) = self.cv.wait_timeout_while(g, timeout, |g| *g == seen).unwrap();
        *g
    }

    pub fn current(&self) -> u64 {
        *self.generation.lock().unwrap()
    }
}

/// What the daemon is busy with, for `wait_idle`.
pub struct Activity {
    pub last_event: Mutex<Instant>,
    /// Debounced changes not applied yet.
    pub pending: AtomicUsize,
    /// Scans running or queued.
    pub scans: AtomicUsize,
    /// Kernel events read so far.
    pub events: AtomicU64,
    /// Queue overflows seen.
    pub overflows: AtomicU64,
}

pub struct Shared {
    pub config: Config,
    pub filesystems: RwLock<Vec<Arc<Fs>>>,
    /// Every mount point, indexed or not (walks never enter one).
    pub mounts: RwLock<HashSet<PathBuf>>,
    /// Bumped whenever an index changes; subscribers wait on it.
    pub changes: Notify,
    pub scan_queue: Mutex<VecDeque<(Arc<Fs>, crate::engine::Speed)>>,
    pub scan_wake: Condvar,
    pub watching: AtomicBool,
    pub activity: Activity,
    pub stop: AtomicBool,
    /// Test hook: stop reading kernel events (to force an overflow).
    pub pause_reader: AtomicBool,
    #[cfg(target_os = "linux")]
    pub fanotify: std::sync::OnceLock<Arc<repertoire_linux::fanotify::Fanotify>>,
}

impl Shared {
    pub fn new(config: Config) -> Shared {
        Shared {
            config,
            filesystems: RwLock::new(Vec::new()),
            mounts: RwLock::new(HashSet::new()),
            changes: Notify::default(),
            scan_queue: Mutex::new(VecDeque::new()),
            scan_wake: Condvar::new(),
            watching: AtomicBool::new(false),
            activity: Activity {
                last_event: Mutex::new(Instant::now()),
                pending: AtomicUsize::new(0),
                scans: AtomicUsize::new(0),
                events: AtomicU64::new(0),
                overflows: AtomicU64::new(0),
            },
            stop: AtomicBool::new(false),
            pause_reader: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            fanotify: std::sync::OnceLock::new(),
        }
    }

    pub fn filesystems(&self) -> Vec<Arc<Fs>> {
        self.filesystems.read().unwrap().clone()
    }

    /// The filesystem whose mount point is the deepest prefix of `path`.
    pub fn fs_for(&self, path: &Path) -> Option<Arc<Fs>> {
        self.filesystems()
            .into_iter()
            .filter_map(|f| f.mount_for(path).map(|m| (m.as_os_str().len(), f.clone())))
            .max_by_key(|(len, _)| *len)
            .map(|(_, f)| f)
    }

    pub fn watching(&self) -> bool {
        self.watching.load(Ordering::Relaxed)
    }

    pub fn debug(&self, msg: impl AsRef<str>) {
        if self.config.verbose {
            crate::info(msg);
        }
    }
}
