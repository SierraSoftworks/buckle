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
    /// The platform a host's facts describe (`os.family`, then `os.id`), for dry
    /// runs over facts fixtures: a Debian fixture must plan as Linux even when the
    /// validator runs on macOS or Windows. `None` when the facts do not say.
    pub fn from_facts(facts: &crate::facts::Facts) -> Option<Self> {
        ["/os/family", "/os/id", "/os/kernel"]
            .iter()
            .filter_map(|p| facts.tree.pointer(p).and_then(|v| v.as_str()))
            .find_map(|value| match value.to_ascii_lowercase().as_str() {
                "darwin" | "macos" | "mac" => Some(Platform::MacOs),
                "windows" | "windows_nt" | "win32" => Some(Platform::Windows),
                "linux" | "debian" | "ubuntu" | "rhel" | "fedora" | "centos" | "suse" | "arch"
                | "alpine" | "nixos" | "raspbian" | "gentoo" => Some(Platform::Linux),
                _ => None,
            })
    }

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

#[cfg(test)]
mod facts_tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(serde_json::json!({"os": {"family": "debian", "id": "debian"}}), Some(Platform::Linux))]
    #[case(serde_json::json!({"os": {"family": "Darwin"}}), Some(Platform::MacOs))]
    #[case(serde_json::json!({"os": {"id": "windows"}}), Some(Platform::Windows))]
    #[case(serde_json::json!({"os": {"family": "plan9"}}), None)]
    #[case(serde_json::json!({}), None)]
    fn platform_from_facts(#[case] tree: serde_json::Value, #[case] expected: Option<Platform>) {
        let facts = crate::facts::Facts {
            tree,
            collected_at: Default::default(),
            stale: Default::default(),
        };
        assert_eq!(Platform::from_facts(&facts), expected);
    }
}
