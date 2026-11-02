//! Parsers and rule modelling shared by the `ufw.*` task kinds.

use buckle_core::Result;
use buckle_core::system::Command;
use buckle_core::task::Cx;
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

pub(crate) fn ufw(args: &[&str]) -> Command {
    Command::new("ufw").args(args.iter().copied())
}

/// One firewall rule, as shown by `ufw status` or described by a task spec.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Rule {
    pub number: Option<u32>,
    /// `allow`, `deny`, `reject` or `limit`.
    pub action: String,
    /// `in`, `out` or `fwd`.
    pub direction: String,
    pub to_ip: Option<String>,
    /// A port, list (`80,443`) or range (`8000:8100`).
    pub port: Option<String>,
    /// `tcp` or `udp`; `None` means any.
    pub proto: Option<String>,
    pub from_ip: Option<String>,
    pub interface: Option<String>,
    /// The IPv6 twin of a rule.
    pub v6: bool,
    /// An application profile name.
    pub app: Option<String>,
    pub comment: Option<String>,
}

fn strip_host_prefix(ip: &str) -> String {
    ip.strip_suffix("/32")
        .or_else(|| ip.strip_suffix("/128"))
        .unwrap_or(ip)
        .to_owned()
}

impl Rule {
    /// The identity of a rule apart from its action and comment.
    pub(crate) fn identity(&self) -> String {
        let norm = |ip: &Option<String>| ip.as_deref().map(strip_host_prefix).unwrap_or_default();
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.direction,
            self.proto.as_deref().unwrap_or_default(),
            self.port
                .as_deref()
                .or(self.app.as_deref())
                .unwrap_or_default(),
            norm(&self.from_ip),
            norm(&self.to_ip),
            self.interface.as_deref().unwrap_or_default(),
        )
    }

    /// Whether this rule is the same selector as `other` (ignoring action and comment).
    pub(crate) fn same_selector(&self, other: &Rule) -> bool {
        self.identity() == other.identity()
    }

    /// Whether the rule is about IPv6 addresses itself (so its `(v6)` entry is the real one).
    pub(crate) fn mentions_ipv6(&self) -> bool {
        [&self.from_ip, &self.to_ip]
            .iter()
            .any(|ip| ip.as_deref().is_some_and(|ip| ip.contains(':')))
    }

    /// The arguments after `ufw` that add (or, after `delete`, remove) this rule.
    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = vec![self.action.clone(), self.direction.clone()];
        if let Some(i) = &self.interface {
            args.extend(["on".to_owned(), i.clone()]);
        }
        args.extend([
            "from".to_owned(),
            self.from_ip.clone().unwrap_or_else(|| "any".into()),
            "to".to_owned(),
            self.to_ip.clone().unwrap_or_else(|| "any".into()),
        ]);
        if let Some(p) = &self.port {
            args.extend(["port".to_owned(), p.clone()]);
        }
        if let Some(p) = &self.proto {
            args.extend(["proto".to_owned(), p.clone()]);
        }
        if let Some(app) = &self.app {
            args.extend(["app".to_owned(), app.clone()]);
        }
        args
    }

    /// A stable human key: `in tcp/22 from any to any on any`.
    pub(crate) fn key(&self) -> String {
        format!(
            "{} {}/{} from {} to {} on {}",
            self.direction,
            self.proto.as_deref().unwrap_or("any"),
            self.port
                .as_deref()
                .or(self.app.as_deref())
                .unwrap_or("any"),
            self.from_ip.as_deref().unwrap_or("any"),
            self.to_ip.as_deref().unwrap_or("any"),
            self.interface.as_deref().unwrap_or("any"),
        )
    }
}

static PORT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<port>\d+(?:[,:]\d+)*)(?:/(?P<proto>tcp|udp))?$").expect("valid regex")
});
static IP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-fA-F:.]+(?:/\d{1,3})?$").expect("valid regex"));
static NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\[\s*(\d+)\]\s*").expect("valid regex"));
static RULE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?P<to>.+?)\s+(?P<action>ALLOW|DENY|REJECT|LIMIT)(?:\s+(?P<dir>IN|OUT|FWD))?\s+(?P<from>\S.*)$",
    )
    .expect("valid regex")
});

