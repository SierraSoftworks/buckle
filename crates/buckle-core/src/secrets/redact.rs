//! Scrubbing known secret values out of text that Buckle did not render
//! itself, such as the stdout and stderr of a `command.run` whose environment
//! held a secret, and building redacted previews.
//!
//! This module reveals values (to search for them) and so holds a
//! [`Witness`]; like the sinks and the template renderer, it is one of the
//! greppable places that do.
//!
//! The scrubber looks for each secret's value and its common encodings:
//!
//! * the value itself, and the value as a JSON string body (escaped quotes,
//!   backslashes and newlines);
//! * the value URL-encoded (RFC 3986 unreserved characters kept, everything
//!   else percent-encoded; the same with `+` for spaces as in form bodies;
//!   and with every non-alphanumeric character encoded);
//! * the value base64-encoded (standard and URL-safe alphabets, with and
//!   without padding).
//!
//! Limitations: base64 is only recognised when the secret starts the encoded
//! block, so `base64("user:" + secret)` (an HTTP basic-auth header) is not
//! scrubbed; other encodings (hex, shell quoting) are not recognised; an
//! empty secret cannot be searched for and is skipped. Very short secrets are
//! scrubbed wherever they occur, which can be noisy but never leaks.

use super::sinks::Witness;
use super::{Redacted, SecretStore};
use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use base64::Engine as _;
use std::fmt;

/// Characters percent-encoded by `encodeURIComponent`: everything except
/// ASCII alphanumerics and the RFC 3986 unreserved marks.
const URI_COMPONENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// The forms of `value` that the scrubber searches for, deduplicated, longest first.
fn encodings(value: &str) -> Vec<String> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    if value.is_empty() {
        return Vec::new();
    }
    let component = percent_encoding::utf8_percent_encode(value, URI_COMPONENT).to_string();
    let mut forms = vec![
        value.to_owned(),
        component.replace("%20", "+"),
        component,
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
            .to_string(),
        STANDARD.encode(value),
        STANDARD_NO_PAD.encode(value),
        URL_SAFE.encode(value),
        URL_SAFE_NO_PAD.encode(value),
    ];
    if let Ok(json) = serde_json::to_string(value) {
        forms.push(json[1..json.len() - 1].to_owned());
    }
    forms.retain(|f| !f.is_empty());
    forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    forms.dedup();
    forms
}

/// A reusable scrubber for one [`SecretStore`]. Build it once per command
/// (or per apply) and run every output through it.
///
/// Its `Debug` never shows the patterns it searches for.
///
/// ```
/// use buckle_core::secrets::redact::Redactor;
/// use buckle_core::secrets::{InstallKey, Secret, SecretStore};
///
/// let mut store = SecretStore::new();
/// store.insert(Secret::new("db", "hunter2".into(), &InstallKey::from_bytes([7u8; 32])));
/// let redactor = Redactor::new(&store);
/// let out = redactor.redact("connecting with hunter2 (aHVudGVyMg==)");
/// assert!(!out.get().contains("hunter2"));
/// assert!(!out.get().contains("aHVudGVyMg"));
/// assert!(out.get().contains("\u{ab}secret:db@"));
/// ```
pub struct Redactor {
    patterns: Vec<String>,
    tokens: Vec<String>,
    automaton: Option<AhoCorasick>,
}

impl Redactor {
    /// Collects the values and encodings of every secret in `store`.
    pub fn new(store: &SecretStore) -> Self {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for secret in store.iter() {
            let token = secret.token();
            for form in encodings(secret.reveal(Witness::new())) {
                pairs.push((form, token.clone()));
            }
        }
        // Longest first so that the fallback scan prefers the longest match,
        // as the automaton does.
        pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
        let (patterns, tokens): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        let automaton = if patterns.is_empty() {
            None
        } else {
            // A contiguous NFA builds in time linear in the pattern bytes; the
            // DFA the builder would otherwise pick for a small pattern set
            // takes seconds for a multi-kilobyte secret such as a private key.
            AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostLongest)
                .kind(Some(AhoCorasickKind::ContiguousNFA))
                .build(&patterns)
                .ok()
        };
        Self {
            patterns,
            tokens,
            automaton,
        }
    }

    /// Whether there is nothing to search for (no secrets, or only empty ones).
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Replaces every occurrence of a known value or encoding with the
    /// secret's token. Text is scanned once, left to right, preferring the
    /// longest match, so a token that was just inserted is never rescanned.
    pub fn redact(&self, text: &str) -> Redacted<String> {
        if self.patterns.is_empty() {
            return Redacted::new(text.to_owned());
        }
        match &self.automaton {
            Some(ac) => Redacted::new(ac.replace_all(text, &self.tokens)),
            None => Redacted::new(self.scan(text)),
        }
    }

    /// Like [`Redactor::redact`] for raw process output; invalid UTF-8 is
    /// replaced with U+FFFD first.
    pub fn redact_bytes(&self, bytes: &[u8]) -> Redacted<String> {
        self.redact(&String::from_utf8_lossy(bytes))
    }

    /// Leftmost-longest replacement without the automaton, used when the
    /// automaton cannot be built (pattern set too large).
    fn scan(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'outer: while !rest.is_empty() {
            for (pattern, token) in self.patterns.iter().zip(&self.tokens) {
                if rest.starts_with(pattern.as_str()) {
                    out.push_str(token);
                    rest = &rest[pattern.len()..];
                    continue 'outer;
                }
            }
            let mut chars = rest.chars();
            if let Some(c) = chars.next() {
                out.push(c);
            }
            rest = chars.as_str();
        }
        out
    }
}

