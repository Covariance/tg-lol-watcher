//! Time sources, injectable so every time-dependent rule is tested on a frozen clock.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Wall-clock time in UTC epoch milliseconds (what the database stores).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn system_clock() -> Clock {
    Arc::new(now_ms)
}

/// Monotonic seconds plus a way to wait: what pacing (rate limits, backoff) needs.
pub trait Pacer: Send + Sync {
    fn monotonic_s(&self) -> f64;
    fn sleep(&self, seconds: f64);
}

pub struct RealPacer {
    origin: Instant,
}

impl Default for RealPacer {
    fn default() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Pacer for RealPacer {
    fn monotonic_s(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    fn sleep(&self, seconds: f64) {
        // Belt and braces: callers sanitise server-sent waits, but a non-finite value
        // here would panic, and a panic aborts the release build.
        if seconds.is_finite() && seconds > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(seconds.min(3600.0)));
        }
    }
}

/// A tiny xorshift for retry jitter: not worth a dependency.
pub fn jitter(low: f64, high: f64) -> f64 {
    use std::cell::Cell;
    fn seed() -> u64 {
        let local = 0u8; // the stack address differs per thread
        (now_ms() as u64 ^ ((&local as *const u8 as u64) << 16)) | 1
    }
    thread_local! {
        static STATE: Cell<u64> = Cell::new(seed());
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        low + (high - low) * ((x >> 11) as f64 / (1u64 << 53) as f64)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn jitter_stays_in_range() {
        for _ in 0..1000 {
            let j = super::jitter(0.5, 1.5);
            assert!((0.5..1.5).contains(&j));
        }
    }
}
