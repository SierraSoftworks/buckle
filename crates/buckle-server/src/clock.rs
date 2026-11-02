//! Time, injected so the controller can be tested with tokio's paused clock.

use chrono::{DateTime, Utc};
use std::future::Future;
use std::pin::Pin;

/// A source of wall-clock time and timers.
pub trait Clock: Send + Sync + 'static {
    /// The current time.
    fn now(&self) -> DateTime<Utc>;

    /// Completes at (or after) `deadline`.
    fn sleep_until(&self, deadline: DateTime<Utc>) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Wall-clock time anchored at construction and advanced by tokio's timer,
/// so `tokio::time::pause()` and auto-advance drive it in tests while it
/// tracks the system clock in production.
#[derive(Debug, Clone)]
pub struct TokioClock {
    wall: DateTime<Utc>,
    start: tokio::time::Instant,
}

impl TokioClock {
    /// Anchors at the current system time.
    pub fn new() -> Self {
        Self::starting_at(Utc::now())
    }

    /// Anchors at `wall` (tests that want a fixed epoch).
    pub fn starting_at(wall: DateTime<Utc>) -> Self {
        Self {
            wall,
            start: tokio::time::Instant::now(),
        }
    }
}

impl Default for TokioClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Utc> {
        let elapsed = tokio::time::Instant::now().saturating_duration_since(self.start);
        self.wall + chrono::Duration::from_std(elapsed).unwrap_or_default()
    }

    fn sleep_until(&self, deadline: DateTime<Utc>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let offset = (deadline - self.wall).to_std().unwrap_or_default();
        let at = self.start + offset;
        Box::pin(tokio::time::sleep_until(at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn paused_time_advances_the_wall_clock() {
        let clock = TokioClock::new();
        let t0 = clock.now();
        clock.sleep_until(t0 + chrono::Duration::minutes(10)).await;
        let elapsed = clock.now() - t0;
        assert!(elapsed >= chrono::Duration::minutes(10));
        assert!(elapsed < chrono::Duration::minutes(11));
    }
}