fn looks_like_ip(s: &str) -> bool {
    IP.is_match(s) && (s.contains('.') || s.contains(':'))
}

/// What one side (To or From) of a status line says.
#[derive(Debug, Default)]
struct Endpoint {
    ip: Option<String>,
    port: Option<String>,
    proto: Option<String>,
    interface: Option<String>,
    v6: bool,
    app: Option<String>,
}

fn parse_endpoint(text: &str) -> Endpoint {
    let mut text = text.trim().to_owned();
    let mut ep = Endpoint::default();
    if text.contains("(v6)") {
        ep.v6 = true;
        text = text.replace("(v6)", "");
    }
    text = text.replace("(out)", "");
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((before, iface)) = text.split_once(" on ") {
        ep.interface = Some(iface.trim().to_owned());
        text = before.trim().to_owned();
    }
    if text.is_empty() || text.eq_ignore_ascii_case("anywhere") {
        return ep;
    }
    let tokens: Vec<&str> = text.split_whitespace().collect();
    match tokens.as_slice() {
        [one] => {
            if let Some(c) = PORT.captures(one) {
                ep.port = Some(c["port"].to_owned());
                ep.proto = c.name("proto").map(|p| p.as_str().to_owned());
            } else if looks_like_ip(one) {
                ep.ip = Some((*one).to_owned());
            } else {
                ep.app = Some((*one).to_owned());
            }
        }
        [ip, port] if looks_like_ip(ip) && PORT.is_match(port) => {
            let c = PORT.captures(port).expect("matched above");
            ep.ip = Some((*ip).to_owned());
            ep.port = Some(c["port"].to_owned());
            ep.proto = c.name("proto").map(|p| p.as_str().to_owned());
        }
        _ => ep.app = Some(text),
    }
    ep
}

/// Parses one rule line of `ufw status [numbered|verbose]`.
pub(crate) fn parse_status_rule(line: &str) -> Option<Rule> {
    let mut line = line.trim().to_owned();
    let mut number = None;
    if let Some(c) = NUMBER.captures(&line) {
        number = c[1].parse().ok();
        line = line[c.get(0)?.end()..].to_owned();
    }
    let mut comment = None;
    if let Some(pos) = line.find(" #").or_else(|| line.find("\t#")) {
        comment = Some(line[pos + 2..].trim().to_owned()).filter(|c| !c.is_empty());
        line.truncate(pos);
    }
    let caps = RULE_LINE.captures(line.trim_end())?;
    let to = parse_endpoint(&caps["to"]);
    let from = parse_endpoint(&caps["from"]);
    Some(Rule {
        number,
        action: caps["action"].to_ascii_lowercase(),
        direction: caps
            .name("dir")
            .map(|d| d.as_str().to_ascii_lowercase())
            .unwrap_or_else(|| "in".into()),
        to_ip: to.ip,
        port: to.port,
        proto: to.proto,
        from_ip: from.ip,
        interface: to.interface.or(from.interface),
        v6: to.v6 || from.v6,
        app: to.app,
        comment,
    })
}

/// Splits a line into words, honouring single and double quotes.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    words
}

/// Parses the arguments of a `ufw allow|deny|reject|limit ...` command (the
/// form `ufw show added` prints, and the form Buckle runs).
pub(crate) fn parse_rule_args(args: &[String]) -> Option<Rule> {
    let (action, rest) = args.split_first()?;
    if !matches!(action.as_str(), "allow" | "deny" | "reject" | "limit") {
        return None;
    }
    let mut rule = Rule {
        action: action.clone(),
        direction: "in".into(),
        ..Default::default()
    };
    let mut i = 0;
    while i < rest.len() {
        let word = rest[i].as_str();
        let next = rest.get(i + 1).map(String::as_str);
        match (word, next) {
            ("in" | "out", _) => {
                rule.direction = word.to_owned();
                i += 1;
            }
            ("on", Some(iface)) => {
                rule.interface = Some(iface.to_owned());
                i += 2;
            }
            ("from", Some(ip)) => {
                rule.from_ip = Some(ip.to_owned()).filter(|ip| ip != "any");
                i += 2;
            }
            ("to", Some(ip)) => {
                rule.to_ip = Some(ip.to_owned()).filter(|ip| ip != "any");
                i += 2;
            }
            ("port", Some(p)) => {
                rule.port = Some(p.to_owned());
                i += 2;
            }
            ("proto", Some(p)) => {
                rule.proto = Some(p.to_owned()).filter(|p| p != "any");
                i += 2;
            }
            ("app", Some(a)) => {
                rule.app = Some(a.to_owned());
                i += 2;
            }
            ("comment", Some(c)) => {
                rule.comment = Some(c.to_owned());
                i += 2;
            }
            (bare, _) => {
                if let Some(c) = PORT.captures(bare) {
                    rule.port = Some(c["port"].to_owned());
                    rule.proto = c.name("proto").map(|p| p.as_str().to_owned());
                } else {
                    rule.app = Some(bare.to_owned());
                }
                i += 1;
            }
        }
    }
    Some(rule)
}

