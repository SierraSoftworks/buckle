use std::time::Duration;

use serde::{Serialize, Deserialize};


#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default)]
    pub limit: u8,

    #[serde(default)]
    pub delay: RetryDelay,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub struct RetryDelay(u32);
impl Default for RetryDelay {
    fn default() -> Self {
        Self(5000)
    }
}

impl From<u32> for RetryDelay {
    fn from(other: u32) -> RetryDelay {
        RetryDelay(other)
    }
}

impl Into<Duration> for RetryDelay {
    fn into(self) -> Duration {
        Duration::from_millis(self.0 as u64)
    }
}