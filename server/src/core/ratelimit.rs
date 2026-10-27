//! Failed sign-ins: after too many in a window, further tries are refused until it passes, so nobody on the
//! network can guess passwords at full speed. Counted per (address, email) and, more loosely, per address, so
//! signing in to one's own account does not reset the count for guesses at another one. An IPv6 address counts
//! for its /64 (one network hands out many), an IPv4 address (also IPv4-mapped IPv6) for itself.
//!
//! A try is counted before the slow part (`attempt`, then the password is hashed), so concurrent guesses can't
//! all pass the check first; one that worked takes its count back (`succeeded`). Old entries are swept every
//! SWEEP_EVERY_S, and at most MAX_KEYS are kept (the stalest go first).

use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::Instant;

/// How often (seconds) entries whose failures all left the window are dropped.
const SWEEP_EVERY_S: f64 = 60.0;
/// Most keys kept: one per address and per (address, email) with failures in the window, a few bytes each.
/// Real use (a few players) is far below; beyond it, the keys with the oldest last failure are dropped.
const MAX_KEYS: usize = 100_000;

/// (address key, lower-case email) or, for the per-address count, (address key, None).
type Key = (String, Option<String>);

#[derive(Default)]
struct Failures {
    by_key: HashMap<Key, VecDeque<f64>>,
    last_sweep: f64,
}

pub struct FailureLimiter {
    pub max_failures: usize,
    pub max_per_address: usize,
    pub window_s: f64,
    max_keys: usize,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    failures: Mutex<Failures>,
}

impl Default for FailureLimiter {
    fn default() -> Self {
        Self::new(10, 30, 300.0)
    }
}

