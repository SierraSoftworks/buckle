//! Ownership and digest stamps on the objects Buckle applies.
//!
//! Executors keep no state for a cluster, so every object carries who
//! applied it and from what: `buckle.target`, `buckle.module`,
//! `buckle.revision` and `buckle.digest`, in the object's `Meta` where it has
//! one (namespaces, node pools) and as a suffix of its description where it
//! does not (ACL policies, quotas). Variables and host volumes have neither
//! and are listed in the target's index variable ([`crate::index`]).
//!
//! Jobs carry only `buckle.target` and `buckle.module` in their meta, which
//! never change after the first registration: Nomad passes job meta to every
//! task, so a meta change is a destructive update. Their revision and digest
//! are in the index.
//!
//! The revision is the one that last *changed* the object: when the intent's
//! digest matches the recorded one, the recorded revision is kept.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const TARGET: &str = "buckle.target";
pub const MODULE: &str = "buckle.module";
pub const REVISION: &str = "buckle.revision";
pub const DIGEST: &str = "buckle.digest";

/// One object's stamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamps {
    pub target: String,
    pub module: String,
    pub revision: String,
    pub digest: String,
}

/// Who owns a live object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    /// Absent, nothing to own.
    Absent,
    /// Present without stamps: Buckle adopts it on apply.
    Unmanaged,
    /// Stamped by this target.
    Us(Stamps),
    /// Stamped by another target: a plan error, so two targets never fight.
    Other(String),
}

impl Owner {
    pub fn of(live: Option<Stamps>, exists: bool, target: &str) -> Owner {
        match live {
            None if exists => Owner::Unmanaged,
            None => Owner::Absent,
            Some(s) if s.target == target => Owner::Us(s),
            Some(s) => Owner::Other(s.target),
        }
    }

    /// The stamps to write for an intent with `digest` at `revision`: the
    /// live revision is kept when the digest has not changed.
    pub fn next(&self, target: &str, module: &str, revision: &str, digest: &str) -> Stamps {
        let revision = match self {
            Owner::Us(s) if s.digest == digest && s.module == module => s.revision.clone(),
            _ => revision.to_owned(),
        };
        Stamps {
            target: target.to_owned(),
            module: module.to_owned(),
            revision,
            digest: digest.to_owned(),
        }
    }

    /// A plan error for an object another target owns.
    pub fn conflict(&self, what: &str) -> Option<String> {
        match self {
            Owner::Other(t) => Some(format!(
                "{what} is managed by the cluster target '{t}'; a workload of one target may not take over another's objects."
            )),
            _ => None,
        }
    }
}

/// The longest description Nomad accepts on an ACL policy (and the
/// other objects stamped in their description).
pub const MAX_DESCRIPTION: usize = 256;
const ELLIPSIS: &str = "...";

impl Stamps {
    /// Reads stamps from an object's `Meta` map.
    pub fn from_meta(meta: Option<&Value>) -> Option<Stamps> {
        let meta = meta?.as_object()?;
        let get = |k: &str| meta.get(k).and_then(Value::as_str).map(str::to_owned);
        Some(Stamps {
            target: get(TARGET)?,
            module: get(MODULE).unwrap_or_default(),
            revision: get(REVISION).unwrap_or_default(),
            digest: get(DIGEST).unwrap_or_default(),
        })
    }

    /// Writes the stamps into `meta`, keeping its other keys.
    pub fn into_meta(&self, meta: &mut Map<String, Value>) {
        for (k, v) in [
            (TARGET, &self.target),
            (MODULE, &self.module),
            (REVISION, &self.revision),
            (DIGEST, &self.digest),
        ] {
            meta.insert(k.to_owned(), Value::String(v.clone()));
        }
    }

    /// Sets only the owner stamps (`buckle.target`, `buckle.module`) on
    /// `object`'s `Meta`: what a job carries.
    pub fn stamp_owner(target: &str, module: &str, object: &mut Value) {
        if let Some(meta) = meta_of(object) {
            meta.insert(TARGET.to_owned(), Value::String(target.to_owned()));
            meta.insert(MODULE.to_owned(), Value::String(module.to_owned()));
        }
    }

    /// Sets the stamps on `object`'s `Meta` field, creating it if needed.
    pub fn stamp_meta(&self, object: &mut Value) {
        if let Some(meta) = meta_of(object) {
            self.into_meta(meta);
        }
    }

