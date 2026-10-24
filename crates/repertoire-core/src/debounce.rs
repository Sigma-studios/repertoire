//! Coalescing of content-change events.
//!
//! A build writes the same files thousands of times a second. Re-stat'ing
//! on every write would cost more than the build; instead an entry is looked
//! at once it has been quiet for `quiet`, or every `max` while it keeps
//! changing, so sizes still move during a long download.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

pub struct Debouncer<K> {
    /// key -> (first change not yet applied, last change)
    pending: HashMap<K, (Instant, Instant)>,
    quiet: Duration,
    max: Duration,
}

impl<K: Hash + Eq + Clone> Debouncer<K> {
    pub fn new(quiet: Duration, max: Duration) -> Self {
        Debouncer { pending: HashMap::new(), quiet, max }
    }

    pub fn touch(&mut self, key: K, now: Instant) {
        self.pending.entry(key).and_modify(|e| e.1 = now).or_insert((now, now));
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Keys whose time has come, removed from the pending set.
    pub fn due(&mut self, now: Instant) -> Vec<K> {
        let mut out = Vec::new();
        let (quiet, max) = (self.quiet, self.max);
        self.pending.retain(|k, &mut (first, last)| {
            let ready = now.duration_since(last) >= quiet || now.duration_since(first) >= max;
            if ready {
                out.push(k.clone());
            }
            !ready
        });
        out
    }

    /// Everything, regardless of timing (shutdown, tests).
    pub fn drain(&mut self) -> Vec<K> {
        self.pending.drain().map(|(k, _)| k).collect()
    }

    /// When the next key becomes due.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.values().map(|&(first, last)| (last + self.quiet).min(first + self.max)).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_and_max() {
        let t0 = Instant::now();
        let s = Duration::from_secs(1);
        let mut d = Debouncer::new(s, 5 * s);
        d.touch("a", t0);
        assert!(d.due(t0 + s / 2).is_empty());
        assert_eq!(d.due(t0 + s), vec!["a"]);
        // Written every half second: due at the 5 s cap, not before.
        for i in 0..12 {
            d.touch("b", t0 + s / 2 * i);
            let due = d.due(t0 + s / 2 * i);
            assert_eq!(!due.is_empty(), i == 10, "at {i}");
        }
        assert_eq!(d.next_due(), Some(t0 + s / 2 * 11 + s));
    }
}