/// Parses `ufw show added` output into rules (route rules are skipped).
pub(crate) fn parse_added(text: &str) -> Vec<Rule> {
    text.lines()
        .filter_map(|l| {
            let words = shell_words(l);
            (words.first().map(String::as_str) == Some("ufw"))
                .then(|| parse_rule_args(&words[1..]))
                .flatten()
        })
        .collect()
}

/// Everything `ufw status verbose|numbered` tells us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Status {
    pub active: bool,
    /// `off`, `on`, `low`, `medium`, `high` or `full`.
    pub logging: Option<String>,
    /// incoming/outgoing/routed → allow/deny/reject.
    pub defaults: BTreeMap<String, String>,
    pub rules: Vec<Rule>,
}

pub(crate) fn parse_status(text: &str) -> Status {
    static DEFAULT_ENTRY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?P<policy>\w+)\s*\((?P<dir>incoming|outgoing|routed)\)").expect("valid regex")
    });
    let mut status = Status::default();
    let mut in_rules = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(s) = trimmed.strip_prefix("Status:") {
            status.active = s.trim() == "active";
        } else if let Some(l) = trimmed.strip_prefix("Logging:") {
            // "on (low)" / "off"
            let l = l.trim();
            status.logging = Some(match (l.find('('), l.find(')')) {
                (Some(a), Some(b)) if b > a => l[a + 1..b].trim().to_owned(),
                _ => l.to_owned(),
            });
        } else if let Some(d) = trimmed.strip_prefix("Default:") {
            for c in DEFAULT_ENTRY.captures_iter(d) {
                status
                    .defaults
                    .insert(c["dir"].to_owned(), normalise_policy(&c["policy"]));
            }
        } else if trimmed.starts_with("--") {
            in_rules = true;
        } else if in_rules
            && !trimmed.is_empty()
            && let Some(rule) = parse_status_rule(trimmed)
        {
            status.rules.push(rule);
        }
    }
    status
}

/// `disabled` (routed traffic with forwarding off) behaves like `deny`.
fn normalise_policy(p: &str) -> String {
    match p.to_ascii_lowercase().as_str() {
        "disabled" => "deny".into(),
        other => other.to_owned(),
    }
}

/// Default policies from `/etc/default/ufw` (readable even when ufw is inactive).
pub(crate) fn parse_default_file(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.trim().split_once('=') else {
            continue;
        };
        let v = match v.trim().trim_matches('"') {
            "ACCEPT" => "allow",
            "DROP" => "deny",
            "REJECT" => "reject",
            _ => continue,
        };
        let dir = match k.trim() {
            "DEFAULT_INPUT_POLICY" => "incoming",
            "DEFAULT_OUTPUT_POLICY" => "outgoing",
            "DEFAULT_FORWARD_POLICY" => "routed",
            _ => continue,
        };
        out.insert(dir.to_owned(), v.to_owned());
    }
    out
}

/// `ENABLED` and `LOGLEVEL` from `/etc/ufw/ufw.conf`.
pub(crate) fn parse_ufw_conf(text: &str) -> (Option<bool>, Option<String>) {
    let mut enabled = None;
    let mut level = None;
    for line in text.lines() {
        if let Some((k, v)) = line.trim().split_once('=') {
            match k.trim() {
                "ENABLED" => enabled = Some(v.trim().eq_ignore_ascii_case("yes")),
                "LOGLEVEL" => level = Some(v.trim().to_ascii_lowercase()),
                _ => {}
            }
        }
    }
    (enabled, level)
}

