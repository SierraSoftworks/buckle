//! A rollout's plan across the fleet: the union of every host's
//! [`PlanView`], each change marked with the hosts it applies to.
//!
//! * Facts as placeholders: before hosts' plans are compared, host-specific
//!   values (`host.id`, `host.labels.*`, `host.facts.*`) that differ across
//!   the fleet are replaced by `«fact:PATH»` tokens, so a change that only
//!   differs by a host's name or address reads as one change.
//! * Variants: a step whose tokenised plan still differs between hosts keeps
//!   one variant per distinct plan, each with its hosts.
//! * Exemplars: every fact path used in a token comes with a sample of its
//!   values across hosts, for the reader to see what it stands for.
//!
//! Tokens are a display device: the hosts' plans are untouched and remain
//! the reference for what each host applies.

use crate::plan::{HandlerView, ModuleNote, PlanView, RehomeView, RemovalView, StepView};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Opens a fact token: `«fact:host.facts.os.codename»`.
pub const FACT_TOKEN_PREFIX: &str = "\u{ab}fact:";
const OPEN: char = '\u{ab}';
const CLOSE: char = '\u{bb}';

/// The shortest value replaced by a token: shorter ones (`4`, `on`) match
/// too much unrelated text.
const MIN_FACT_LEN: usize = 3;
/// Exemplars kept per fact.
const SAMPLES: usize = 5;

/// One host's input to [`FleetPlan::build`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetHost {
    pub host: String,
    /// The host's plan of the rollout's revision, if one is readable.
    pub plan: Option<PlanView>,
    /// The host as templates see it: `{"id", "labels", "facts"}` (the
    /// `host` object of the template context).
    pub context: serde_json::Value,
}

/// A value shared by some hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shared<T> {
    pub hosts: Vec<String>,
    pub value: T,
}

/// The union of a rollout's plans.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPlan {
    /// Hosts whose plans are included; a change shared by all of them
    /// applies fleet-wide.
    pub hosts: Vec<String>,
    /// Hosts in the rollout without a readable plan of its revision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unplanned: Vec<String>,
    #[serde(default)]
    pub modules: Vec<FleetModule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<Shared<RemovalView>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rehomed: Vec<Shared<RehomeView>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<Shared<ModuleNote>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<Shared<ModuleNote>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indeterminate: Vec<Shared<ModuleNote>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Shared<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Shared<String>>,
    /// Exemplars for every fact token in the plan, by path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<FactSamples>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetModule {
    pub module: String,
    /// Hosts the module is planned on.
    pub hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default)]
    pub steps: Vec<FleetStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<Shared<HandlerView>>,
}

/// One task's resource across hosts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStep {
    pub task: String,
    pub kind: String,
    /// The resource key, tokenised.
    pub key: String,
    /// One entry per distinct (tokenised) change or block; empty when the
    /// resource already matches everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<Shared<StepView>>,
    /// Hosts where the resource already matches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unchanged: Vec<String>,
}

impl FleetStep {
    /// `kind[key]`.
    pub fn resource(&self) -> String {
        format!("{}[{}]", self.kind, self.key)
    }

    /// Hosts the step changes (or is blocked on).
    pub fn changed_hosts(&self) -> impl Iterator<Item = &String> {
        self.variants.iter().flat_map(|v| v.hosts.iter())
    }
}

/// Values a fact takes across the fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactSamples {
    /// `host.facts.os.codename`.
    pub path: String,
    /// How many distinct values the fleet holds.
    pub distinct: u32,
    /// Up to a handful of hosts' values, distinct values first.
    pub samples: Vec<FactSample>,
    /// Every planned host's value, to render a change as one host sees it
    /// ([`FleetPlan::render_step`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactSample {
    pub host: String,
    pub value: String,
}

