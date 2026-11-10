//! A commit message as a card shows it: its subject, then its body as
//! blocks re-flowed to the card's width (Git wraps at ~72 columns), with the
//! trailers that tools append (`Co-authored-by`, `Claude-Session`,
//! `Signed-off-by`, …) taken out of the prose. A squash merge repeats them
//! after every commit it folds in; here each shows once.

use crate::components::avatar::split_author;

/// A body block: what the card renders, one element each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// Lines joined into one paragraph.
    Para(String),
    /// Consecutive items, each re-flowed.
    List { ordered: bool, items: Vec<String> },
    /// Verbatim: fenced or indented code.
    Code(String),
    /// A `---` separator between parts of the body.
    Rule,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub subject: String,
    pub body: Vec<Block>,
    /// `Co-authored-by` and `Signed-off-by` values in order of appearance,
    /// unique per person.
    pub coauthors: Vec<String>,
    /// `Claude-Session` links, unique.
    pub sessions: Vec<String>,
    /// Every other trailer, unique, as `(key, value)`.
    pub trailers: Vec<(String, String)>,
}

/// Bodies up to this size (and at most this many blocks) show in full;
/// longer ones collapse to their first block behind "Read more".
const PREVIEW_CHARS: usize = 280;
const PREVIEW_BLOCKS: usize = 2;
/// What a collapsed body shows of its leading blocks, in characters.
const PREVIEW_BUDGET: usize = 200;

/// Trailer keys recognised on a line of their own, even inside prose
/// (a squash merge puts them straight after each commit's text).
const KNOWN: &[&str] = &[
    "co-authored-by",
    "claude-session",
    "signed-off-by",
    "reviewed-by",
    "acked-by",
    "tested-by",
    "reported-by",
    "suggested-by",
    "helped-by",
    "change-id",
];

impl Message {
    pub fn parse(text: &str) -> Self {
        let mut lines = text.lines();
        let subject = lines.next().unwrap_or("").trim().to_string();
        let mut p = Parser::default();
        for line in lines {
            p.line(line);
        }
        p.flush();
        let mut m = Message {
            subject,
            ..Default::default()
        };
        for (k, v) in p.trailers {
            match k.to_ascii_lowercase().as_str() {
                "co-authored-by" | "signed-off-by" => {
                    if !m.coauthors.iter().any(|a| same_person(a, &v)) {
                        m.coauthors.push(v);
                    }
                }
                "claude-session" => {
                    if !m.sessions.contains(&v) {
                        m.sessions.push(v);
                    }
                }
                _ => {
                    if !m.trailers.contains(&(k.clone(), v.clone())) {
                        m.trailers.push((k, v));
                    }
                }
            }
        }
        m.body = tidy(p.blocks);
        m
    }

    /// Whether the body is long enough to collapse behind "Read more".
    pub fn long(&self) -> bool {
        let chars: usize = self.body.iter().map(Block::len).sum();
        chars > PREVIEW_CHARS || self.body.len() > PREVIEW_BLOCKS || !self.trailers.is_empty()
    }

    /// How many leading blocks a collapsed body shows: up to the one that
    /// crosses the preview's budget (the stylesheet clips and fades it).
    pub fn preview(&self) -> usize {
        let mut total = 0;
        let fits = self
            .body
            .iter()
            .take_while(|b| {
                let room = total < PREVIEW_BUDGET;
                total += b.len();
                room
            })
            .count();
        fits.max(1).min(self.body.len())
    }

    /// Who wrote the commit: `author` first, then each co-author not already
    /// named, people before Claude. When a Claude session is linked but no
    /// author is Claude, Claude joins the list so that its sessions have
    /// somewhere to hang.
    pub fn authors(&self, author: Option<&str>) -> Vec<String> {
        let mut all: Vec<String> = author.map(str::to_string).into_iter().collect();
        let (people, claude): (Vec<_>, Vec<_>) = self.coauthors.iter().partition(|a| !is_claude(a));
        for a in people.into_iter().chain(claude) {
            if !all.iter().any(|b| same_person(a, b)) {
                all.push(a.clone());
            }
        }
        if !self.sessions.is_empty() && !all.iter().any(|a| is_claude(a)) {
            all.push("Claude <noreply@anthropic.com>".into());
        }
        all
    }
}