/// Reads the firewall state; `numbered` also wants the rules. When ufw is
/// inactive the status command knows nothing, so defaults and logging come
/// from ufw's own files and the rules from `ufw show added`.
pub(crate) async fn read_status(cx: &Cx, numbered: bool) -> Result<Status> {
    let out = cx
        .system
        .run(ufw(&[
            "status",
            if numbered { "numbered" } else { "verbose" },
        ]))
        .await?;
    if !out.success() {
        return Err(buckle_core::error::user(
            format!(
                "`ufw status` failed: {}",
                super::sys_exec::first_lines(&out.stderr_str(), &out.stdout_str(), 3)
            ),
            &["Run Buckle as root; ufw refuses to run for other users."],
        ));
    }
    let mut status = parse_status(&out.stdout_str());
    if !status.active {
        if let Some(bytes) = cx
            .system
            .read(std::path::Path::new("/etc/default/ufw"))
            .await?
        {
            for (dir, policy) in parse_default_file(&String::from_utf8_lossy(&bytes)) {
                status.defaults.entry(dir).or_insert(policy);
            }
        }
        if let Some(bytes) = cx
            .system
            .read(std::path::Path::new("/etc/ufw/ufw.conf"))
            .await?
        {
            let (_, level) = parse_ufw_conf(&String::from_utf8_lossy(&bytes));
            if status.logging.is_none() {
                status.logging = level;
            }
        }
        if numbered {
            let added = cx.system.run(ufw(&["show", "added"])).await?;
            if added.success() {
                status.rules = parse_added(&added.stdout_str());
            }
        }
    }
    Ok(status)
}

/// Whether ufw is installed. A missing ufw is treated by tasks as "not yet
/// installed" so that a module can install it before configuring it.
pub(crate) async fn installed(cx: &Cx) -> Result<bool> {
    Ok(cx.system.which("ufw").await?.is_some())
}

// ---------------------------------------------------------------------------
// Lockout protection
// ---------------------------------------------------------------------------

/// Interfaces that count as a management path when a rule allows them wholesale.
const MANAGEMENT_INTERFACE_PREFIXES: &[&str] = &["tailscale"];

/// The ports sshd listens on: `Port` lines of `/etc/ssh/sshd_config` and
/// `/etc/ssh/sshd_config.d/*.conf`, or 22.
pub(crate) async fn ssh_ports(cx: &Cx) -> Result<Vec<u16>> {
    let mut files = vec![std::path::PathBuf::from("/etc/ssh/sshd_config")];
    if let Some(mut entries) = cx
        .system
        .list_dir(std::path::Path::new("/etc/ssh/sshd_config.d"))
        .await?
    {
        entries.sort();
        files.extend(
            entries
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "conf")),
        );
    }
    let mut ports = Vec::new();
    for file in files {
        let Some(bytes) = cx.system.read(&file).await? else {
            continue;
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let mut words = line.split_whitespace();
            if words.next().is_some_and(|w| w.eq_ignore_ascii_case("port"))
                && let Some(port) = words.next().and_then(|p| p.parse().ok())
                && !ports.contains(&port)
            {
                ports.push(port);
            }
        }
    }
    if ports.is_empty() {
        ports.push(22);
    }
    Ok(ports)
}

fn port_spec_covers(spec: &str, port: u16) -> bool {
    spec.split(',').any(|part| match part.split_once(':') {
        Some((lo, hi)) => matches!((lo.parse::<u16>(), hi.parse::<u16>()), (Ok(lo), Ok(hi)) if lo <= port && port <= hi),
        None => part.parse::<u16>() == Ok(port),
    })
}

/// Whether `rule` keeps a management path open: an incoming `allow` or
/// `limit` for one of the SSH ports (TCP or any protocol), for the `OpenSSH`
/// application profile, or for every port, either on any interface (from
/// some or all sources) or on a management interface such as `tailscale0`.
pub(crate) fn keeps_access(rule: &Rule, ssh_ports: &[u16]) -> bool {
    if !matches!(rule.action.as_str(), "allow" | "limit") || rule.direction != "in" {
        return false;
    }
    if rule.proto.as_deref().is_some_and(|p| p != "tcp") {
        return false;
    }
    if let Some(app) = &rule.app {
        return app.eq_ignore_ascii_case("OpenSSH");
    }
    match &rule.port {
        Some(port) => ssh_ports.iter().any(|p| port_spec_covers(port, *p)),
        // Every port: on any interface, or on a management interface.
        None => rule.interface.as_deref().is_none_or(|i| {
            MANAGEMENT_INTERFACE_PREFIXES
                .iter()
                .any(|p| i.starts_with(p))
        }),
    }
}

