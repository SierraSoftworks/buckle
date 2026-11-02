//! Reconnect backoff: exponential from 1 s to 5 min with ±20 % jitter, reset
//! once a session is established (design §08, "Reconnect").

use std::time::Duration;

/// Exponential backoff with jitter.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    next: Duration,
    jitter: f64,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(300))
    }
}

impl Backoff {
    /// Backoff from `min` doubling to `max`.
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max: max.max(min),
            next: min,
            jitter: 0.2,
        }
    }

    /// Without jitter, for deterministic tests.
    pub fn without_jitter(mut self) -> Self {
        self.jitter = 0.0;
        self
    }

    /// The un-jittered delay the next call to [`Backoff::next_delay`] is based on.
    pub fn base(&self) -> Duration {
        self.next
    }

    /// The delay before the next attempt; doubles the base for the one after.
    pub fn next_delay(&mut self) -> Duration {
        let base = self.next;
        self.next = (self.next * 2).min(self.max);
        if self.jitter <= 0.0 {
            return base;
        }
        let factor = 1.0 - self.jitter + 2.0 * self.jitter * rand::random::<f64>();
        base.mul_f64(factor).min(self.max)
    }

    /// Back to the minimum after a successful session.
    pub fn reset(&mut self) {
        self.next = self.min;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_to_the_cap_and_resets() {
        let mut b = Backoff::default().without_jitter();
        let delays: Vec<u64> = (0..12).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(
            delays,
            vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]
        );
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let mut b = Backoff::default();
        for _ in 0..4 {
            let base = b.base();
            let d = b.next_delay();
            assert!(
                d >= base.mul_f64(0.8) && d <= base.mul_f64(1.2),
                "{d:?} vs {base:?}"
            );
        }
    }
}