impl Block {
    fn len(&self) -> usize {
        match self {
            Block::Para(t) | Block::Code(t) => t.len(),
            Block::List { items, .. } => items.iter().map(String::len).sum(),
            Block::Rule => 0,
        }
    }
}

/// Whether `Name <email>` is Claude (Anthropic's agents sign as
/// `Claude … <noreply@anthropic.com>`).
pub fn is_claude(author: &str) -> bool {
    let (name, email) = split_author(author);
    email.is_some_and(|e| e.eq_ignore_ascii_case("noreply@anthropic.com"))
        || name.eq_ignore_ascii_case("claude")
}

/// The same person by address, or by name where either has none.
fn same_person(a: &str, b: &str) -> bool {
    match (split_author(a), split_author(b)) {
        ((_, Some(x)), (_, Some(y))) => x.eq_ignore_ascii_case(y),
        ((x, _), (y, _)) => x.eq_ignore_ascii_case(y),
    }
}

/// `Key: value`, where the key is a single token.
fn trailer(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once(": ")?;
    let v = v.trim();
    let token = !k.is_empty()
        && k.starts_with(|c: char| c.is_ascii_alphabetic())
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    (token && !v.is_empty()).then_some((k, v))
}

fn known_trailer(line: &str) -> Option<(&str, &str)> {
    trailer(line).filter(|(k, _)| KNOWN.iter().any(|n| k.eq_ignore_ascii_case(n)))
}

/// `- item`, `* item`, `+ item`, `1. item`, `1) item` → (ordered, text).
fn list_item(line: &str) -> Option<(bool, &str)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    if let Some(rest) = t.strip_prefix(['-', '*', '+']) {
        return rest.strip_prefix(' ').map(|r| (false, r.trim()));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    let rest = t[digits..].strip_prefix(['.', ')'])?;
    (digits > 0 && digits < 10)
        .then(|| rest.strip_prefix(' ').map(|r| (true, r.trim())))
        .flatten()
}

fn indented(line: &str) -> bool {
    line.starts_with('\t') || line.starts_with("    ")
}

fn rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
}

enum Cur {
    Para(Vec<String>),
    List { ordered: bool, items: Vec<String> },
    Code(Vec<String>),
    Fence(Vec<String>),
}

#[derive(Default)]
struct Parser {
    cur: Option<Cur>,
    blocks: Vec<Block>,
    trailers: Vec<(String, String)>,
}

impl Parser {
    fn line(&mut self, line: &str) {
        let line = line.trim_end();
        if let Some(Cur::Fence(code)) = &mut self.cur {
            if line.trim_start().starts_with("```") {
                self.flush();
            } else {
                code.push(line.to_string());
            }
            return;
        }
        if line.trim_start().starts_with("```") {
            self.flush();
            self.cur = Some(Cur::Fence(vec![]));
            return;
        }
        if line.is_empty() {
            match &mut self.cur {
                Some(Cur::Code(code)) => code.push(String::new()),
                _ => self.flush(),
            }
            return;
        }
        let in_list = matches!(self.cur, Some(Cur::List { .. }));
        if indented(line) && !in_list && matches!(self.cur, None | Some(Cur::Code(_))) {
            let code = line
                .strip_prefix('\t')
                .or_else(|| line.strip_prefix("    "))
                .unwrap_or(line);
            match &mut self.cur {
                Some(Cur::Code(c)) => c.push(code.to_string()),
                _ => self.cur = Some(Cur::Code(vec![code.to_string()])),
            }
            return;
        }
        if matches!(self.cur, Some(Cur::Code(_))) {
            self.flush();
        }
        // A known trailer on its own line leaves the prose wherever it is.
        if !matches!(self.cur, Some(Cur::Para(_)))
            && let Some((k, v)) = known_trailer(line.trim())
        {
            self.trailers.push((k.to_string(), v.to_string()));
            return;
        }
        if let Some((ordered, text)) = list_item(line).filter(|_| !rule(line)) {
            match &mut self.cur {
                Some(Cur::List { ordered: o, items }) if *o == ordered => {
                    items.push(text.to_string())
                }
                _ => {
                    self.flush();
                    self.cur = Some(Cur::List {
                        ordered,
                        items: vec![text.to_string()],
                    });
                }
            }
            return;
        }
        match &mut self.cur {
            Some(Cur::List { items, .. }) => {
                let last = items.last_mut().expect("a list has an item");
                last.push(' ');
                last.push_str(line.trim());
            }
            Some(Cur::Para(lines)) => lines.push(line.trim().to_string()),
            _ => self.cur = Some(Cur::Para(vec![line.trim().to_string()])),
        }
    }

