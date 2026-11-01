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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_no_retries_with_a_five_second_delay() {
        let config: RetryConfig = serde_yaml::from_str("{}").expect("an empty config should parse");

        assert_eq!(config.limit, 0);
        let delay: Duration = config.delay.into();
        assert_eq!(delay, Duration::from_secs(5));
    }

    #[test]
    fn reads_the_limit_and_delay_in_milliseconds() {
        let config: RetryConfig =
            serde_yaml::from_str("limit: 3\ndelay: 250\n").expect("the config should parse");

        assert_eq!(config.limit, 3);
        let delay: Duration = config.delay.into();
        assert_eq!(delay, Duration::from_millis(250));
    }
}
