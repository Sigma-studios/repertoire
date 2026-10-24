//! The répertoire daemon as a library, so tests can run it in-process.
//! The binaries (`repertoired`, `repertoire`) are thin wrappers.

pub mod config;
pub mod engine;
pub mod handlers;
pub mod index;
pub mod server;
#[cfg(target_os = "linux")]
pub mod watch;

pub use config::Config;
pub use index::{Phase, Shared};

use index::Fs;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Log a line to stderr (always for `info`, only with `--verbose` for
/// `debug`). journald adds the timestamps.
pub fn info(msg: impl AsRef<str>) {
    eprintln!("repertoired: {}", msg.as_ref());
}

pub struct Daemon {
    pub shared: Arc<Shared>,
}

impl Daemon {
    /// Load snapshots, set up the watch, start scanning and listening.
    /// Watching starts before any scan, so nothing changes unseen between
    /// a scan and the first event.
    pub fn start(config: Config) -> io::Result<Daemon> {
        repertoire_core::sys::raise_fd_limit();
        let shared = Arc::new(Shared::new(config));
        let (filesystems, mounts) = engine::discover(&shared.config);
        *shared.mounts.write().unwrap() = mounts;
        for fs in filesystems {
            let fs = Arc::new(Fs::new(&shared, fs));
            engine::load_snapshot(&shared, &fs);
            shared.filesystems.write().unwrap().push(fs);
        }
        #[cfg(target_os = "linux")]
        if shared.config.watch {
            if let Err(e) = watch::start(&shared) {
                info(format!("not watching for changes ({e}); indexes will be refreshed by periodic rescans"));
            }
        }
        if let Some(dir) = &shared.config.state_dir {
            std::fs::create_dir_all(dir)?;
        }
        engine::spawn_scanner(shared.clone());
        for fs in shared.filesystems.read().unwrap().iter() {
            let fast = fs.index.read().unwrap().tree.is_empty();
            engine::request_scan(&shared, fs, if fast { engine::Speed::Fast } else { engine::Speed::Idle });
        }
        engine::spawn_housekeeping(shared.clone());
        if let Some(socket) = shared.config.socket.clone() {
            server::spawn(shared.clone(), &socket)?;
        }
        Ok(Daemon { shared })
    }

    /// Save snapshots and stop background threads.
    pub fn shutdown(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for fs in self.shared.filesystems.read().unwrap().iter() {
            engine::save_snapshot(&self.shared, fs);
        }
        if let Some(s) = &self.shared.config.socket {
            let _ = std::fs::remove_file(s);
        }
        self.shared.changes.bump();
    }

    /// Wait until nothing has happened for `quiet`: no kernel events, no
    /// debounced changes pending, no scan running or queued. For tests and
    /// benchmarks.
    pub fn wait_idle(&self, quiet: Duration, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let a = &self.shared.activity;
            let busy = a.pending.load(Ordering::Relaxed) > 0 || a.scans.load(Ordering::Relaxed) > 0;
            let last = *a.last_event.lock().unwrap();
            if !busy && last.elapsed() >= quiet {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }
}