impl FleetPlan {
    /// Unions `hosts`' plans (in the given order), tokenising the facts
    /// that vary between them.
    pub fn build(hosts: &[FleetHost]) -> Self {
        let leaves: Vec<Vec<(String, String)>> = hosts
            .iter()
            .map(|h| {
                let mut out = Vec::new();
                flatten("host", &h.context, &mut out);
                out
            })
            .collect();

        // Paths whose value differs between planned hosts.
        let mut values: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (h, l) in hosts.iter().zip(&leaves) {
            if h.plan.is_some() {
                for (p, v) in l {
                    values.entry(p.as_str()).or_default().insert(v.as_str());
                }
            }
        }
        let varying: BTreeSet<&str> = values
            .iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(p, _)| *p)
            .collect();

        let mut out = FleetPlan::default();
        let mut used = BTreeSet::new();
        for (h, l) in hosts.iter().zip(&leaves) {
            let Some(plan) = &h.plan else {
                out.unplanned.push(h.host.clone());
                continue;
            };
            let tok = Tokeniser::new(
                l.iter()
                    .filter(|(p, v)| varying.contains(p.as_str()) && v.len() >= MIN_FACT_LEN),
            );
            out.add(&h.host, &tok.plan(plan));
            used.extend(tok.used.into_inner());
        }

        out.facts = used
            .into_iter()
            .map(|path| samples(&path, hosts, &leaves))
            .collect();
        out
    }

    fn add(&mut self, host: &str, plan: &PlanView) {
        self.hosts.push(host.to_owned());
        for m in &plan.modules {
            let fm = match self.modules.iter_mut().position(|x| x.module == m.module) {
                Some(i) => &mut self.modules[i],
                None => {
                    self.modules.push(FleetModule {
                        module: m.module.clone(),
                        needs: m.needs.clone(),
                        ..Default::default()
                    });
                    self.modules.last_mut().expect("just pushed")
                }
            };
            fm.hosts.push(host.to_owned());
            for s in &m.steps {
                let fs = match fm
                    .steps
                    .iter_mut()
                    .position(|x| x.task == s.task && x.kind == s.kind && x.key == s.key)
                {
                    Some(i) => &mut fm.steps[i],
                    None => {
                        fm.steps.push(FleetStep {
                            task: s.task.clone(),
                            kind: s.kind.clone(),
                            key: s.key.clone(),
                            ..Default::default()
                        });
                        fm.steps.last_mut().expect("just pushed")
                    }
                };
                if s.outcome == crate::StepOutcome::NoOp {
                    fs.unchanged.push(host.to_owned());
                } else {
                    share(&mut fs.variants, host, s);
                }
            }
            for h in &m.handlers {
                share(&mut fm.handlers, host, h);
            }
        }
        for r in &plan.removals {
            share(&mut self.removals, host, r);
        }
        for r in &plan.rehomed {
            share(&mut self.rehomed, host, r);
        }
        for (list, notes) in [
            (&mut self.deferred, &plan.deferred),
            (&mut self.held, &plan.held),
            (&mut self.indeterminate, &plan.indeterminate),
        ] {
            for n in notes {
                share(list, host, n);
            }
        }
        for s in &plan.skipped {
            share(&mut self.skipped, host, s);
        }
        for w in &plan.warnings {
            share(&mut self.warnings, host, w);
        }
    }

    /// The plan as far as it concerns `keep` (a phase's hosts): other hosts
    /// left out, and whatever only they had dropped. Tokens stay as built
    /// for the whole fleet; exemplars are drawn from the kept hosts.
    pub fn restrict(&self, keep: &[String]) -> FleetPlan {
        let hosts = |h: &[String]| -> Vec<String> {
            h.iter().filter(|h| keep.contains(h)).cloned().collect()
        };
        fn shared<T: Clone>(
            list: &[Shared<T>],
            hosts: &impl Fn(&[String]) -> Vec<String>,
        ) -> Vec<Shared<T>> {
            list.iter()
                .map(|s| Shared {
                    hosts: hosts(&s.hosts),
                    value: s.value.clone(),
                })
                .filter(|s| !s.hosts.is_empty())
                .collect()
        }
        FleetPlan {
            hosts: hosts(&self.hosts),
            unplanned: hosts(&self.unplanned),
            modules: self
                .modules
                .iter()
                .map(|m| FleetModule {
                    module: m.module.clone(),
                    hosts: hosts(&m.hosts),
                    needs: m.needs.clone(),
                    steps: m
                        .steps
                        .iter()
                        .map(|s| FleetStep {
                            variants: shared(&s.variants, &hosts),
                            unchanged: hosts(&s.unchanged),
                            ..s.clone()
                        })
                        .filter(|s| !s.variants.is_empty() || !s.unchanged.is_empty())
                        .collect(),
                    handlers: shared(&m.handlers, &hosts),
                })
                .filter(|m| !m.hosts.is_empty())
                .collect(),
            removals: shared(&self.removals, &hosts),
            rehomed: shared(&self.rehomed, &hosts),
            deferred: shared(&self.deferred, &hosts),
            held: shared(&self.held, &hosts),
            indeterminate: shared(&self.indeterminate, &hosts),
            skipped: shared(&self.skipped, &hosts),
            warnings: shared(&self.warnings, &hosts),
            facts: self
                .facts
                .iter()
                .map(|f| {
                    let kept = self.hosts.iter().filter(|h| keep.contains(h));
                    exemplars(
                        &f.path,
                        kept.filter_map(|h| f.values.get(h).map(|v| (h.as_str(), v.as_str()))),
                    )
                })
                .collect(),
        }
    }

    /// Whether `hosts` covers every planned host.
    pub fn is_fleet_wide(&self, hosts: &[String]) -> bool {
        self.hosts.iter().all(|h| hosts.contains(h))
    }

    /// `text` as `host` renders it: each fact token replaced by the host's
    /// value (tokens the host has no value for are left in place).
    pub fn render(&self, host: &str, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(FACT_TOKEN_PREFIX) {
            let body = &rest[start + FACT_TOKEN_PREFIX.len()..];
            let Some(end) = body.find(CLOSE) else {
                break;
            };
            let path = &body[..end];
            out.push_str(&rest[..start]);
            match self.fact(path).and_then(|f| f.values.get(host)) {
                Some(v) => out.push_str(v),
                None => out.push_str(
                    &rest[start..start + FACT_TOKEN_PREFIX.len() + end + CLOSE.len_utf8()],
                ),
            }
            rest = &body[end + CLOSE.len_utf8()..];
        }
        out.push_str(rest);
        out
    }

    /// A (tokenised) step as `host` planned it.
    pub fn render_step(&self, host: &str, step: &StepView) -> StepView {
        let r = |t: &String| self.render(host, t);
        StepView {
            key: r(&step.key),
            name: step.name.as_ref().map(r),
            summary: r(&step.summary),
            unified: step.unified.as_ref().map(r),
            preview: step.preview.as_ref().map(r),
            ..step.clone()
        }
    }

    /// The exemplars of a fact token's path.
    pub fn fact(&self, path: &str) -> Option<&FactSamples> {
        self.facts.iter().find(|f| f.path == path)
    }
}

