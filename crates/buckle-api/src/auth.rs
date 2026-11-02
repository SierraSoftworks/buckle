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
        };
        let text = serde_json::to_string(&me).unwrap();
        assert!(!text.contains("anonymous"));
        assert_eq!(serde_json::from_str::<Me>(&text).unwrap(), me);
        let open = serde_json::to_value(Me::anonymous(false)).unwrap();
        assert_eq!(open["anonymous"], true);
        assert_eq!(open["is_admin"], false);
    }
}