    fn flush(&mut self) {
        let block = match self.cur.take() {
            None => return,
            Some(Cur::Para(lines)) => {
                // A paragraph of nothing but trailers is a trailer block
                // (any `Key: value` once one key is hyphenated, as Git's are).
                let parsed: Vec<_> = lines.iter().map(|l| trailer(l)).collect();
                let all = parsed.iter().all(Option::is_some);
                if all && parsed.iter().flatten().any(|(k, _)| k.contains('-')) {
                    self.trailers.extend(
                        parsed
                            .into_iter()
                            .flatten()
                            .map(|(k, v)| (k.to_string(), v.to_string())),
                    );
                    return;
                }
                // Known trailers trailing the prose come out too.
                let mut lines = lines;
                let mut tail = vec![];
                while lines.len() > 1
                    && let Some((k, v)) = lines.last().and_then(|l| known_trailer(l))
                {
                    tail.push((k.to_string(), v.to_string()));
                    lines.pop();
                }
                self.trailers.extend(tail.into_iter().rev());
                if lines.len() == 1 && rule(&lines[0]) {
                    Block::Rule
                } else {
                    Block::Para(lines.join(" "))
                }
            }
            Some(Cur::List { ordered, items }) => Block::List { ordered, items },
            Some(Cur::Code(mut lines)) | Some(Cur::Fence(mut lines)) => {
                while lines.last().is_some_and(|l| l.is_empty()) {
                    lines.pop();
                }
                Block::Code(lines.join("\n"))
            }
        };
        self.blocks.push(block);
    }
}