fn share<T: PartialEq + Clone>(list: &mut Vec<Shared<T>>, host: &str, value: &T) {
    match list.iter_mut().find(|s| &s.value == value) {
        Some(s) => s.hosts.push(host.to_owned()),
        None => list.push(Shared {
            hosts: vec![host.to_owned()],
            value: value.clone(),
        }),
    }
}

/// String leaves of `value` by template path (`host.facts.net.ipv4[0]`).
fn flatten(path: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::String(s) => out.push((path.to_owned(), s.clone())),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let simple = !k.is_empty()
                    && !k.starts_with(|c: char| c.is_ascii_digit())
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                let child = if simple {
                    format!("{path}.{k}")
                } else {
                    format!("{path}[{}]", serde_json::Value::String(k.clone()))
                };
                flatten(&child, v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten(&format!("{path}[{i}]"), v, out);
            }
        }
        _ => {}
    }
}

fn samples(path: &str, hosts: &[FleetHost], leaves: &[Vec<(String, String)>]) -> FactSamples {
    let all = hosts.iter().zip(leaves).filter_map(|(h, l)| {
        l.iter()
            .find(|(p, _)| p == path)
            .map(|(_, v)| (h.host.as_str(), v.as_str()))
    });
    exemplars(path, all)
}

/// Exemplars of `path` from `(host, value)` pairs, in host order.
fn exemplars<'a>(path: &str, all: impl Iterator<Item = (&'a str, &'a str)>) -> FactSamples {
    let all: Vec<FactSample> = all
        .map(|(host, value)| FactSample {
            host: host.to_owned(),
            value: value.to_owned(),
        })
        .collect();
    let values = all
        .iter()
        .map(|s| (s.host.clone(), s.value.clone()))
        .collect();
    let distinct: BTreeSet<&str> = all.iter().map(|s| s.value.as_str()).collect();
    let distinct = distinct.len() as u32;
    // Distinct values first, then repeats, keeping host order within each.
    let mut seen = BTreeSet::new();
    let (first, rest): (Vec<_>, Vec<_>) =
        all.into_iter().partition(|s| seen.insert(s.value.clone()));
    FactSamples {
        path: path.to_owned(),
        distinct,
        samples: first.into_iter().chain(rest).take(SAMPLES).collect(),
        values,
    }
}

