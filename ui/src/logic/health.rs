//! Health readings as states: how bad a reading is.

use super::tone::Tone;

/// A reading's state, least to most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CheckState {
    Pass,
    /// An informational check failing: worth seeing, never gating.
    Informational,
    /// The check could not run.
    Unknown,
    Fail,
}

impl CheckState {
    pub fn of(status: &str, required: bool) -> Self {
        match status {
            "pass" => Self::Pass,
            "fail" if !required => Self::Informational,
            "fail" => Self::Fail,
            _ => Self::Unknown,
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Self::Pass => Tone::Ok,
            Self::Informational => Tone::Mute,
            Self::Unknown => Tone::Warn,
            Self::Fail => Tone::Danger,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "passing",
            Self::Informational => "failing (informational)",
            Self::Unknown => "unknown",
            Self::Fail => "failing",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Informational => "info",
            Self::Unknown => "unknown",
            Self::Fail => "fail",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_from_status() {
        for (status, required, want) in [
            ("pass", true, CheckState::Pass),
            ("fail", true, CheckState::Fail),
            ("fail", false, CheckState::Informational),
            ("unknown", true, CheckState::Unknown),
        ] {
            assert_eq!(
                CheckState::of(status, required),
                want,
                "{status} required={required}"
            );
        }
    }

    #[test]
    fn severity_puts_required_failures_first() {
        assert!(CheckState::Fail > CheckState::Unknown);
        assert!(CheckState::Unknown > CheckState::Informational);
        assert!(CheckState::Informational > CheckState::Pass);
    }
}