/// Rules that `ufw.rule` tasks earlier in the same plan will add. Keyed by
/// the plan's templater (one per planner, shared by the plan's tasks and its
/// apply), held weakly so entries vanish with the plan. This lets
/// `ufw.state`/`ufw.default` see an SSH rule planned before them although it
/// is not on the host yet. (A core facility for "what earlier steps of this
/// plan will do" would replace this.)
type PlannedRules = Vec<(std::sync::Weak<buckle_core::template::Templater>, Rule)>;
static PLANNED: LazyLock<std::sync::Mutex<PlannedRules>> =
    LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Notes that this plan will add `rule`.
pub(crate) fn note_planned_rule(cx: &Cx, rule: &Rule) {
    let mut planned = PLANNED.lock().unwrap_or_else(|p| p.into_inner());
    planned.retain(|(w, _)| w.strong_count() > 0);
    let me = std::sync::Arc::downgrade(&cx.templater);
    if !planned
        .iter()
        .any(|(w, r)| w.ptr_eq(&me) && r.same_selector(rule) && r.action == rule.action)
    {
        planned.push((me, rule.clone()));
    }
}

fn planned_rules(cx: &Cx) -> Vec<Rule> {
    let mut planned = PLANNED.lock().unwrap_or_else(|p| p.into_inner());
    planned.retain(|(w, _)| w.strong_count() > 0);
    let me = std::sync::Arc::downgrade(&cx.templater);
    planned
        .iter()
        .filter(|(w, _)| w.ptr_eq(&me))
        .map(|(_, r)| r.clone())
        .collect()
}