/// Adjacent lists of one kind become one (a loose list, or a squash merge's
/// commits without bodies); separators only separate.
fn tidy(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    for b in blocks {
        match (out.last_mut(), b) {
            (
                Some(Block::List { ordered: a, items }),
                Block::List {
                    ordered: b,
                    items: more,
                },
            ) if *a == b => items.extend(more),
            (None | Some(Block::Rule), Block::Rule) => {}
            (_, b) => out.push(b),
        }
    }
    if out.last() == Some(&Block::Rule) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(s: &str) -> Block {
        Block::Para(s.into())
    }

    fn list(items: &[&str]) -> Block {
        Block::List {
            ordered: false,
            items: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn bodies_reflow_but_keep_their_structure() {
        for (body, expected) in [
            ("one\ntwo\n\nthree", vec![para("one two"), para("three")]),
            (
                "Changes:\n- a\n  wrapped\n- b\n\nAfter.",
                vec![para("Changes:"), list(&["a wrapped", "b"]), para("After.")],
            ),
            (
                "1. first\n2) second",
                vec![Block::List {
                    ordered: true,
                    items: vec!["first".into(), "second".into()],
                }],
            ),
            (
                "Run:\n\n```\nbuckle plan\n\n  --all\n```\nthen",
                vec![
                    para("Run:"),
                    Block::Code("buckle plan\n\n  --all".into()),
                    para("then"),
                ],
            ),
            (
                "Text\n\n    code\n      more\n\nText",
                vec![
                    para("Text"),
                    Block::Code("code\n  more".into()),
                    para("Text"),
                ],
            ),
            ("* a\n\n* b", vec![list(&["a", "b"])]),
            ("a\n\n---\n\nb", vec![para("a"), Block::Rule, para("b")]),
            (
                "Note: prose with a colon",
                vec![para("Note: prose with a colon")],
            ),
        ] {
            assert_eq!(
                Message::parse(&format!("subject\n\n{body}")).body,
                expected,
                "{body:?}"
            );
        }
    }

    #[test]
    fn squash_merges_lose_their_trailers() {
        let m = Message::parse(
            "fix(reflector): pin public DNS (#55)\n\n\
             * feat(reflector): add an exit node\n\n\
             Userspace tailscaled in Docker, enrolled from a tagged\nauth key.\n\n\
             Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>\n\
             Claude-Session: https://claude.ai/code/session_a\n\n\
             * fix(reflector): pin to prophet\n\
             Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>\n\
             Claude-Session: https://claude.ai/code/session_b\n\n\
             ---------\n\n\
             Co-authored-by: Claude <noreply@anthropic.com>\n\
             Co-authored-by: Bob <bob@example.com>\n\
             Signed-off-by: Ada <ada@example.com>\n\
             Change-Id: I123",
        );
        assert_eq!(m.subject, "fix(reflector): pin public DNS (#55)");
        assert_eq!(
            m.body,
            vec![
                list(&["feat(reflector): add an exit node"]),
                para("Userspace tailscaled in Docker, enrolled from a tagged auth key."),
                list(&["fix(reflector): pin to prophet"]),
            ]
        );
        assert_eq!(
            m.coauthors,
            [
                "Claude Opus 5.5 <noreply@anthropic.com>",
                "Bob <bob@example.com>",
                "Ada <ada@example.com>"
            ]
        );
        assert_eq!(
            m.sessions,
            [
                "https://claude.ai/code/session_a",
                "https://claude.ai/code/session_b"
            ]
        );
        assert_eq!(m.trailers, [("Change-Id".to_string(), "I123".to_string())]);
    }

    #[test]
    fn authors_lead_with_the_author_and_name_each_person_once() {
        let m = Message::parse(
            "s\n\nCo-authored-by: Ada <ADA@example.com>\nCo-authored-by: Bob <bob@example.com>",
        );
        assert_eq!(
            m.authors(Some("Ada <ada@example.com>")),
            ["Ada <ada@example.com>", "Bob <bob@example.com>"]
        );
        let m = Message::parse(
            "s\n\nCo-authored-by: Claude <noreply@anthropic.com>\nCo-authored-by: Bob <bob@example.com>",
        );
        assert_eq!(
            m.authors(None),
            ["Bob <bob@example.com>", "Claude <noreply@anthropic.com>"]
        );
        // A session with no Claude author still has a Claude to hang from.
        let m = Message::parse("s\n\nClaude-Session: https://claude.ai/code/session_a");
        assert!(
            m.authors(Some("Ada <ada@example.com>"))
                .iter()
                .any(|a| is_claude(a))
        );
        assert_eq!(m.authors(Some("Claude <noreply@anthropic.com>")).len(), 1);
    }

    #[test]
    fn only_long_bodies_collapse() {
        for (body, long) in [
            ("", false),
            ("A short reason.", false),
            (&"word ".repeat(80), true),
            ("a\n\nb\n\nc", true),
            ("a\n\nChange-Id: I123", true),
            ("a\n\nSigned-off-by: Ada <ada@example.com>", false),
            ("a\n\nCo-authored-by: Ada <ada@example.com>", false),
        ] {
            assert_eq!(
                Message::parse(&format!("s\n\n{body}")).long(),
                long,
                "{body:?}"
            );
        }
    }

    #[test]
    fn previews_stop_at_the_block_that_crosses_the_budget() {
        let long = "word ".repeat(80);
        for (body, shown) in [
            ("a\n\nb\n\nc".to_string(), 3),
            (long.clone(), 1),
            (format!("a\n\n{long}\n\nb"), 2),
        ] {
            assert_eq!(
                Message::parse(&format!("s\n\n{body}")).preview(),
                shown,
                "{body:?}"
            );
        }
    }

    #[test]
    fn claude_is_recognised_by_address_or_name() {
        for (author, claude) in [
            ("Claude Opus 5.5 <noreply@anthropic.com>", true),
            ("Claude", true),
            ("Claudette <c@example.com>", false),
            ("Ada <ada@example.com>", false),
        ] {
            assert_eq!(is_claude(author), claude, "{author}");
        }
    }
}