/// Replaces one host's fact values with tokens.
struct Tokeniser {
    /// `(value, path)`, longest value first; among equal values, the
    /// shortest path (`host.id` over `host.facts.os.hostname`).
    facts: Vec<(String, String)>,
    used: std::cell::RefCell<BTreeSet<String>>,
}

impl Tokeniser {
    fn new<'a>(leaves: impl Iterator<Item = &'a (String, String)>) -> Self {
        let mut facts: Vec<(String, String)> =
            leaves.map(|(p, v)| (v.clone(), p.clone())).collect();
        facts.sort_by(|a, b| {
            b.0.len()
                .cmp(&a.0.len())
                .then(a.1.len().cmp(&b.1.len()))
                .then(a.1.cmp(&b.1))
        });
        facts.dedup_by(|a, b| a.0 == b.0);
        Self {
            facts,
            used: Default::default(),
        }
    }

    /// `text` with every fact value that stands as a word of its own (not
    /// inside a longer word or an existing `«…»` token) replaced.
    fn text(&self, text: &str) -> String {
        if self.facts.is_empty() {
            return text.to_owned();
        }
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let mut out = String::with_capacity(text.len());
        let mut prev: Option<char> = None;
        let mut i = 0;
        'scan: while i < text.len() {
            let rest = &text[i..];
            if rest.starts_with(OPEN)
                && let Some(end) = rest.find(CLOSE)
            {
                let token = &rest[..end + CLOSE.len_utf8()];
                out.push_str(token);
                prev = Some(CLOSE);
                i += token.len();
                continue;
            }
            if !word(prev) {
                for (value, path) in &self.facts {
                    if rest.starts_with(value.as_str()) && !word(rest[value.len()..].chars().next())
                    {
                        out.push_str(FACT_TOKEN_PREFIX);
                        out.push_str(path);
                        out.push(CLOSE);
                        self.used.borrow_mut().insert(path.clone());
                        prev = value.chars().last();
                        i += value.len();
                        continue 'scan;
                    }
                }
            }
            let c = rest.chars().next().expect("i is within text");
            out.push(c);
            prev = Some(c);
            i += c.len_utf8();
        }
        out
    }

    fn opt(&self, text: &Option<String>) -> Option<String> {
        text.as_deref().map(|t| self.text(t))
    }

    fn plan(&self, plan: &PlanView) -> PlanView {
        let note = |n: &ModuleNote| ModuleNote {
            reason: self.text(&n.reason),
            ..n.clone()
        };
        let mut p = plan.clone();
        for m in &mut p.modules {
            for s in &mut m.steps {
                *s = StepView {
                    key: self.text(&s.key),
                    name: self.opt(&s.name),
                    summary: self.text(&s.summary),
                    unified: self.opt(&s.unified),
                    preview: self.opt(&s.preview),
                    ..s.clone()
                };
            }
            for h in &mut m.handlers {
                h.summary = self.text(&h.summary);
            }
        }
        for r in &mut p.removals {
            r.key = self.text(&r.key);
            r.summary = self.text(&r.summary);
        }
        for r in &mut p.rehomed {
            r.from = self.text(&r.from);
            r.to = self.text(&r.to);
            r.summary = self.text(&r.summary);
        }
        p.deferred = p.deferred.iter().map(note).collect();
        p.held = p.held.iter().map(note).collect();
        p.indeterminate = p.indeterminate.iter().map(note).collect();
        p.warnings = p.warnings.iter().map(|w| self.text(w)).collect();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiffKind, ModuleView, StepOutcome};
    use rstest::rstest;
    use serde_json::json;

    fn change(task: &str, key: &str, unified: &str) -> StepView {
        StepView {
            kind: "file.template".into(),
            key: key.into(),
            task: task.into(),
            outcome: StepOutcome::Change,
            summary: format!("update {key}"),
            diff: Some(DiffKind::Update),
            unified: Some(unified.into()),
            ..Default::default()
        }
    }

    fn noop(task: &str, key: &str) -> StepView {
        StepView {
            kind: "file.template".into(),
            key: key.into(),
            task: task.into(),
            ..Default::default()
        }
    }

    fn host(name: &str, ip: &str, steps: Vec<StepView>) -> FleetHost {
        FleetHost {
            host: name.into(),
            plan: Some(PlanView {
                host: name.into(),
                modules: vec![ModuleView {
                    module: "nomad".into(),
                    steps,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            context: json!({
                "id": name,
                "labels": {"site": "home"},
                "facts": {"net": {"ipv4": [ip]}, "os": {"codename": "bookworm"}},
            }),
        }
    }

    fn conf(name: &str, ip: &str) -> String {
        format!("-name = \"old\"\n+name = \"{name}\"\n+bind = \"{ip}\"\n")
    }

    #[test]
    fn facts_that_vary_become_tokens_and_identical_changes_merge() {
        let plan = FleetPlan::build(&[
            host(
                "seer",
                "10.0.0.1",
                vec![change(
                    "hcl",
                    "/etc/nomad.d/seer.hcl",
                    &conf("seer", "10.0.0.1"),
                )],
            ),
            host(
                "prophet",
                "10.0.0.2",
                vec![change(
                    "hcl",
                    "/etc/nomad.d/prophet.hcl",
                    &conf("prophet", "10.0.0.2"),
                )],
            ),
        ]);
        let step = &plan.modules[0].steps[0];
        assert_eq!(step.key, "/etc/nomad.d/\u{ab}fact:host.id\u{bb}.hcl");
        assert_eq!(step.variants.len(), 1, "{step:#?}");
        assert!(plan.is_fleet_wide(&step.variants[0].hosts));
        let unified = step.variants[0].value.unified.as_deref().unwrap();
        assert!(
            unified.contains("\u{ab}fact:host.facts.net.ipv4[0]\u{bb}"),
            "{unified}"
        );
        assert!(
            unified.contains("\"old\""),
            "untouched text stays: {unified}"
        );

        // Facts shared by every host are left as text, and get no exemplars.
        let paths: Vec<&str> = plan.facts.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["host.facts.net.ipv4[0]", "host.id"]);
        let id = plan.fact("host.id").unwrap();
        assert_eq!(id.distinct, 2);
        assert_eq!(id.samples.len(), 2);

        // Each host's own render comes back from the merged change.
        let own = plan.render_step("prophet", &step.variants[0].value);
        assert_eq!(own.key, "/etc/nomad.d/prophet.hcl");
        assert_eq!(
            own.unified.as_deref(),
            Some(conf("prophet", "10.0.0.2").as_str())
        );
        assert_eq!(plan.render("nobody", &step.key), step.key);

        // A phase's exemplars come from its own hosts.
        let phase = plan.restrict(&["prophet".to_owned()]);
        let id = phase.fact("host.id").unwrap();
        assert_eq!(id.samples.len(), 1);
        assert_eq!(id.samples[0].host, "prophet");
    }

    #[test]
    fn differing_changes_keep_a_variant_each_and_noops_are_listed_apart() {
        let plan = FleetPlan::build(&[
            host(
                "seer",
                "10.0.0.1",
                vec![change("hcl", "/etc/a", "+x = 1\n")],
            ),
            host(
                "prophet",
                "10.0.0.2",
                vec![change("hcl", "/etc/a", "+x = 2\n")],
            ),
            host("rpi5", "10.0.0.3", vec![noop("hcl", "/etc/a")]),
        ]);
        let step = &plan.modules[0].steps[0];
        assert_eq!(step.variants.len(), 2);
        assert_eq!(step.unchanged, ["rpi5"]);
        assert!(!plan.is_fleet_wide(&step.changed_hosts().cloned().collect::<Vec<_>>()));
        assert_eq!(plan.modules[0].hosts.len(), 3);
    }

    #[test]
    fn restricting_to_some_hosts_drops_what_only_others_had() {
        let plan = FleetPlan::build(&[
            host(
                "seer",
                "10.0.0.1",
                vec![change("hcl", "/etc/a", "+x = 1\n")],
            ),
            host(
                "prophet",
                "10.0.0.2",
                vec![change("hcl", "/etc/a", "+x = 2\n"), noop("b", "/etc/b")],
            ),
        ]);
        let phase = plan.restrict(&["seer".to_owned()]);
        assert_eq!(phase.hosts, ["seer"]);
        let steps = &phase.modules[0].steps;
        assert_eq!(steps.len(), 1, "{steps:#?}");
        assert_eq!(steps[0].variants.len(), 1);
        assert!(phase.is_fleet_wide(&steps[0].variants[0].hosts));
    }

    #[test]
    fn hosts_without_a_plan_are_unplanned_and_contribute_no_facts() {
        let mut absent = host("rpi5", "10.0.0.9", vec![]);
        absent.plan = None;
        let plan = FleetPlan::build(&[host("seer", "10.0.0.1", vec![]), absent]);
        assert_eq!(plan.hosts, ["seer"]);
        assert_eq!(plan.unplanned, ["rpi5"]);
        assert!(plan.facts.is_empty());
    }

    #[rstest]
    #[case::whole_word("host seer up", "host \u{ab}fact:host.id\u{bb} up")]
    #[case::inside_a_word("overseer", "overseer")]
    #[case::prefix_of_a_word("seers", "seers")]
    #[case::punctuated(
        "seer-data/seer.conf",
        "\u{ab}fact:host.id\u{bb}-data/\u{ab}fact:host.id\u{bb}.conf"
    )]
    #[case::inside_a_secret("\u{ab}secret:seer@ab12\u{bb}", "\u{ab}secret:seer@ab12\u{bb}")]
    #[case::longest_wins("seer.lan", "\u{ab}fact:host.facts.fqdn\u{bb}")]
    fn tokenising_matches_whole_values_only(#[case] text: &str, #[case] expected: &str) {
        let leaves = [
            ("host.id".to_owned(), "seer".to_owned()),
            ("host.facts.fqdn".to_owned(), "seer.lan".to_owned()),
        ];
        assert_eq!(Tokeniser::new(leaves.iter()).text(text), expected);
    }

    #[test]
    fn equal_values_take_the_shortest_path() {
        let leaves = [
            ("host.facts.os.hostname".to_owned(), "seer".to_owned()),
            ("host.id".to_owned(), "seer".to_owned()),
        ];
        assert_eq!(
            Tokeniser::new(leaves.iter()).text("seer"),
            "\u{ab}fact:host.id\u{bb}"
        );
    }

    #[test]
    fn flatten_names_paths_as_templates_read_them() {
        let mut out = Vec::new();
        flatten("host", &json!({"a": {"b-c": "x", "d": ["y", 3]}}), &mut out);
        let paths: Vec<&str> = out.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, ["host.a[\"b-c\"]", "host.a.d[0]"]);
    }

    #[test]
    fn a_fleet_plan_round_trips() {
        let plan = FleetPlan::build(&[host("seer", "10.0.0.1", vec![noop("hcl", "/etc/a")])]);
        let text = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<FleetPlan>(&text).unwrap(), plan);
    }
}