/// `None` when a rule on the host (or one planned earlier in this plan, when
/// `include_planned`) keeps SSH or a management interface reachable;
/// otherwise the reason the change would lock the operator out.
pub(crate) async fn lockout_reason(
    cx: &Cx,
    what: &str,
    include_planned: bool,
) -> Result<Option<String>> {
    let ports = ssh_ports(cx).await?;
    let mut rules = if installed(cx).await? {
        read_status(cx, true).await?.rules
    } else {
        Vec::new()
    };
    if include_planned {
        rules.extend(planned_rules(cx));
    }
    if rules.iter().any(|r| keeps_access(r, &ports)) {
        return Ok(None);
    }
    let ports = ports
        .iter()
        .map(|p| format!("{p}/tcp"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(Some(format!(
        "{what} would cut off remote access: no ufw rule allows (or limits) SSH ({ports}) or a management interface such as tailscale0, on the host or earlier in this plan. \
         Add a ufw.rule such as `{{rule: limit, to_port: 22, proto: tcp}}` or `{{rule: allow, interface: tailscale0}}` before this task, or set `allow_lockout: true` if this host is reached another way"
    )))
}

#[cfg(test)]
pub(crate) mod fixtures {
    pub const VERBOSE_ACTIVE: &str = "\
Status: active
Logging: on (low)
Default: deny (incoming), allow (outgoing), disabled (routed)
New profiles: skip

To                         Action      From
--                         ------      ----
22/tcp                     ALLOW IN    Anywhere
80,443/tcp                 ALLOW IN    10.0.0.0/8
Anywhere on tailscale0     ALLOW IN    Anywhere
22/tcp (v6)                ALLOW IN    Anywhere (v6)
Anywhere (v6) on tailscale0 ALLOW IN    Anywhere (v6)
";

    pub const VERBOSE_INACTIVE: &str = "Status: inactive\n";

    pub const NUMBERED: &str = "\
Status: active

     To                         Action      From
     --                         ------      ----
[ 1] 22/tcp                     ALLOW IN    Anywhere
[ 2] 80/tcp                     ALLOW IN    10.0.0.0/8                 # http from lan
[ 3] Anywhere on tailscale0     ALLOW IN    Anywhere
[ 4] 192.168.1.5 5432/tcp       ALLOW IN    10.1.0.0/16
[ 5] 443/tcp                    LIMIT IN    Anywhere
[ 6] Anywhere                   DENY IN     203.0.113.7
[ 7] 53/udp                     ALLOW OUT   Anywhere (out)
[ 8] Nginx Full                 ALLOW IN    Anywhere
[ 9] 8000:8100/tcp              REJECT IN   Anywhere on eth1
[10] 22/tcp (v6)                ALLOW IN    Anywhere (v6)
";

    pub const SHOW_ADDED: &str = "\
Added user rules (see 'ufw status' for running firewall):
ufw allow 22/tcp
ufw allow from 10.0.0.0/8 to any port 80 proto tcp comment 'http from lan'
ufw allow in on tailscale0
ufw limit 443/tcp
ufw route allow in on eth0 out on eth1
ufw deny in from 203.0.113.7
";
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A stateful pretend `ufw` that renders and parses the real formats.

    use super::*;
    use buckle_core::testing::{FakeSystem, ok_output};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    pub struct World {
        pub active: bool,
        pub logging: String,
        pub defaults: BTreeMap<String, String>,
        pub rules: Vec<Rule>,
    }

    impl Default for World {
        fn default() -> Self {
            Self {
                active: false,
                logging: "low".into(),
                defaults: [
                    ("incoming", "deny"),
                    ("outgoing", "allow"),
                    ("routed", "deny"),
                ]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
                rules: vec![],
            }
        }
    }

    #[derive(Clone, Default)]
    pub struct Ufw(pub Arc<Mutex<World>>);

    fn render_side(
        ip: &Option<String>,
        port: &Option<String>,
        proto: &Option<String>,
        app: &Option<String>,
        iface: &Option<String>,
        v6: bool,
    ) -> String {
        let mut parts = Vec::new();
        if let Some(ip) = ip {
            parts.push(ip.clone());
        }
        if let Some(p) = port {
            parts.push(match proto {
                Some(pr) => format!("{p}/{pr}"),
                None => p.clone(),
            });
        }
        if let Some(a) = app {
            parts.push(a.clone());
        }
        let mut s = if parts.is_empty() {
            "Anywhere".to_owned()
        } else {
            parts.join(" ")
        };
        if v6 {
            s.push_str(" (v6)");
        }
        if let Some(i) = iface {
            s.push_str(&format!(" on {i}"));
        }
        s
    }

    impl World {
        pub fn render(&self, numbered: bool) -> String {
            if !self.active {
                return "Status: inactive\n".into();
            }
            let mut out = String::from("Status: active\n");
            if !numbered {
                out.push_str(&format!("Logging: on ({})\n", self.logging));
                let d = |k: &str| {
                    let p = &self.defaults[k];
                    if k == "routed" && p == "deny" {
                        "disabled".to_owned()
                    } else {
                        p.clone()
                    }
                };
                out.push_str(&format!(
                    "Default: {} (incoming), {} (outgoing), {} (routed)\nNew profiles: skip\n",
                    d("incoming"),
                    d("outgoing"),
                    d("routed")
                ));
            }
            out.push('\n');
            let mut lines: Vec<(String, String, String)> = Vec::new();
            for r in &self.rules {
                let twin = r.from_ip.is_none() && r.to_ip.is_none();
                let versions: &[bool] = if twin { &[false, true] } else { &[false] };
                for &v6 in versions {
                    let to = render_side(&r.to_ip, &r.port, &r.proto, &r.app, &r.interface, v6);
                    let mut from = render_side(&r.from_ip, &None, &None, &None, &None, v6);
                    if r.direction == "out" {
                        from.push_str(" (out)");
                    }
                    if let Some(c) = &r.comment {
                        from.push_str(&format!(" # {c}"));
                    }
                    let action =
                        format!("{} {}", r.action.to_uppercase(), r.direction.to_uppercase());
                    lines.push((to, action, from));
                }
            }
            out.push_str(&format!("{:<27}{:<12}{}\n", "To", "Action", "From"));
            out.push_str(&format!("{:<27}{:<12}{}\n", "--", "------", "----"));
            for (i, (to, action, from)) in lines.iter().enumerate() {
                let prefix = if numbered {
                    format!("[{:>2}] ", i + 1)
                } else {
                    String::new()
                };
                out.push_str(&format!("{prefix}{to:<27}{action:<12}{from}\n"));
            }
            out
        }
    }

    impl Ufw {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn active(self) -> Self {
            self.0.lock().unwrap().active = true;
            self
        }

        pub fn default_policy(self, dir: &str, policy: &str) -> Self {
            self.0
                .lock()
                .unwrap()
                .defaults
                .insert(dir.into(), policy.into());
            self
        }

        pub fn rule(self, rule: &str) -> Self {
            let words = shell_words(rule);
            let parsed = parse_rule_args(&words).expect("test rule parses");
            self.0.lock().unwrap().rules.push(parsed);
            self
        }

        pub fn snapshot(&self) -> World {
            self.0.lock().unwrap().clone()
        }

        pub fn attach(&self, sys: FakeSystem) -> FakeSystem {
            let world = self.0.clone();
            sys.on_command("ufw", &[], move |cmd| {
                let mut w = world.lock().unwrap();
                let args: Vec<String> = cmd
                    .args
                    .iter()
                    .filter(|a| a.as_str() != "--force")
                    .cloned()
                    .collect();
                match args.first().map(String::as_str) {
                    Some("status") => {
                        ok_output(&w.render(args.get(1).map(String::as_str) == Some("numbered")))
                    }
                    Some("show") => {
                        let mut out = String::from(
                            "Added user rules (see 'ufw status' for running firewall):\n",
                        );
                        for r in &w.rules {
                            out.push_str(&format!("ufw {}\n", r.args().join(" ")));
                        }
                        ok_output(&out)
                    }
                    Some("enable") => {
                        w.active = true;
                        ok_output("Firewall is active and enabled on system startup\n")
                    }
                    Some("disable") => {
                        w.active = false;
                        ok_output("Firewall stopped and disabled on system startup\n")
                    }
                    Some("logging") => {
                        w.logging = args.get(1).cloned().unwrap_or_default();
                        ok_output("Logging enabled\n")
                    }
                    Some("default") => {
                        if let [_, policy, dir] = args.as_slice() {
                            w.defaults.insert(dir.clone(), policy.clone());
                        }
                        ok_output("Default policy changed\n")
                    }
                    Some("delete") => {
                        if let Some(target) = parse_rule_args(&args[1..]) {
                            w.rules.retain(|r| {
                                !(r.action == target.action && r.same_selector(&target))
                            });
                        }
                        ok_output("Rule deleted\n")
                    }
                    _ => {
                        if let Some(rule) = parse_rule_args(&args) {
                            let dup = w
                                .rules
                                .iter()
                                .any(|r| r.action == rule.action && r.same_selector(&rule));
                            if !dup {
                                w.rules.push(rule);
                            }
                            ok_output("Rule added\n")
                        } else {
                            ok_output("")
                        }
                    }
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use rstest::rstest;

    #[test]
    fn verbose_status_parses_header_and_rules() {
        let s = parse_status(VERBOSE_ACTIVE);
        assert!(s.active);
        assert_eq!(s.logging.as_deref(), Some("low"));
        assert_eq!(s.defaults["incoming"], "deny");
        assert_eq!(s.defaults["outgoing"], "allow");
        assert_eq!(
            s.defaults["routed"], "deny",
            "'disabled' routed policy behaves as deny"
        );
        assert_eq!(s.rules.len(), 5);
        assert_eq!(s.rules[1].port.as_deref(), Some("80,443"));
        assert_eq!(s.rules[1].from_ip.as_deref(), Some("10.0.0.0/8"));
        assert_eq!(s.rules[2].interface.as_deref(), Some("tailscale0"));
        assert!(s.rules[3].v6 && !s.rules[0].v6);
    }

    #[test]
    fn inactive_status_has_nothing_else() {
        let s = parse_status(VERBOSE_INACTIVE);
        assert!(!s.active && s.rules.is_empty() && s.defaults.is_empty() && s.logging.is_none());
    }

    #[test]
    fn numbered_status_with_many_rules_parses() {
        let s = parse_status(NUMBERED);
        assert_eq!(s.rules.len(), 10);
        let by_num = |n: u32| s.rules.iter().find(|r| r.number == Some(n)).unwrap();
        assert_eq!(by_num(1).port.as_deref(), Some("22"));
        assert_eq!(by_num(1).proto.as_deref(), Some("tcp"));
        assert_eq!(by_num(2).comment.as_deref(), Some("http from lan"));
        assert_eq!(by_num(2).port.as_deref(), Some("80"));
        assert_eq!(by_num(3).interface.as_deref(), Some("tailscale0"));
        assert_eq!(by_num(3).port, None);
        assert_eq!(by_num(4).to_ip.as_deref(), Some("192.168.1.5"));
        assert_eq!(by_num(4).port.as_deref(), Some("5432"));
        assert_eq!(by_num(4).from_ip.as_deref(), Some("10.1.0.0/16"));
        assert_eq!(by_num(5).action, "limit");
        assert_eq!(by_num(6).action, "deny");
        assert_eq!(by_num(6).from_ip.as_deref(), Some("203.0.113.7"));
        assert_eq!(by_num(7).direction, "out");
        assert_eq!(by_num(7).proto.as_deref(), Some("udp"));
        assert_eq!(by_num(8).app.as_deref(), Some("Nginx Full"));
        assert_eq!(by_num(9).action, "reject");
        assert_eq!(by_num(9).port.as_deref(), Some("8000:8100"));
        assert_eq!(by_num(9).interface.as_deref(), Some("eth1"));
        assert!(by_num(10).v6);
    }

    #[test]
    fn show_added_parses_and_skips_route_rules() {
        let rules = parse_added(SHOW_ADDED);
        assert_eq!(rules.len(), 5);
        assert_eq!(rules[0].port.as_deref(), Some("22"));
        assert_eq!(rules[1].comment.as_deref(), Some("http from lan"));
        assert_eq!(rules[1].from_ip.as_deref(), Some("10.0.0.0/8"));
        assert_eq!(rules[2].interface.as_deref(), Some("tailscale0"));
        assert_eq!(rules[3].action, "limit");
        assert_eq!(rules[4].from_ip.as_deref(), Some("203.0.113.7"));
    }

    #[rstest]
    #[case(
        "allow in from any to any port 22 proto tcp",
        "in tcp/22 from any to any on any"
    )]
    #[case(
        "allow in on eth0 from 10.0.0.0/8 to any port 80 proto tcp",
        "in tcp/80 from 10.0.0.0/8 to any on eth0"
    )]
    #[case(
        "deny out from any to 1.2.3.4",
        "out any/any from any to 1.2.3.4 on any"
    )]
    fn rules_have_stable_keys_and_round_trip_through_args(#[case] args: &str, #[case] key: &str) {
        let words: Vec<String> = args.split(' ').map(str::to_owned).collect();
        let rule = parse_rule_args(&words).unwrap();
        assert_eq!(rule.key(), key);
        assert_eq!(rule.args().join(" "), args);
    }

    #[test]
    fn host_prefixes_do_not_change_identity() {
        let a = parse_rule_args(&["allow".into(), "from".into(), "10.0.0.5/32".into()]).unwrap();
        let b = parse_rule_args(&["allow".into(), "from".into(), "10.0.0.5".into()]).unwrap();
        assert!(a.same_selector(&b));
    }

    #[test]
    fn default_files_parse() {
        let d = parse_default_file(
            "IPV6=yes\nDEFAULT_INPUT_POLICY=\"DROP\"\nDEFAULT_OUTPUT_POLICY=\"ACCEPT\"\nDEFAULT_FORWARD_POLICY=\"DROP\"\n",
        );
        assert_eq!(d["incoming"], "deny");
        assert_eq!(d["outgoing"], "allow");
        assert_eq!(d["routed"], "deny");
        assert_eq!(
            parse_ufw_conf("# comment\nENABLED=no\nLOGLEVEL=low\n"),
            (Some(false), Some("low".into()))
        );
    }

    #[test]
    fn shell_words_honours_quotes() {
        assert_eq!(
            shell_words("ufw allow 22/tcp comment 'a b'"),
            vec!["ufw", "allow", "22/tcp", "comment", "a b"]
        );
    }
}