    /// `description` with the stamps appended, for objects without `Meta`.
    /// The revision is shortened to 12 characters (Nomad bounds descriptions).
    pub fn describe(&self, description: &str) -> String {
        let revision = &self.revision[..self.revision.len().min(12)];
        let suffix = format!(
            "[buckle target={} module={} revision={revision} digest={}]",
            self.target, self.module, self.digest
        );
        // Nomad rejects descriptions over 256 bytes, so the author's text
        // gives way to the stamps, which ownership depends on.
        let room = MAX_DESCRIPTION.saturating_sub(suffix.len() + 1);
        if description.is_empty() || room == 0 {
            suffix
        } else if description.len() <= room {
            format!("{description} {suffix}")
        } else {
            let mut end = room.saturating_sub(ELLIPSIS.len());
            while !description.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}{ELLIPSIS} {suffix}", description[..end].trim_end())
        }
    }

    /// Splits a stamped description into the author's text and the stamps.
    pub fn from_description(description: &str) -> (String, Option<Stamps>) {
        let Some(start) = description.rfind("[buckle ") else {
            return (description.to_owned(), None);
        };
        let Some(body) = description[start..]
            .strip_prefix("[buckle ")
            .and_then(|b| b.strip_suffix(']'))
        else {
            return (description.to_owned(), None);
        };
        let field = |k: &str| {
            body.split(' ')
                .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                .map(str::to_owned)
        };
        let Some(target) = field("target") else {
            return (description.to_owned(), None);
        };
        let stamps = Stamps {
            target,
            module: field("module").unwrap_or_default(),
            revision: field("revision").unwrap_or_default(),
            digest: field("digest").unwrap_or_default(),
        };
        (description[..start].trim_end().to_owned(), Some(stamps))
    }
}

fn meta_of(object: &mut Value) -> Option<&mut Map<String, Value>> {
    let map = object.as_object_mut()?;
    let meta = map
        .entry("Meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    meta.as_object_mut()
}

/// A short digest for stamps: the first 16 hex characters of the intent's SHA-256.
pub fn short_digest<T: Serialize>(intent: &T) -> String {
    let d = buckle_core::Digest::of_json(intent);
    d.as_str()[..16].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stamps(revision: &str, digest: &str) -> Stamps {
        Stamps {
            target: "home-nomad".into(),
            module: "paste".into(),
            revision: revision.into(),
            digest: digest.into(),
        }
    }

    #[test]
    fn meta_round_trips_and_keeps_other_keys() {
        let mut job = json!({"ID": "paste", "Meta": {"autoupdate.enabled": "true"}});
        stamps("abc", "d1").stamp_meta(&mut job);
        assert_eq!(job["Meta"]["autoupdate.enabled"], "true");
        assert_eq!(
            Stamps::from_meta(job.get("Meta")),
            Some(stamps("abc", "d1"))
        );
        let mut bare = json!({"ID": "x", "Meta": null});
        stamps("abc", "d1").stamp_meta(&mut bare);
        assert_eq!(bare["Meta"][TARGET], "home-nomad");
    }

    #[rstest::rstest]
    #[case("Atuin's policy")]
    #[case("")]
    #[case("text with [brackets]")]
    fn descriptions_round_trip(#[case] text: &str) {
        let stamped = stamps("abc", "d1").describe(text);
        assert_eq!(
            Stamps::from_description(&stamped),
            (text.to_owned(), Some(stamps("abc", "d1")))
        );
        assert_eq!(Stamps::from_description(text), (text.to_owned(), None));
    }

    #[rstest::rstest]
    #[case::ascii("a".repeat(400))]
    #[case::multibyte("é".repeat(200))]
    fn long_descriptions_are_cut_to_fit_and_keep_their_stamps(#[case] text: String) {
        let stamped = stamps("abc", "d1").describe(&text);
        assert!(stamped.len() <= MAX_DESCRIPTION, "{} bytes", stamped.len());
        let (kept, s) = Stamps::from_description(&stamped);
        assert_eq!(s, Some(stamps("abc", "d1")));
        assert!(kept.ends_with("..."));
    }

    #[rstest::rstest]
    #[case::unchanged_keeps_revision(Owner::Us(stamps("old", "d1")), "d1", "old")]
    #[case::changed_takes_revision(Owner::Us(stamps("old", "d0")), "d1", "new")]
    #[case::adopted_takes_revision(Owner::Unmanaged, "d1", "new")]
    #[case::created_takes_revision(Owner::Absent, "d1", "new")]
    fn the_revision_is_the_one_that_last_changed_the_object(
        #[case] owner: Owner,
        #[case] digest: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            owner.next("home-nomad", "paste", "new", digest).revision,
            expected
        );
    }

    #[test]
    fn ownership_is_decided_by_the_target_stamp() {
        let mut other = stamps("r", "d");
        other.target = "lab-nomad".into();
        assert_eq!(Owner::of(None, false, "home-nomad"), Owner::Absent);
        assert_eq!(Owner::of(None, true, "home-nomad"), Owner::Unmanaged);
        let theirs = Owner::of(Some(other), true, "home-nomad");
        assert!(theirs.conflict("job paste").unwrap().contains("lab-nomad"));
        assert!(Owner::Unmanaged.conflict("job paste").is_none());
    }
}
