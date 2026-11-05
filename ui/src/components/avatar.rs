//! A person's round avatar: their Gravatar (an identicon for an address
//! Gravatar does not know), over their initials, which show while it loads
//! or when it cannot (offline, or demo mode, which never leaves the page).

use sha2::{Digest, Sha256};
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct AvatarProps {
    /// `Name <email>`, as Git records an author, or a bare name.
    pub author: AttrValue,
}

#[function_component(Avatar)]
pub fn avatar(props: &AvatarProps) -> Html {
    let (name, email) = split_author(&props.author);
    let src = email
        .filter(|_| !crate::fixtures::is_demo())
        .map(gravatar_url);
    html! {
        <span class="avatar" aria-hidden="true">
            { initials(name) }
            if let Some(src) = src { <img class="avatar__img" {src} alt="" loading="lazy" referrerpolicy="no-referrer" /> }
        </span>
    }
}

/// `Ada Lovelace <ada@example.com>` → (`Ada Lovelace`, `ada@example.com`).
pub fn split_author(author: &str) -> (&str, Option<&str>) {
    match author.split_once(" <") {
        Some((name, rest)) => {
            let email = rest.trim_end_matches('>').trim();
            (name.trim(), (!email.is_empty()).then_some(email))
        }
        None if author.contains('@') && !author.contains(' ') => (author, Some(author)),
        None => (author.trim(), None),
    }
}

/// Gravatar takes the SHA-256 of the trimmed, lower-cased address.
pub fn gravatar_url(email: &str) -> String {
    let digest = Sha256::digest(email.trim().to_lowercase().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("https://gravatar.com/avatar/{hex}?s=48&d=identicon")
}

/// Up to two initials: `Ada Lovelace` → `AL`.
pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authors_split_into_name_and_address() {
        for (author, expected) in [
            (
                "Ada Lovelace <ada@example.com>",
                ("Ada Lovelace", Some("ada@example.com")),
            ),
            (
                "ada@example.com",
                ("ada@example.com", Some("ada@example.com")),
            ),
            ("Ada Lovelace", ("Ada Lovelace", None)),
            ("Ada <>", ("Ada", None)),
        ] {
            assert_eq!(split_author(author), expected, "{author}");
        }
    }

    #[test]
    fn gravatar_hashes_the_normalised_address() {
        // Gravatar's documented example address.
        let url = gravatar_url(" MyEmailAddress@example.com ");
        assert!(
            url.starts_with("https://gravatar.com/avatar/84059b07d4be67b806386c0aad8070a23f18836bbaae342275dc0a83414c32ee?"),
            "{url}"
        );
        assert!(url.contains("d=identicon"));
    }
}
