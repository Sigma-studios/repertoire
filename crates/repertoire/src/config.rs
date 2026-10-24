//! Daemon settings, from the command line.

use repertoire_core::Excludes;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    /// Directories to index as if each were a filesystem. Empty: every real
    /// filesystem from the mount table (Linux) or the home folder.
    pub roots: Vec<PathBuf>,
    pub excludes: Excludes,
    /// Where snapshots go. `None`: no persistence.
    pub state_dir: Option<PathBuf>,
    /// `None`: don't listen (tests driving the index directly).
    pub socket: Option<PathBuf>,
    /// Watch with fanotify. Off: scan only, then rescan periodically.
    pub watch: bool,
    pub verbose: bool,
    /// Off only to force queue overflows in tests.
    pub unlimited_queue: bool,
    /// Content changes are applied once an entry was quiet this long...
    pub quiet: Duration,
    /// ...or at least this often while it keeps changing.
    pub max_delay: Duration,
    pub snapshot_every: Duration,
    /// Rescan period when not watching.
    pub rescan_every: Duration,
    /// Threads for background (idle priority) scans.
    pub idle_threads: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: Vec::new(),
            excludes: Excludes::default(),
            state_dir: Some("/var/lib/repertoire".into()),
            socket: Some(repertoire_proto::DEFAULT_SOCKET.into()),
            watch: true,
            verbose: false,
            unlimited_queue: true,
            quiet: Duration::from_secs(1),
            max_delay: Duration::from_secs(5),
            snapshot_every: Duration::from_secs(5 * 60),
            rescan_every: Duration::from_secs(20 * 60),
            idle_threads: 2,
        }
    }
}