impl fmt::Debug for Redactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Redactor({} patterns)", self.patterns.len())
    }
}

/// Replaces every occurrence of any secret value in `store` (and its common
/// URL and base64 encodings; see the module docs) with the secret's token.
///
/// The engine runs captured command output through this before it reaches
/// spans, plans or state. For many outputs against one store, build a
/// [`Redactor`] once instead.
///
/// ```
/// use buckle_core::secrets::{redact_known, InstallKey, Secret, SecretStore};
///
/// let mut store = SecretStore::new();
/// store.insert(Secret::new("api", "p@ss word".into(), &InstallKey::from_bytes([7u8; 32])));
/// let out = redact_known("GET /login?pw=p%40ss%20word -> p@ss word", &store);
/// assert!(!out.get().contains("p@ss"));
/// assert!(!out.get().contains("p%40ss"));
/// ```
pub fn redact_known(text: &str, store: &SecretStore) -> Redacted<String> {
    Redactor::new(store).redact(text)
}

/// [`redact_known`] for raw bytes (process output); invalid UTF-8 becomes U+FFFD.
pub fn redact_known_bytes(bytes: &[u8], store: &SecretStore) -> Redacted<String> {
    Redactor::new(store).redact_bytes(bytes)
}

/// Converts a template error into a Buckle user error that is safe to show.
///
/// minijinja's `Debug` (and alternate `Display`) of an error appends a
/// "Referenced variables" dump that formats every variable the template
/// touched, and formatting a secret in a real render reveals it. This keeps
/// only the plain description of the error and its causes, scrubs every
/// value in `store` from that text with [`redact_known`], and drops the
/// original error (and its debug info) entirely.
///
/// Hook: `Templater::render_with_view` should map its render error with
/// `.map_err(|e| crate::secrets::redact::template_error(e, &ctx.secrets))`
/// instead of `.to_human_error()`.
///
/// ```
/// use buckle_core::secrets::redact::template_error;
/// use buckle_core::secrets::{InstallKey, Secret, SecretStore};
///
/// let mut store = SecretStore::new();
/// store.insert(Secret::new("db", "hunter2".into(), &InstallKey::from_bytes([7u8; 32])));
/// let env = minijinja::Environment::new();
/// let raw = env
///     .render_str("{{ pw }} {{ missing.field }}", minijinja::context! { pw => "hunter2" })
///     .unwrap_err();
/// assert!(format!("{raw:?}").contains("hunter2")); // the debug info dumps variables
/// let safe = template_error(raw, &store);
/// assert!(!format!("{safe:?} {safe}").contains("hunter2"));
/// ```
pub fn template_error(err: minijinja::Error, store: &SecretStore) -> crate::Error {
    use std::error::Error as _;
    // Non-alternate Display never includes the debug info.
    let mut text = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    drop(err);
    let scrubbed = redact_known(&text, store).into_inner();
    crate::error::wrap_user(
        scrubbed,
        "A template could not be rendered.",
        &[
            "Check the template for syntax errors and that every variable it references is defined.",
            "Secret values may only be used in file templates and in fields declared as secret-capable.",
        ],
    )
}

