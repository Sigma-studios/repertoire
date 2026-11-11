//! Background scans give way to everyone else's disk reads.
//!
//! Idle I/O priority isn't enough: NVMe drives usually run the `none`
//! scheduler, which ignores it, so a rescan at boot competed at full
//! strength with the login it overlapped (desktop components waited seconds
//! for their own files). Instead the pacer measures what *other* processes
//! read (every disk's reads minus our own) and:
//!
//! - before a background scan, waits until the disk has been quiet for a few
//!   seconds, but never more than [`MAX_WAIT`], so a busy disk can't put the
//!   scan off forever;
//! - during it, while others are reading, pauses briefly before each
//!   directory: the scan slows to a trickle but always moves on, and finishes
//!   even on a disk that's never idle.
//!
//! First scans (nothing to serve yet) aren't paced.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Others reading more than this, per second, is "busy".
const BUSY_BYTES_PER_SEC: u64 = 4 << 20;
/// How often the counters are read.
const SAMPLE: Duration = Duration::from_millis(250);
/// Pause before each directory while others are reading.
const SLOW_DOWN: Duration = Duration::from_millis(10);
/// Quiet needed before a background scan starts...
const QUIET_FOR: Duration = Duration::from_secs(5);
/// ...but it starts after this regardless.
pub const MAX_WAIT: Duration = Duration::from_secs(120);

pub struct Pacer {
    enabled: bool,
    /// Pacing applies only while a background scan runs.
    active: AtomicBool,
    busy: AtomicBool,
    last: Mutex<Option<Sample>>,
    /// Directories the current scan paused before.
    paused: AtomicU64,
}

#[derive(Clone, Copy)]
struct Sample {
    at: Instant,
    disk: u64,
    own: u64,
}

impl Pacer {
    pub fn new(enabled: bool) -> Pacer {
        Pacer {
            enabled: enabled && measure().is_some(),
            active: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            last: Mutex::new(None),
            paused: AtomicU64::new(0),
        }
    }

    /// Before a background scan: wait for the disk to be quiet, at most
    /// [`MAX_WAIT`], or until `stop`. Returns how long it waited.
    pub fn wait_for_quiet(&self, stop: &AtomicBool) -> Duration {
        let start = Instant::now();
        if !self.enabled {
            return Duration::ZERO;
        }
        let mut quiet_since = Instant::now();
        while start.elapsed() < MAX_WAIT && !stop.load(Ordering::Relaxed) {
            std::thread::sleep(SAMPLE);
            if self.sample(&mut self.last.lock().unwrap()) {
                quiet_since = Instant::now();
            } else if quiet_since.elapsed() >= QUIET_FOR {
                break;
            }
        }
        start.elapsed()
    }

    /// Start or stop pacing the scan that's running.
    pub fn set_active(&self, active: bool) {
        if active {
            self.paused.store(0, Ordering::Relaxed);
        }
        self.active.store(self.enabled && active, Ordering::Relaxed);
    }

    /// Directories the current (or last) scan paused before, and for how
    /// long each.
    pub fn paused(&self) -> (u64, Duration) {
        (self.paused.load(Ordering::Relaxed), SLOW_DOWN)
    }

    /// Called before each directory a scan reads.
    pub fn pace(&self) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        // One thread at a time checks and takes the sample, under one lock:
        // two threads sampling back to back would see an instant with no
        // reads in it, call the disk idle, and let the scan run flat out
        // until the next sample (at full speed, that's the whole scan).
        let busy = match self.last.try_lock() {
            Ok(mut last) if last.is_none_or(|s| s.at.elapsed() >= SAMPLE) => self.sample(&mut last),
            _ => self.busy.load(Ordering::Relaxed),
        };
        if busy {
            std::thread::sleep(SLOW_DOWN);
            self.paused.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Read the counters; returns (and remembers) whether others are busy.
    /// `last` is the previous sample, held locked by the caller.
    fn sample(&self, last: &mut Option<Sample>) -> bool {
        let Some((disk, own)) = measure() else { return false };
        let now = Instant::now();
        let busy = match *last {
            Some(prev) => {
                let secs = now.duration_since(prev.at).as_secs_f64().max(1e-3);
                // Our own reads are counted on the disk too; take them out.
                let others = disk.saturating_sub(prev.disk).saturating_sub(own.saturating_sub(prev.own));
                others as f64 / secs > BUSY_BYTES_PER_SEC as f64
            }
            None => false,
        };
        *last = Some(Sample { at: now, disk, own });
        self.busy.store(busy, Ordering::Relaxed);
        busy
    }
}

/// (bytes read from every disk, bytes we read), or `None` where that can't
/// be known: then nothing is paced.
fn measure() -> Option<(u64, u64)> {
    #[cfg(target_os = "linux")]
    {
        Some((repertoire_linux::diskio::disk_read_bytes()?, repertoire_linux::diskio::own_read_bytes()?))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
