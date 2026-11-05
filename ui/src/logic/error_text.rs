//! Reads a flattened `human-errors` dump back into its parts: the message,
//! the "This was caused by:" chain and the "To try and fix this, you can:"
//! advice, so a rejected commit's reason renders like any other error.

use buckle_api::{ErrorKind, ErrorView};

const CAUSES: &str = "This was caused by:";
const ADVICE: &str = "To try and fix this, you can:";

/// Splits `text` at its section markers. Line breaks are optional: the dump
/// is also accepted with its whitespace collapsed to single spaces.
pub fn parse(text: &str) -> ErrorView {
    let text = text.trim();
    let (head, rest) = split_at_marker(text, &[CAUSES, ADVICE]);
    let (causes_text, advice_text) = match rest {
        Some((m, rest)) if m == CAUSES => match rest.split_once(ADVICE) {
            Some((c, a)) => (c, a),
            None => (rest, ""),
        },
        Some((_, rest)) => ("", rest),
        None => ("", ""),
    };

    let (head, kind) = strip_kind(head.trim());
    // `description: cause` is how a flattened chain reads when the dump
    // carries no sections; the first `.: ` is the join.
    let (message, inline_cause) = match head.split_once(".: ") {
        Some((m, c)) => (format!("{m}."), Some(c.trim().to_owned())),
        None => (head.to_owned(), None),
    };

    let mut causes: Vec<String> = Vec::new();
    for c in inline_cause.into_iter().chain(items(causes_text)) {
        let c = strip_kind(&c).0.to_owned();
        if !c.is_empty() && !causes.contains(&c) && c != message {
            causes.push(c);
        }
    }
    let mut advice: Vec<String> = Vec::new();
    for a in items(advice_text) {
        // Advice sometimes arrives with its cause chain glued on after `.: `.
        let a = a
            .split_once(".: ")
            .map_or(a.clone(), |(a, _)| format!("{a}."));
        if !a.is_empty() && !advice.contains(&a) {
            advice.push(a);
        }
    }

    ErrorView {
        kind,
        message,
        causes,
        advice,
    }
}

fn split_at_marker<'a>(
    text: &'a str,
    markers: &[&'static str],
) -> (&'a str, Option<(&'static str, &'a str)>) {
    markers
        .iter()
        .filter_map(|m| text.find(m).map(|i| (i, *m)))
        .min()
        .map_or((text, None), |(i, m)| {
            (&text[..i], Some((m, &text[i + m.len()..])))
        })
}

fn strip_kind(text: &str) -> (&str, ErrorKind) {
    for (suffix, kind) in [
        ("(User error)", ErrorKind::User),
        ("(System error)", ErrorKind::System),
    ] {
        if let Some(t) = text.trim_end().strip_suffix(suffix) {
            return (t.trim_end(), kind);
        }
    }
    (text, ErrorKind::System)
}

/// The `- item` entries of a section, one per line or, when the line breaks
/// were lost, one per ` - `.
fn items(section: &str) -> Vec<String> {
    let section = section.trim();
    let parts: Vec<&str> = if section.contains('\n') {
        section.lines().collect()
    } else {
        section.split(" - ").collect()
    };
    parts
        .into_iter()
        .map(|p| {
            p.trim()
                .trim_start_matches(['-', '*', '•'])
                .trim()
                .to_owned()
        })
        .filter(|p| !p.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const LINES: &str = "The workload is invalid.: A YAML document could not be parsed. (User error)\n\
        This was caused by:\n - resources[0]: unknown field\n\
        To try and fix this, you can:\n - Check the file for syntax errors.\n - Check the file for syntax errors.: resources[0]: unknown field";

    #[test]
    fn splits_message_causes_and_advice() {
        for text in [LINES.to_owned(), LINES.replace('\n', " ")] {
            let e = parse(&text);
            assert_eq!(e.kind, ErrorKind::User);
            assert_eq!(e.message, "The workload is invalid.");
            assert_eq!(
                e.causes,
                vec![
                    "A YAML document could not be parsed.",
                    "resources[0]: unknown field"
                ]
            );
            assert_eq!(e.advice, vec!["Check the file for syntax errors."]);
        }
    }

    #[test]
    fn plain_text_is_just_a_message() {
        let e = parse("Something broke");
        assert_eq!(e.message, "Something broke");
        assert!(e.causes.is_empty() && e.advice.is_empty());
    }
}
