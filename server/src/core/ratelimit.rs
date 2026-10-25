//! Failed sign-ins: after too many in a window, further tries are refused until it passes, so nobody on the
//! network can guess passwords at full speed. Counted per (address, email) and, more loosely, per address, so
//! signing in to one's own account does not reset the count for guesses at another one.

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// (address, lower-case email) or, for the per-address count, (address, None).
type Key = (String, Option<String>);

pub struct FailureLimiter {
    pub max_failures: usize,
    pub max_per_address: usize,
    pub window_s: f64,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    failures: Mutex<HashMap<Key, VecDeque<f64>>>,
}

impl Default for FailureLimiter {
    fn default() -> Self {
        Self::new(10, 30, 300.0)
    }
}

impl FailureLimiter {
    pub fn new(max_failures: usize, max_per_address: usize, window_s: f64) -> Self {
        let start = Instant::now();
        Self::with_clock(max_failures, max_per_address, window_s, Box::new(move || start.elapsed().as_secs_f64()))
    }

    /// With a clock of our own (monotonic seconds), for tests.
    pub fn with_clock(max_failures: usize, max_per_address: usize, window_s: f64,
                      clock: Box<dyn Fn() -> f64 + Send + Sync>) -> Self {
        Self { max_failures, max_per_address, window_s, clock, failures: Mutex::new(HashMap::new()) }
    }

    fn keys(address: &str, email: &str) -> [Key; 2] {
        [(address.to_string(), Some(email.to_lowercase())), (address.to_string(), None)]
    }

    /// Failures of `key` still in the window (older ones are forgotten).
    fn count(&self, failures: &mut HashMap<Key, VecDeque<f64>>, key: &Key, now: f64) -> usize {
        let Some(q) = failures.get_mut(key) else { return 0 };
        while q.front().is_some_and(|t| now - t > self.window_s) {
            q.pop_front();
        }
        if q.is_empty() {
            failures.remove(key);
            return 0;
        }
        q.len()
    }

    pub fn blocked(&self, address: &str, email: &str) -> bool {
        let now = (self.clock)();
        let [pair, addr] = Self::keys(address, email);
        let mut f = self.failures.lock();
        self.count(&mut f, &pair, now) >= self.max_failures || self.count(&mut f, &addr, now) >= self.max_per_address
    }

    pub fn failed(&self, address: &str, email: &str) {
        let now = (self.clock)();
        let mut f = self.failures.lock();
        for key in Self::keys(address, email) {
            self.count(&mut f, &key, now);
            f.entry(key).or_default().push_back(now);
        }
    }

    pub fn succeeded(&self, address: &str, email: &str) {
        let [pair, _] = Self::keys(address, email);
        self.failures.lock().remove(&pair);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn blocks_until_the_window_passes() {
        let t = Arc::new(AtomicU64::new(0));
        let clock = t.clone();
        let lim = FailureLimiter::with_clock(3, 5, 300.0, Box::new(move || clock.load(Ordering::Relaxed) as f64));
        for _ in 0..3 {
            assert!(!lim.blocked("ip", "A@x.org"));
            lim.failed("ip", "A@x.org");
        }
        assert!(lim.blocked("ip", "a@x.org"), "emails count case-insensitively");
        assert!(!lim.blocked("ip", "b@x.org"));
        lim.failed("ip", "b@x.org");
        lim.failed("ip", "c@x.org");
        assert!(lim.blocked("ip", "d@x.org"), "per address: 5 failures");
        assert!(!lim.blocked("other", "a@x.org"));
        t.store(301, Ordering::Relaxed);
        assert!(!lim.blocked("ip", "a@x.org"));
        lim.failed("ip", "a@x.org");
        lim.succeeded("ip", "a@x.org");
        assert!(!lim.blocked("ip", "a@x.org"));
    }
}
