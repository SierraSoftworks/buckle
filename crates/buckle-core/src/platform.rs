//! Platform identification and the bitset task kinds use to declare support.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The operating system family Buckle is running on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    #[default]
    Linux,
    MacOs,
    Windows,
}

impl Platform {
    /// The platform of the running binary.
    pub const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::Linux => "linux",
            Platform::MacOs => "macos",
            Platform::Windows => "windows",
        })
    }
}

/// A set of platforms. Task kinds, fact providers and health checks declare the
/// set they support; planning an unsupported kind is a plan error rather than a
/// runtime failure.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Platforms(u8);

impl Platforms {
    pub const NONE: Platforms = Platforms(0);
    pub const LINUX: Platforms = Platforms(1);
    pub const MACOS: Platforms = Platforms(2);
    pub const WINDOWS: Platforms = Platforms(4);
    pub const UNIX: Platforms = Platforms(1 | 2);
    pub const ALL: Platforms = Platforms(1 | 2 | 4);

    pub const fn contains(self, platform: Platform) -> bool {
        let bit = match platform {
            Platform::Linux => 1,
            Platform::MacOs => 2,
            Platform::Windows => 4,
        };
        self.0 & bit != 0
    }

    pub const fn union(self, other: Platforms) -> Platforms {
        Platforms(self.0 | other.0)
    }

    pub fn iter(self) -> impl Iterator<Item = Platform> {
        [Platform::Linux, Platform::MacOs, Platform::Windows]
            .into_iter()
            .filter(move |p| self.contains(*p))
    }
}

impl fmt::Debug for Platforms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(Platforms::ALL, Platform::Windows, true)]
    #[case(Platforms::LINUX, Platform::MacOs, false)]
    #[case(Platforms::UNIX, Platform::MacOs, true)]
    #[case(Platforms::UNIX, Platform::Windows, false)]
    fn membership(#[case] set: Platforms, #[case] platform: Platform, #[case] expected: bool) {
        assert_eq!(set.contains(platform), expected);
    }

    #[test]
    fn union_combines() {
        assert_eq!(Platforms::LINUX.union(Platforms::WINDOWS).iter().count(), 2);
    }
}
