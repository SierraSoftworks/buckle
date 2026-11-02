//! Unified diffs and secret reference tokens, ready to draw.
//!
//! Plans and drift reports carry text that was redacted on the server: every
//! secret value is already a `«secret:NAME@fp»` token by the time it reaches
//! the UI. These helpers split that text so the page can show the tokens as
//! tokens, which is what makes a rotation (`@3f9a` → `@b21c`) legible.

/// What one line of a unified diff is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// `--- current` / `+++ planned`.
    File,
    /// `@@ -1,3 +1,3 @@`.
    Hunk,
    Add,
    Del,
    Context,
    /// `\ No newline at end of file`.
    Meta,
}

impl LineKind {
    pub fn class(self) -> &'static str {
        match self {
            LineKind::File => "diff__line--file",
            LineKind::Hunk => "diff__line--hunk",
            LineKind::Add => "diff__line--add",
            LineKind::Del => "diff__line--del",
            LineKind::Context => "diff__line--context",
            LineKind::Meta => "diff__line--meta",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'a> {
    pub kind: LineKind,
    pub text: &'a str,
}

pub fn lines(unified: &str) -> Vec<Line<'_>> {
    unified
        .lines()
        .map(|text| {
            let kind = if text.starts_with("+++") || text.starts_with("---") {
                LineKind::File
            } else if text.starts_with("@@") {
                LineKind::Hunk
            } else if text.starts_with('+') {
                LineKind::Add
            } else if text.starts_with('-') {
                LineKind::Del
            } else if text.starts_with('\\') {
                LineKind::Meta
            } else {
                LineKind::Context
            };
            Line { kind, text }
        })
        .collect()
}

/// Lines added and removed.
pub fn stats(unified: &str) -> (usize, usize) {
    lines(unified)
        .iter()
        .fold((0, 0), |(a, d), l| match l.kind {
            LineKind::Add => (a + 1, d),
            LineKind::Del => (a, d + 1),
            _ => (a, d),
        })
}

/// A piece of redacted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment<'a> {
    Text(&'a str),
    /// `«secret:NAME@fp»`: the secret's name and the fingerprint of its value.
    Secret {
        raw: &'a str,
        name: &'a str,
        fingerprint: &'a str,
    },
    /// `«fact:PATH»`: a host value that varies across the fleet
    /// (`host.id`, `host.facts.os.codename`), in a fleet-wide plan.
    Fact {
        raw: &'a str,
        path: &'a str,
    },
}

const OPEN: char = '\u{ab}';
const CLOSE: char = '\u{bb}';
const SECRET: &str = "\u{ab}secret:";
const FACT: &str = buckle_api::FACT_TOKEN_PREFIX;

/// Splits text around secret and fact tokens.
pub fn segments(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut rest = text;
    let next = |s: &str| {
        [SECRET, FACT]
            .into_iter()
            .filter_map(|p| s.find(p).map(|i| (i, p)))
            .min()
    };
    while let Some((start, prefix)) = next(rest) {
        let Some(len) = rest[start..].find(CLOSE) else {
            break;
        };
        let raw = &rest[start..start + len + CLOSE.len_utf8()];
        let inner = &raw[prefix.len()..raw.len() - CLOSE.len_utf8()];
        if inner.contains(OPEN) {
            // Not a token: an opening guillemet inside it. Keep it as text.
            out.push(Segment::Text(&rest[..start + prefix.len()]));
            rest = &rest[start + prefix.len()..];
            continue;
        }
        if start > 0 {
            out.push(Segment::Text(&rest[..start]));
        }
        out.push(if prefix == FACT {
            Segment::Fact { raw, path: inner }
        } else {
            let (name, fingerprint) = inner.rsplit_once('@').unwrap_or((inner, ""));
            Segment::Secret {
                raw,
                name,
                fingerprint,
            }
        });
        rest = &rest[start + raw.len()..];
    }
    if !rest.is_empty() {
        out.push(Segment::Text(rest));
    }
    out
}

/// The names of the secrets whose fingerprint differs between the removed and
/// added lines of a diff: a rotation, in the reviewer's terms.
pub fn rotated_secrets(unified: &str) -> Vec<String> {
    let mut before = std::collections::BTreeMap::new();
    let mut after = std::collections::BTreeMap::new();
    for line in lines(unified) {
        let target = match line.kind {
            LineKind::Del => &mut before,
            LineKind::Add => &mut after,
            _ => continue,
        };
        for seg in segments(line.text) {
            if let Segment::Secret {
                name, fingerprint, ..
            } = seg
            {
                target.insert(name.to_string(), fingerprint.to_string());
            }
        }
    }
    after
        .into_iter()
        .filter(|(name, fp)| before.get(name).is_some_and(|old| old != fp))
        .map(|(name, _)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "--- current\n+++ planned\n@@ -1,2 +1,2 @@\n datacenter = \"home\"\n-servers = [\"«secret:nomad_server@3f9a»\"]\n+servers = [\"«secret:nomad_server@b21c»\"]\n\\ No newline at end of file\n";

    #[test]
    fn diff_lines_are_classified() {
        let kinds: Vec<LineKind> = lines(DIFF).iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::File,
                LineKind::File,
                LineKind::Hunk,
                LineKind::Context,
                LineKind::Del,
                LineKind::Add,
                LineKind::Meta
            ]
        );
        assert_eq!(stats(DIFF), (1, 1));
    }

    #[test]
    fn secret_tokens_are_split_out_of_text() {
        let segs = segments("servers = [\"«secret:nomad_server@3f9a»\", «secret:a.b@ff»]");
        assert_eq!(
            segs,
            vec![
                Segment::Text("servers = [\""),
                Segment::Secret {
                    raw: "«secret:nomad_server@3f9a»",
                    name: "nomad_server",
                    fingerprint: "3f9a"
                },
                Segment::Text("\", "),
                Segment::Secret {
                    raw: "«secret:a.b@ff»",
                    name: "a.b",
                    fingerprint: "ff"
                },
                Segment::Text("]"),
            ]
        );
        assert_eq!(segments("plain"), vec![Segment::Text("plain")]);
        assert_eq!(segments(""), Vec::<Segment>::new());
        // An unterminated token is left alone rather than swallowing the line.
        assert_eq!(
            segments("x «secret:broken"),
            vec![Segment::Text("x «secret:broken")]
        );
    }

    #[test]
    fn fact_tokens_are_split_out_alongside_secrets() {
        assert_eq!(
            segments("name = «fact:host.id» key = «secret:k@1»"),
            vec![
                Segment::Text("name = "),
                Segment::Fact {
                    raw: "«fact:host.id»",
                    path: "host.id"
                },
                Segment::Text(" key = "),
                Segment::Secret {
                    raw: "«secret:k@1»",
                    name: "k",
                    fingerprint: "1"
                },
            ]
        );
    }

    #[test]
    fn a_changed_fingerprint_reads_as_a_rotation() {
        assert_eq!(rotated_secrets(DIFF), vec!["nomad_server"]);
        assert!(rotated_secrets("-a «secret:x@1»\n+a «secret:x@1» b\n").is_empty());
    }
}