/// What failures are counted under: an IPv6 address's /64, else the address as given.
fn address_key(address: &str) -> String {
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
            }
        },
        _ => address.to_string(),
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
        Self { max_failures, max_per_address, window_s, max_keys: MAX_KEYS, clock,
               failures: Mutex::new(Failures::default()) }
    }

    fn keys(address: &str, email: &str) -> [Key; 2] {
        let address = address_key(address);
        [(address.clone(), Some(email.to_lowercase())), (address, None)]
    }

    /// Failures of `key` still in the window (older ones are forgotten).
    fn count(&self, failures: &mut Failures, key: &Key, now: f64) -> usize {
        let Some(q) = failures.by_key.get_mut(key) else { return 0 };
        while q.front().is_some_and(|t| now - t > self.window_s) {
            q.pop_front();
        }
        if q.is_empty() {
            failures.by_key.remove(key);
            return 0;
        }
        q.len()
    }

    fn is_blocked(&self, f: &mut Failures, [pair, addr]: &[Key; 2], now: f64) -> bool {
        self.count(f, pair, now) >= self.max_failures || self.count(f, addr, now) >= self.max_per_address
    }

    /// Every SWEEP_EVERY_S, or when there are too many keys: drop the expired ones, then the stalest beyond
    /// max_keys.
    fn sweep(&self, f: &mut Failures, now: f64) {
        if now - f.last_sweep < SWEEP_EVERY_S && f.by_key.len() <= self.max_keys {
            return;
        }
        f.last_sweep = now;
        let window = self.window_s;
        f.by_key.retain(|_, q| {
            while q.front().is_some_and(|t| now - t > window) {
                q.pop_front();
            }
            !q.is_empty()
        });
        if f.by_key.len() > self.max_keys {
            let mut newest: Vec<(f64, Key)> = f.by_key.iter()
                .map(|(k, q)| (q.back().copied().unwrap_or(0.0), k.clone())).collect();
            newest.sort_by(|a, b| a.0.total_cmp(&b.0));
            let excess = f.by_key.len() - self.max_keys;
            for (_, key) in newest.into_iter().take(excess) {
                f.by_key.remove(&key);
            }
        }
    }

    pub fn blocked(&self, address: &str, email: &str) -> bool {
        let now = (self.clock)();
        let mut f = self.failures.lock();
        self.sweep(&mut f, now);
        self.is_blocked(&mut f, &Self::keys(address, email), now)
    }

    /// Before checking a password: false (nothing counted) while blocked, else the try counts as a failure
    /// until `succeeded` takes it back. Checking and counting under one lock: concurrent tries can't all pass.
    pub fn attempt(&self, address: &str, email: &str) -> bool {
        let now = (self.clock)();
        let keys = Self::keys(address, email);
        let mut f = self.failures.lock();
        if self.is_blocked(&mut f, &keys, now) {
            return false;
        }
        for key in keys {
            f.by_key.entry(key).or_default().push_back(now);
        }
        self.sweep(&mut f, now);
        true
    }

    /// The try worked: the (address, email) count starts over, and the try is taken back from the address's.
    pub fn succeeded(&self, address: &str, email: &str) {
        let [pair, addr] = Self::keys(address, email);
        let mut f = self.failures.lock();
        f.by_key.remove(&pair);
        if let Some(q) = f.by_key.get_mut(&addr) {
            q.pop_back();
            if q.is_empty() {
                f.by_key.remove(&addr);
            }
        }
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.failures.lock().by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    fn clocked(max_failures: usize, max_per_address: usize) -> (FailureLimiter, Arc<AtomicU64>) {
        let t = Arc::new(AtomicU64::new(0));
        let clock = t.clone();
        let lim = FailureLimiter::with_clock(max_failures, max_per_address, 300.0,
                                             Box::new(move || clock.load(Ordering::Relaxed) as f64));
        (lim, t)
    }

    #[test]
    fn blocks_until_the_window_passes() {
        let (lim, t) = clocked(3, 5);
        for _ in 0..3 {
            assert!(!lim.blocked("ip", "A@x.org"));
            assert!(lim.attempt("ip", "A@x.org"));
        }
        assert!(lim.blocked("ip", "a@x.org"), "emails count case-insensitively");
        assert!(!lim.attempt("ip", "a@x.org"), "a blocked try is not counted");
        assert!(!lim.blocked("ip", "b@x.org"));
        assert!(lim.attempt("ip", "b@x.org"));
        assert!(lim.attempt("ip", "c@x.org"));
        assert!(lim.blocked("ip", "d@x.org"), "per address: 5 failures");
        assert!(!lim.blocked("other", "a@x.org"));
        t.store(301, Ordering::Relaxed);
        assert!(!lim.blocked("ip", "a@x.org"));
        assert!(lim.attempt("ip", "a@x.org"));
        lim.succeeded("ip", "a@x.org");
        assert!(!lim.blocked("ip", "a@x.org"));
    }

    /// A sign-in that works takes its own try back: the address's count is the failures only.
    #[test]
    fn a_success_is_not_a_failure() {
        let (lim, _) = clocked(3, 2);
        for _ in 0..5 {
            assert!(lim.attempt("ip", "me@x.org"));
            lim.succeeded("ip", "me@x.org");
        }
        assert!(lim.attempt("ip", "other@x.org") && lim.attempt("ip", "other@x.org"));
        assert!(!lim.attempt("ip", "me@x.org"), "two failures from this address");
    }

    /// Checking and counting are one step: of many concurrent tries only max_failures get to the password.
    #[test]
    fn concurrent_tries_are_counted_before_they_run() {
        let lim = Arc::new(FailureLimiter::new(3, 30, 300.0));
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let passed: usize = (0..16).map(|_| {
            let (lim, barrier) = (lim.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                lim.attempt("ip", "a@x.org")
            })
        }).collect::<Vec<_>>().into_iter().map(|h| usize::from(h.join().unwrap())).sum();
        assert_eq!(passed, 3);
    }

    #[test]
    fn ipv6_counts_per_64() {
        let (lim, _) = clocked(2, 3);
        assert!(lim.attempt("2001:db8:1:2::1", "a@x.org"));
        assert!(lim.attempt("2001:db8:1:2:ffff::9", "a@x.org"));
        assert!(lim.blocked("2001:db8:1:2:abcd::1", "A@x.org"), "the same /64");
        assert!(!lim.blocked("2001:db8:1:3::1", "a@x.org"), "another /64");
        assert!(lim.attempt("::ffff:10.0.0.1", "a@x.org") && lim.attempt("10.0.0.1", "a@x.org"));
        assert!(lim.blocked("10.0.0.1", "a@x.org"), "IPv4-mapped is the IPv4 address");
        assert_eq!(address_key("2001:DB8:0:0:1::1"), "2001:db8:0:0::/64");
    }

    /// Entries go once their failures left the window (no need to ask for them again), and there are never
    /// more than max_keys.
    #[test]
    fn old_entries_are_swept_and_the_map_is_capped() {
        let (mut lim, t) = clocked(3, 1000);
        for i in 0..50 {
            assert!(lim.attempt(&format!("10.0.0.{i}"), "a@x.org"));
        }
        assert_eq!(lim.tracked(), 100);
        t.store(301 + SWEEP_EVERY_S as u64, Ordering::Relaxed);
        assert!(!lim.blocked("10.9.9.9", "b@x.org"));
        assert_eq!(lim.tracked(), 0, "swept without being asked for");
        lim.max_keys = 10;
        for i in 0..50 {
            t.store(1000 + i, Ordering::Relaxed);
            assert!(lim.attempt("10.1.1.1", &format!("guess{i}@x.org")));
            assert!(lim.tracked() <= 10, "{}", lim.tracked());
        }
        let newest = ("10.1.1.1".to_string(), Some("guess49@x.org".to_string()));
        assert!(lim.failures.lock().by_key.contains_key(&newest), "the newest are kept");
        assert_eq!(lim.failures.lock().by_key[&("10.1.1.1".to_string(), None)].len(), 50, "the address's count too");
    }
}