impl Redacted<String> {
    /// A unified diff of two redacted shadows (`current` → `planned`, three
    /// lines of context), formatted exactly like [`crate::plan::Diff::unified`]
    /// so tasks can build previews and summaries without going through the
    /// plan module. Empty when the texts are equal.
    ///
    /// ```
    /// use buckle_core::secrets::Redacted;
    ///
    /// let old = Redacted::plain("a\nb\n".to_string());
    /// let new = Redacted::plain("a\nc\n".to_string());
    /// let diff = Redacted::shadow_diff(&old, &new);
    /// assert!(diff.get().contains("-b\n"));
    /// assert!(diff.get().contains("+c\n"));
    /// assert!(Redacted::shadow_diff(&old, &old).get().is_empty());
    /// ```
    pub fn shadow_diff(old: &Redacted<String>, new: &Redacted<String>) -> Redacted<String> {
        if old.get() == new.get() {
            return Redacted::new(String::new());
        }
        let text = similar::TextDiff::from_lines(old.get().as_str(), new.get().as_str())
            .unified_diff()
            .context_radius(3)
            .header("current", "planned")
            .to_string();
        Redacted::new(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{InstallKey, Secret};
    use rstest::rstest;

    fn store(values: &[(&str, &str)]) -> SecretStore {
        let key = InstallKey::from_bytes([5u8; 32]);
        let mut store = SecretStore::new();
        for (name, value) in values {
            store.insert(Secret::new(*name, (*value).to_owned(), &key));
        }
        store
    }

    #[rstest]
    #[case("plain: hunter2!", "plain: {db}!")]
    #[case("b64: aHVudGVyMg==", "b64: {db}")]
    #[case("b64 nopad: aHVudGVyMg.", "b64 nopad: {db}.")]
    #[case("twice hunter2hunter2", "twice {db}{db}")]
    #[case("nothing here", "nothing here")]
    fn known_values_and_encodings_are_replaced(#[case] text: &str, #[case] expected: &str) {
        let store = store(&[("db", "hunter2")]);
        let token = store.get("db").unwrap().token();
        assert_eq!(
            redact_known(text, &store).get(),
            &expected.replace("{db}", &token)
        );
    }

    #[test]
    fn url_and_json_encodings_are_replaced() {
        let value = "p@ss wörd/\"x\"";
        let store = store(&[("api", value)]);
        let token = store.get("api").unwrap().token();
        for form in encodings(value) {
            let out = redact_known(&format!("<{form}>"), &store);
            assert_eq!(out.get(), &format!("<{token}>"), "form {form}");
        }
        assert!(encodings(value).contains(&"p%40ss%20w%C3%B6rd%2F%22x%22".to_owned()));
        assert!(encodings(value).contains(&"p%40ss+w%C3%B6rd%2F%22x%22".to_owned()));
        assert!(encodings(value).contains(&r#"p@ss wörd/\"x\""#.to_owned()));
    }

    #[test]
    fn longest_match_wins_and_tokens_are_not_rescanned() {
        // "secret" occurs inside every token; it must not be replaced again.
        let store = store(&[("short", "secret"), ("long", "secret-and-more")]);
        let short = store.get("short").unwrap().token();
        let long = store.get("long").unwrap().token();
        let out = redact_known("secret-and-more / secret", &store);
        assert_eq!(out.get(), &format!("{long} / {short}"));
    }

    #[test]
    fn fallback_scan_matches_the_automaton() {
        let store = store(&[("short", "secret"), ("long", "secret-and-more")]);
        let redactor = Redactor::new(&store);
        let text = "x secret-and-more ü secret c2VjcmV0 y";
        assert_eq!(&redactor.scan(text), redactor.redact(text).get());
    }

    #[test]
    fn empty_secrets_and_empty_stores_change_nothing() {
        assert_eq!(redact_known("abc", &store(&[("e", "")])).get(), "abc");
        assert_eq!(redact_known("abc", &SecretStore::new()).get(), "abc");
        assert!(Redactor::new(&store(&[("e", "")])).is_empty());
    }

    #[test]
    fn bytes_are_scrubbed_after_lossy_decoding() {
        let store = store(&[("db", "hunter2")]);
        let mut bytes = b"\xff hunter2 \xfe".to_vec();
        bytes.extend_from_slice(b"end");
        let out = redact_known_bytes(&bytes, &store);
        assert!(!out.get().contains("hunter2"));
        assert!(out.get().ends_with("end"));
    }

    #[test]
    fn redactor_debug_hides_patterns() {
        let dbg = format!("{:?}", Redactor::new(&store(&[("db", "hunter2")])));
        assert!(!dbg.contains("hunter2"));
        assert!(!dbg.contains("aHVudGVyMg"));
    }

    #[test]
    fn shadow_diff_matches_the_plan_diff_format() {
        let old = Redacted::plain("a\nb\nc\n".to_string());
        let new = Redacted::plain("a\nB\nc\n".to_string());
        let ours = Redacted::shadow_diff(&old, &new);
        let crate::plan::Diff::Update {
            unified: Some(theirs),
            ..
        } = crate::plan::Diff::unified("1 line", &old, &new)
        else {
            panic!("expected an update diff");
        };
        assert_eq!(ours.get(), theirs.get());
        assert!(ours.get().starts_with("--- current\n+++ planned\n"));
    }

    #[test]
    fn template_errors_drop_debug_info_and_scrub_values() {
        let store = store(&[("db", "hunter2")]);
        let env = minijinja::Environment::new();
        let raw = env
            .render_str(
                "{{ pw }} {{ missing.field }}",
                minijinja::context! { pw => "hunter2", other => "visible" },
            )
            .unwrap_err();
        assert!(format!("{raw:?}").contains("hunter2"));
        assert!(format!("{raw:#}").contains("hunter2"));
        let safe = template_error(raw, &store);
        assert!(safe.is(crate::error::Kind::User));
        let shown = format!(
            "{safe} | {safe:?} | {safe:#} | {} | {}",
            safe.message(),
            crate::error::pretty(&safe)
        );
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(!shown.contains("visible"), "{shown}");
        assert!(shown.contains("missing"), "{shown}");

        // Values that reach the description itself are scrubbed too.
        let raw = minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "cannot use hunter2 here",
        );
        let safe = template_error(raw, &store);
        assert!(!format!("{safe:?}").contains("hunter2"));
        assert!(format!("{safe:?}").contains("secret:db@"));
    }
}
