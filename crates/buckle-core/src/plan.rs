//! The plan: a redacted, serialisable description of what an apply will do.
//!
//! Plans are produced by the pure half of the engine, shown to operators,
//! digested for deduplication across hosts, and consumed verbatim by the
//! mutating half. Everything in a plan is safe to display by construction:
//! previews and diffs are [`Redacted`].

use crate::Digest;
use crate::model::RemovalPolicy;
use crate::secrets::Redacted;
use serde::{Deserialize, Serialize};

/// Identifies a revision of the configuration (a Git commit, or a synthetic id
/// for a local directory).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RevisionId(pub String);

impl RevisionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn short(&self) -> &str {
        &self.0[..self.0.len().min(8)]
    }
}

impl std::fmt::Display for RevisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stable identity of a managed resource: `module/kind[key]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceKey {
    pub module: String,
    pub kind: String,
    pub key: String,
}

impl ResourceKey {
    pub fn new(module: impl Into<String>, kind: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            module: module.into(),
            kind: kind.into(),
            key: key.into(),
        }
    }
}

impl std::fmt::Display for ResourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}[{}]", self.module, self.kind, self.key)
    }
}

/// A reference to a task within a module plan.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskRef {
    pub module: String,
    /// The task's id (explicit `id:` or derived from the kind's key).
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// What a task's `plan` decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Planned {
    /// The resource already matches the spec.
    NoOp,
    /// A change is needed.
    Change { diff: Diff },
    /// The task cannot run here; the reason is shown to the operator.
    Blocked { reason: String },
}

impl Planned {
    pub fn change(diff: Diff) -> Self {
        Planned::Change { diff }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        Planned::Blocked {
            reason: reason.into(),
        }
    }

    pub fn is_change(&self) -> bool {
        matches!(self, Planned::Change { .. })
    }
}

/// A displayable description of a change. Text previews are redacted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Diff {
    Create {
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<Redacted<String>>,
    },
    Update {
        summary: String,
        /// A unified diff of redacted shadows (old vs new); absent when content is binary or unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unified: Option<Redacted<String>>,
    },
    Delete {
        summary: String,
    },
    /// Nothing to diff, e.g. "run `nomad node eligibility -enable`".
    Opaque {
        summary: String,
    },
}

impl Diff {
    pub fn summary(&self) -> &str {
        match self {
            Diff::Create { summary, .. }
            | Diff::Update { summary, .. }
            | Diff::Delete { summary }
            | Diff::Opaque { summary } => summary,
        }
    }

    pub fn is_destructive(&self) -> bool {
        matches!(self, Diff::Delete { .. })
    }

    /// Builds a unified diff of two redacted texts.
    pub fn unified(
        summary: impl Into<String>,
        old: &Redacted<String>,
        new: &Redacted<String>,
    ) -> Diff {
        Self::unified_with_context(summary, old, new, 3)
    }

    /// [`Diff::unified`] with `context` lines around each change; zero shows
    /// only the changed lines (for files whose other lines are sensitive).
    pub fn unified_with_context(
        summary: impl Into<String>,
        old: &Redacted<String>,
        new: &Redacted<String>,
        context: usize,
    ) -> Diff {
        let text = similar::TextDiff::from_lines(old.get().as_str(), new.get().as_str())
            .unified_diff()
            .context_radius(context)
            .header("current", "planned")
            .to_string();
        Diff::Update {
            summary: summary.into(),
            unified: Some(Redacted::new(text)),
        }
    }
}

/// One task in a module plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub task: TaskRef,
    pub resource: ResourceKey,
    pub planned: Planned,
    /// Resource locks the executor serialises on (e.g. `apt`).
    #[serde(default)]
    pub locks: Vec<String>,
    /// Handlers notified when this step changes something.
    #[serde(default)]
    pub notifies: Vec<String>,
    /// Whether this step runs always (a `command.run` with no idempotency guard).
    #[serde(default)]
    pub always_runs: bool,
    #[serde(default)]
    pub removal: RemovalPolicy,
    /// The host values (`host.facts.net.fqdn`) the task's templates emitted
    /// through expressions, so the fleet view marks only those as fact
    /// placeholders; `None` when the task's renders were not traced. Not
    /// part of the plan's digests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<Vec<String>>,
    /// The step's diff text (the preview of a creation, the unified diff of
    /// an update) with `«fact:PATH»` placeholders exactly where the task's
    /// templates emitted host values, for the fleet view; `None` when no
    /// line of it came from such an expression. Not part of the plan's
    /// digests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModulePlan {
    pub module: String,
    #[serde(default)]
    pub needs: Vec<String>,
    pub steps: Vec<Step>,
    /// Handlers that will run at the end of the module, after dedupe and ordering.
    #[serde(default)]
    pub handlers: Vec<HandlerRun>,
    /// Facts to refresh after this module applies.
    #[serde(default)]
    pub provides_facts: Vec<String>,
}

impl ModulePlan {
    pub fn changes(&self) -> usize {
        self.steps.iter().filter(|s| s.planned.is_change()).count()
    }

    /// The digest of what this module intends ([`Plan::intent_digest`]).
    pub fn intent_digest(&self) -> Digest {
        Digest::of_json(&ModuleIntent::of(self))
    }
}

/// A module plan without its rendered text (see [`Plan::intent_digest`]).
#[derive(Serialize)]
struct ModuleIntent<'a> {
    module: &'a str,
    needs: &'a [String],
    steps: Vec<StepIntent<'a>>,
    handlers: Vec<(&'a str, &'a str, &'a str)>,
}

#[derive(Serialize)]
struct StepIntent<'a> {
    resource: &'a ResourceKey,
    task: &'a str,
    outcome: &'static str,
    always_runs: bool,
    removal: RemovalPolicy,
    notifies: &'a [String],
}

impl<'a> ModuleIntent<'a> {
    fn of(m: &'a ModulePlan) -> Self {
        fn outcome(planned: &Planned) -> &'static str {
            match planned {
                Planned::NoOp => "noop",
                Planned::Blocked { .. } => "blocked",
                Planned::Change { diff } => match diff {
                    Diff::Create { .. } => "create",
                    Diff::Update { .. } => "update",
                    Diff::Delete { .. } => "delete",
                    Diff::Opaque { .. } => "run",
                },
            }
        }
        ModuleIntent {
            module: &m.module,
            needs: &m.needs,
            steps: m
                .steps
                .iter()
                .map(|s| StepIntent {
                    resource: &s.resource,
                    task: &s.task.id,
                    outcome: outcome(&s.planned),
                    always_runs: s.always_runs,
                    removal: s.removal,
                    notifies: &s.notifies,
                })
                .collect(),
            handlers: m
                .handlers
                .iter()
                .map(|h| (h.module.as_str(), h.handler.as_str(), h.kind.as_str()))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerRun {
    pub module: String,
    pub handler: String,
    pub kind: String,
    pub summary: String,
}

/// A resource in state that the desired set no longer contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    pub resource: ResourceKey,
    pub policy: RemovalPolicy,
    /// What `remove()` will do, or why nothing will happen.
    pub summary: String,
    /// Kinds whose removal can take unrelated things down (packages, users, groups).
    #[serde(default)]
    pub caution: bool,
    /// The revision that introduced the resource, when known.
    #[serde(default)]
    pub introduced_in: Option<RevisionId>,
}

/// A module a drift plan leaves alone because the host does not have it
/// applied at the plan's revision: it moved on to a newer revision, its last
/// run failed, or it never ran. Its resources are never orphans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Held {
    pub module: String,
    pub reason: String,
}

/// How an orphaned record relates to a desired resource that took it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RehomeKind {
    /// The same kind and key now lives in another module: the record is
    /// re-keyed to the new module and kept (with its origin and shadow).
    Moved,
    /// A desired resource of another kind (or key) manages the same artefact
    /// (file path, package, user, unit…): the old record is forgotten without
    /// running its cleanup, which would delete what the new resource manages.
    Superseded,
}

/// A resource in state that left the desired set but is not removed, because
/// a desired resource took it over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rehome {
    pub from: ResourceKey,
    pub to: ResourceKey,
    pub kind: RehomeKind,
    pub summary: String,
}

/// A module that could not be planned yet because it depends on facts a
/// sibling module provides after applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deferred {
    pub module: String,
    pub waiting_on: Vec<String>,
    pub reason: String,
    /// The module planned ahead of its providers: against the facts known
    /// now, with each fact a provider has yet to produce rendered as a
    /// `«fact:PATH»` placeholder. Shown for review and bound by the intent
    /// digest; at apply time the module's real plan must keep its intent.
    /// Absent when it could not be planned ahead (e.g. its `when` waits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<ModulePlan>,
}

/// A module whose selection or plan could not be determined on this host: its
/// `when` could not be evaluated or reads facts a provider failed to collect,
/// its vars or fields could not be rendered, its directory has no
/// `module.yaml`, or it needs such a module. An indeterminate module is never
/// applied and its recorded resources are never treated as orphans, so an
/// uncertain selection cannot remove anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Indeterminate {
    pub module: String,
    pub reason: String,
    /// The module is selected but could not be planned: a plan error for that
    /// module (design §03). Applying the plan reports the module as failed;
    /// other modules still run.
    #[serde(default)]
    pub plan_error: bool,
    /// The plan error, structured (message, causes, advice), when the module
    /// could not be planned. Not part of the plan digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<crate::error::ErrorReport>,
}

impl Indeterminate {
    /// An indeterminate module (not a plan error).
    pub fn new(module: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            module: module.into(),
            reason: reason.into(),
            plan_error: false,
            error_detail: None,
        }
    }

    /// A module that is selected but could not be planned.
    pub fn plan_error(
        module: impl Into<String>,
        reason: impl Into<String>,
        detail: Option<crate::error::ErrorReport>,
    ) -> Self {
        Self {
            module: module.into(),
            reason: reason.into(),
            plan_error: true,
            error_detail: detail,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub revision: RevisionId,
    pub host: String,
    pub digest: Digest,
    /// Modules selected for this host, in dependency order.
    pub modules: Vec<ModulePlan>,
    #[serde(default)]
    pub removals: Vec<Removal>,
    #[serde(default)]
    pub deferred: Vec<Deferred>,
    /// Modules evaluated but not selected, with the selector that excluded them (for the UI).
    #[serde(default)]
    pub skipped: Vec<String>,
    /// Warnings worth showing the operator (always-run commands, cautionary removals, …).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Modules whose selection or plan is uncertain; see [`Indeterminate`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indeterminate: Vec<Indeterminate>,
    /// Records taken over by a desired resource instead of being removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rehomed: Vec<Rehome>,
    /// Drift plans only: modules not applied at this revision, left alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<Held>,
}

impl Plan {
    /// Builds the plan and computes its digest over the content (not the host or revision),
    /// so hosts with identical plans share a digest.
    pub fn new(
        revision: RevisionId,
        host: impl Into<String>,
        modules: Vec<ModulePlan>,
        removals: Vec<Removal>,
        deferred: Vec<Deferred>,
    ) -> Self {
        let mut plan = Plan {
            revision,
            host: host.into(),
            digest: Digest::of(b""),
            modules,
            removals,
            deferred,
            skipped: Vec::new(),
            warnings: Vec::new(),
            indeterminate: Vec::new(),
            rehomed: Vec::new(),
            held: Vec::new(),
        };
        plan.digest = plan.compute_digest();
        plan
    }

    /// Sets the indeterminate modules and recomputes the digest (they change
    /// what an apply does).
    pub fn with_indeterminate(mut self, indeterminate: Vec<Indeterminate>) -> Self {
        self.indeterminate = indeterminate;
        self.digest = self.compute_digest();
        self
    }

    /// Sets the held modules and recomputes the digest.
    pub fn with_held(mut self, held: Vec<Held>) -> Self {
        self.held = held;
        self.digest = self.compute_digest();
        self
    }

    /// Sets the re-homed records and recomputes the digest.
    pub fn with_rehomed(mut self, rehomed: Vec<Rehome>) -> Self {
        self.rehomed = rehomed;
        self.digest = self.compute_digest();
        self
    }

    pub fn compute_digest(&self) -> Digest {
        /// A removal without its origin: hosts that applied the same resource
        /// in different revisions still share the plan.
        #[derive(Serialize)]
        struct RemovalContent<'a> {
            resource: &'a ResourceKey,
            policy: RemovalPolicy,
            summary: &'a str,
            caution: bool,
        }
        let removals: Vec<RemovalContent<'_>> = self
            .removals
            .iter()
            .map(|r| RemovalContent {
                resource: &r.resource,
                policy: r.policy,
                summary: &r.summary,
                caution: r.caution,
            })
            .collect();
        /// An indeterminate module without its structured error, so the
        /// digest is what it was before that field existed.
        #[derive(Serialize)]
        struct IndeterminateContent<'a> {
            module: &'a str,
            reason: &'a str,
            plan_error: bool,
        }
        let indeterminate: Vec<IndeterminateContent<'_>> = self
            .indeterminate
            .iter()
            .map(|i| IndeterminateContent {
                module: &i.module,
                reason: &i.reason,
                plan_error: i.plan_error,
            })
            .collect();
        #[derive(Serialize)]
        struct Content<'a> {
            modules: &'a [ModulePlan],
            removals: &'a [RemovalContent<'a>],
            deferred: &'a [Deferred],
            #[serde(skip_serializing_if = "<[IndeterminateContent]>::is_empty")]
            indeterminate: &'a [IndeterminateContent<'a>],
            #[serde(skip_serializing_if = "<[Rehome]>::is_empty")]
            rehomed: &'a [Rehome],
            #[serde(skip_serializing_if = "<[Held]>::is_empty")]
            held: &'a [Held],
        }
        // Which host values a step's text came from is display metadata:
        // hosts with the same plan share a digest whatever their facts.
        let traced = |m: &ModulePlan| {
            m.steps
                .iter()
                .any(|s| s.facts.is_some() || s.display.is_some())
        };
        let untraced = |m: &ModulePlan| ModulePlan {
            steps: m
                .steps
                .iter()
                .map(|s| Step {
                    facts: None,
                    display: None,
                    ..s.clone()
                })
                .collect(),
            ..m.clone()
        };
        let modules: std::borrow::Cow<'_, [ModulePlan]> = if self.modules.iter().any(traced) {
            self.modules.iter().map(untraced).collect()
        } else {
            (&self.modules[..]).into()
        };
        let deferred: std::borrow::Cow<'_, [Deferred]> = if self
            .deferred
            .iter()
            .any(|d| d.preview.as_ref().is_some_and(traced))
        {
            self.deferred
                .iter()
                .map(|d| Deferred {
                    preview: d.preview.as_ref().map(untraced),
                    ..d.clone()
                })
                .collect()
        } else {
            (&self.deferred[..]).into()
        };
        Digest::of_json(&Content {
            modules: &modules,
            removals: &removals,
            deferred: &deferred,
            indeterminate: &indeterminate,
            rehomed: &self.rehomed,
            held: &self.held,
        })
    }

    /// The digest of what the plan intends, without any rendered text: per
    /// module its steps (resource, task, outcome and diff kind, whether it
    /// always runs, its removal policy and notifies) and handlers, plus the
    /// removals, re-homes, deferred, indeterminate and held modules.
    /// Summaries, diffs, previews and reasons are left out, so a re-plan
    /// whose only differences are content (a fact value, a secret
    /// fingerprint, a clock) keeps it, while a new resource, a new delete or
    /// a new removal changes it. Approvals bind this digest.
    pub fn intent_digest(&self) -> Digest {
        /// A deferred module previewed ahead of its providers is bound by
        /// its preview's intent; without one, by its name alone.
        type DeferredIntent<'a> = (&'a str, &'a [String], Option<ModuleIntent<'a>>);
        #[derive(Serialize)]
        struct Intent<'a> {
            modules: Vec<ModuleIntent<'a>>,
            removals: Vec<(&'a ResourceKey, RemovalPolicy, bool)>,
            rehomed: Vec<(&'a ResourceKey, &'a ResourceKey, RehomeKind)>,
            deferred: Vec<DeferredIntent<'a>>,
            indeterminate: Vec<(&'a str, bool)>,
            held: Vec<&'a str>,
        }
        Digest::of_json(&Intent {
            modules: self.modules.iter().map(ModuleIntent::of).collect(),
            removals: self
                .removals
                .iter()
                .map(|r| (&r.resource, r.policy, r.caution))
                .collect(),
            rehomed: self
                .rehomed
                .iter()
                .map(|r| (&r.from, &r.to, r.kind))
                .collect(),
            deferred: self
                .deferred
                .iter()
                .map(|d| {
                    (
                        d.module.as_str(),
                        d.waiting_on.as_slice(),
                        d.preview.as_ref().map(ModuleIntent::of),
                    )
                })
                .collect(),
            indeterminate: self
                .indeterminate
                .iter()
                .map(|i| (i.module.as_str(), i.plan_error))
                .collect(),
            held: self.held.iter().map(|h| h.module.as_str()).collect(),
        })
    }

    pub fn changes(&self) -> usize {
        self.modules.iter().map(ModulePlan::changes).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.changes() == 0 && self.removals.is_empty()
    }

    /// Whether any step (a deferred module's preview included) deletes
    /// something or any removal will run.
    pub fn is_destructive(&self) -> bool {
        let previews = self.deferred.iter().filter_map(|d| d.preview.as_ref());
        self.removals
            .iter()
            .any(|r| r.policy == RemovalPolicy::Cleanup)
            || self
                .modules
                .iter()
                .chain(previews)
                .flat_map(|m| &m.steps)
                .any(|s| matches!(&s.planned, Planned::Change { diff } if diff.is_destructive()))
    }

    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.modules.iter().flat_map(|m| m.steps.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(module: &str, key: &str, planned: Planned) -> Step {
        Step {
            task: TaskRef {
                module: module.into(),
                id: key.into(),
                kind: "file.copy".into(),
                name: None,
            },
            resource: ResourceKey::new(module, "file.copy", key),
            planned,
            locks: vec![],
            notifies: vec![],
            always_runs: false,
            removal: RemovalPolicy::Cleanup,
            facts: None,
            display: None,
        }
    }

    fn module(name: &str, steps: Vec<Step>) -> ModulePlan {
        ModulePlan {
            module: name.into(),
            needs: vec![],
            steps,
            handlers: vec![],
            provides_facts: vec![],
        }
    }

    #[test]
    fn digest_depends_on_content_not_host_or_revision() {
        let a = Plan::new(
            RevisionId::new("aaa"),
            "pi-1",
            vec![module("m", vec![step("m", "/x", Planned::NoOp)])],
            vec![],
            vec![],
        );
        let b = Plan::new(
            RevisionId::new("bbb"),
            "pi-2",
            vec![module("m", vec![step("m", "/x", Planned::NoOp)])],
            vec![],
            vec![],
        );
        let c = Plan::new(
            RevisionId::new("aaa"),
            "pi-1",
            vec![module("m", vec![step("m", "/y", Planned::NoOp)])],
            vec![],
            vec![],
        );
        assert_eq!(a.digest, b.digest);
        assert_ne!(a.digest, c.digest);
    }

    fn update(summary: &str, unified: &str) -> Planned {
        Planned::change(Diff::Update {
            summary: summary.into(),
            unified: Some(Redacted::new(unified.into())),
        })
    }

    fn one_step(planned: Planned) -> Plan {
        Plan::new(
            RevisionId::new("r"),
            "h",
            vec![module("m", vec![step("m", "/x", planned)])],
            vec![],
            vec![],
        )
    }

    #[rstest::rstest]
    #[case::summary_and_diff_text(
        update("index is 6h 1m old", "-a\n+b"),
        update("index is 6h 9m old", "-a\n+c"),
        true
    )]
    #[case::blocked_reason(
        Planned::blocked("apt is locked"),
        Planned::blocked("dpkg is locked"),
        true
    )]
    #[case::update_becomes_noop(update("u", "-a\n+b"), Planned::NoOp, false)]
    #[case::update_becomes_delete(update("u", "-a\n+b"), Planned::change(Diff::Delete { summary: "u".into() }), false)]
    #[case::create_becomes_run(Planned::change(Diff::Create { summary: "c".into(), preview: None }), Planned::change(Diff::Opaque { summary: "c".into() }), false)]
    fn intent_digest_ignores_text_but_not_actions(
        #[case] a: Planned,
        #[case] b: Planned,
        #[case] same: bool,
    ) {
        let (a, b) = (one_step(a), one_step(b));
        assert_eq!(a.intent_digest() == b.intent_digest(), same);
    }

    #[test]
    fn intent_digest_changes_with_new_resources_and_removals() {
        let base = one_step(Planned::NoOp);
        let mut added = base.clone();
        added.modules[0].steps.push(step("m", "/y", Planned::NoOp));
        let mut removing = base.clone();
        removing.removals.push(Removal {
            resource: ResourceKey::new("m", "file.copy", "/gone"),
            policy: RemovalPolicy::Cleanup,
            summary: "delete /gone".into(),
            caution: false,
            introduced_in: None,
        });
        assert_ne!(base.intent_digest(), added.intent_digest());
        assert_ne!(base.intent_digest(), removing.intent_digest());
    }

    #[test]
    fn traced_facts_do_not_split_the_digests() {
        let untraced = one_step(update("u", "-a\n+seer"));
        let mut traced = untraced.clone();
        traced.modules[0].steps[0].facts = Some(vec!["host.id".into()]);
        traced.deferred.push(Deferred {
            module: "d".into(),
            waiting_on: vec![],
            reason: "r".into(),
            preview: Some(traced.modules[0].clone()),
        });
        let mut deferred = untraced.clone();
        deferred.deferred.push(Deferred {
            preview: Some(untraced.modules[0].clone()),
            ..traced.deferred[0].clone()
        });
        traced.modules[0].steps[0].facts = Some(vec!["host.facts.fqdn".into()]);
        traced.modules[0].steps[0].display = Some("-a\n+\u{ab}fact:host.id\u{bb}".into());
        assert_eq!(traced.compute_digest(), deferred.compute_digest());
        assert_eq!(traced.intent_digest(), deferred.intent_digest());
    }

    #[test]
    fn removal_origins_do_not_split_the_digest() {
        let removal = |origin: &str| Removal {
            resource: ResourceKey::new("m", "file.copy", "/gone"),
            policy: RemovalPolicy::Cleanup,
            summary: "delete /gone".into(),
            caution: false,
            introduced_in: Some(RevisionId::new(origin)),
        };
        let a = Plan::new(
            RevisionId::new("r"),
            "a",
            vec![],
            vec![removal("r1")],
            vec![],
        );
        let b = Plan::new(
            RevisionId::new("r"),
            "b",
            vec![],
            vec![removal("r7")],
            vec![],
        );
        assert_eq!(a.digest, b.digest);
    }

    #[test]
    fn counts_changes_and_detects_destructive_plans() {
        let plan = Plan::new(
            RevisionId::new("r"),
            "h",
            vec![module(
                "m",
                vec![
                    step("m", "/a", Planned::NoOp),
                    step(
                        "m",
                        "/b",
                        Planned::change(Diff::Create {
                            summary: "create".into(),
                            preview: None,
                        }),
                    ),
                ],
            )],
            vec![],
            vec![],
        );
        assert_eq!(plan.changes(), 1);
        assert!(!plan.is_destructive());
        assert!(!plan.is_empty());

        let destructive = Plan::new(
            RevisionId::new("r"),
            "h",
            vec![],
            vec![Removal {
                resource: ResourceKey::new("m", "file.copy", "/gone"),
                policy: RemovalPolicy::Cleanup,
                summary: "delete /gone".into(),
                caution: false,
                introduced_in: None,
            }],
            vec![],
        );
        assert!(destructive.is_destructive());
        let left = Plan::new(
            RevisionId::new("r"),
            "h",
            vec![],
            vec![Removal {
                resource: ResourceKey::new("m", "file.copy", "/gone"),
                policy: RemovalPolicy::Leave,
                summary: "released".into(),
                caution: false,
                introduced_in: None,
            }],
            vec![],
        );
        assert!(!left.is_destructive());
    }

    #[test]
    fn unified_diffs_are_built_from_redacted_text() {
        let old = Redacted::plain("a\nb\n".to_string());
        let new = Redacted::plain("a\nc\n".to_string());
        let Diff::Update {
            unified: Some(u), ..
        } = Diff::unified("2 lines", &old, &new)
        else {
            panic!()
        };
        assert!(u.get().contains("-b"));
        assert!(u.get().contains("+c"));
    }

    #[test]
    fn unified_diffs_can_omit_context() {
        let old = Redacted::plain("a\nb\nc\nd\n".to_string());
        let new = Redacted::plain("a\nb\nX\nd\n".to_string());
        let Diff::Update {
            unified: Some(u), ..
        } = Diff::unified_with_context("1 line", &old, &new, 0)
        else {
            panic!()
        };
        assert!(
            u.get().contains("-c") && u.get().contains("+X"),
            "{}",
            u.get()
        );
        assert!(!u.get().contains(" b"), "no context lines: {}", u.get());
        let Diff::Update {
            unified: Some(u), ..
        } = Diff::unified("1 line", &old, &new)
        else {
            panic!()
        };
        assert!(u.get().contains(" b"));
    }

    #[test]
    fn plans_round_trip_through_json() {
        let plan = Plan::new(
            RevisionId::new("r"),
            "h",
            vec![module(
                "m",
                vec![step("m", "/a", Planned::blocked("unsupported platform"))],
            )],
            vec![],
            vec![],
        );
        let json = serde_json::to_string(&plan).unwrap();
        let back: Plan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan, back);
    }
}
