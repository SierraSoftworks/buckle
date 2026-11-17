use serde::{Deserialize, Serialize};

/// `GET /api/v1/me` → 200 with this. Without OIDC configured the caller is
/// [`Me::anonymous`] and `is_admin` says what `admin_acl` (absent: allowed)
/// made of the request's metadata. (Servers before 2.0 answered 204 there;
/// a client should still read a 204 as "no sign-in, everything permitted".)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub subject: String,
    pub is_admin: bool,
    /// The server has no sign-in configured: nobody is signed in, and
    /// `name` and `subject` are placeholders.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub anonymous: bool,
    /// Which deployment this is (`web.environment`), shown beside the
    /// product name so nobody mistakes production for staging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
}

/// `web.environment` in `server.yaml`: a label for this deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    /// What the tag says, e.g. `Production`.
    pub name: String,
    /// The tag's fill as `#rgb` or `#rrggbb`; its text is black or white,
    /// whichever reads better on it. Absent: the neutral tag colours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
}

impl Environment {
    /// The colour as `(r, g, b)`, if it is `#rgb` or `#rrggbb`.
    pub fn rgb(&self) -> Option<(u8, u8, u8)> {
        parse_hex(self.colour.as_deref()?)
    }
}

fn parse_hex(colour: &str) -> Option<(u8, u8, u8)> {
    let hex = colour.strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let at = |i: usize, n: usize| u8::from_str_radix(&hex[i..i + n], 16).ok();
    match hex.len() {
        3 => Some((at(0, 1)? * 17, at(1, 1)? * 17, at(2, 1)? * 17)),
        6 => Some((at(0, 2)?, at(2, 2)?, at(4, 2)?)),
        _ => None,
    }
}

impl Me {
    /// The caller of a server without sign-in.
    pub fn anonymous(is_admin: bool) -> Self {
        Self {
            name: "anonymous".into(),
            email: None,
            subject: "anonymous".into(),
            is_admin,
            anonymous: true,
            environment: None,
        }
    }
}

/// `GET /api/v1/csrf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsrfToken {
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_in_user_round_trips_without_the_anonymous_flag() {
        let me = Me {
            name: "Ada".into(),
            email: Some("ada@example.com".into()),
            subject: "ada".into(),
            is_admin: true,
            anonymous: false,
            environment: None,
        };
        let text = serde_json::to_string(&me).unwrap();
        assert!(!text.contains("anonymous"));
        assert_eq!(serde_json::from_str::<Me>(&text).unwrap(), me);
        let open = serde_json::to_value(Me::anonymous(false)).unwrap();
        assert_eq!(open["anonymous"], true);
        assert_eq!(open["is_admin"], false);
        assert!(open.get("environment").is_none());
    }

    #[test]
    fn environment_colours_parse_short_and_long_hex_only() {
        let env = |c: &str| Environment {
            name: "Production".into(),
            colour: Some(c.into()),
        };
        for (colour, rgb) in [
            ("#d00", Some((0xdd, 0, 0))),
            ("#1A2b3C", Some((0x1a, 0x2b, 0x3c))),
            ("d00000", None),
            ("#d0000", None),
            ("#zzz", None),
            ("red", None),
        ] {
            assert_eq!(env(colour).rgb(), rgb, "{colour}");
        }
    }
}
