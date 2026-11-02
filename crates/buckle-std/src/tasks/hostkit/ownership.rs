//! What Buckle owns, carried across applies.
//!
//! The engine stores the artefacts of a resource's latest apply and replaces
//! the previous ones (design section 04). A task that only adjusts something
//! it created earlier (a group change on a user it created, a mode change on
//! a directory it created) must therefore report the earlier artefacts again,
//! or removal later forgets that Buckle created the thing. These helpers read
//! the previous record from [`Cx::previous`] and merge it forward.
//!
//! They also define the "adopted" marker: a pre-existing path whose
//! permissions Buckle changed but whose content it never wrote. Removal leaves
//! an adopted path alone.

use buckle_core::task::{Applied, Artefact, Cx};
use std::path::Path;

/// Prefix of the [`Artefact::Other`] that marks a pre-existing path Buckle adopted.
pub const ADOPTED_PREFIX: &str = "adopted (existed before Buckle; permissions only): ";

/// The artefacts recorded by this resource's previous apply (empty for a new resource).
pub fn previous_artefacts(cx: &Cx) -> &[Artefact] {
    cx.previous
        .as_deref()
        .map(|r| r.artefacts.as_slice())
        .unwrap_or(&[])
}

/// Appends every artefact of the previous record for which `keep` holds and
/// that `applied` does not already carry. Order: this apply's artefacts first.
pub fn carry_forward_if(
    cx: &Cx,
    mut applied: Applied,
    keep: impl Fn(&Artefact) -> bool,
) -> Applied {
    for artefact in previous_artefacts(cx) {
        if keep(artefact) && !applied.artefacts.contains(artefact) {
            applied.artefacts.push(artefact.clone());
        }
    }
    applied
}

/// [`carry_forward_if`] for every previous artefact.
pub fn carry_forward(cx: &Cx, applied: Applied) -> Applied {
    carry_forward_if(cx, applied, |_| true)
}

/// Whether the previous record says Buckle wrote the file at `path`.
pub fn owns_file(cx: &Cx, path: &Path) -> bool {
    previous_artefacts(cx)
        .iter()
        .any(|a| matches!(a, Artefact::File { path: p } if p == path))
}

/// The marker for a pre-existing `path` that Buckle only changed the permissions of.
pub fn adopted(path: &Path) -> Artefact {
    Artefact::Other {
        description: format!("{ADOPTED_PREFIX}{}", path.display()),
    }
}

/// Whether `artefacts` mark any path as adopted.
pub fn any_adopted(artefacts: &[Artefact]) -> bool {
    artefacts.iter().any(
        |a| matches!(a, Artefact::Other { description } if description.starts_with(ADOPTED_PREFIX)),
    )
}

/// The `Applied` for a single-file writer (`file.copy`, `file.template`,
/// `file.download`, `github.release`).
///
/// * When the content was written now (`wrote_content`), Buckle owns the file:
///   the file artefact, the digest of what it wrote and the shadow.
/// * When only permissions changed (or nothing did), Buckle owns the file
///   only if an earlier apply wrote it; otherwise the file existed before
///   Buckle and is recorded as adopted, which removal leaves alone.
pub fn file_written(
    cx: &Cx,
    summary: impl Into<String>,
    path: &Path,
    wrote_content: bool,
    digest: buckle_core::Digest,
    shadow: Option<buckle_core::secrets::Redacted<String>>,
    changed: bool,
) -> Applied {
    if wrote_content || owns_file(cx, path) {
        return super::written(summary, path, digest, shadow, changed);
    }
    let mut applied = if changed {
        Applied::changed(summary)
    } else {
        Applied::unchanged(summary)
    };
    applied.artefacts.push(adopted(path));
    applied.shadow = shadow;
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::record;
    use buckle_core::testing::{CxBuilder, FakeSystem};
    use std::path::PathBuf;

    #[test]
    fn carry_forward_appends_missing_previous_artefacts_once() {
        let prev = Applied::changed("x")
            .with_artefact(Artefact::User { name: "u".into() })
            .with_artefact(Artefact::Group { name: "g".into() });
        let cx = CxBuilder::new(FakeSystem::new())
            .previous(record("k", "u", &prev))
            .build();
        let now = Applied::changed("y").with_artefact(Artefact::Group { name: "g".into() });
        let merged = carry_forward(&cx, now);
        assert_eq!(
            merged.artefacts,
            vec![
                Artefact::Group { name: "g".into() },
                Artefact::User { name: "u".into() }
            ]
        );
        let filtered = carry_forward_if(&cx, Applied::changed("z"), |a| {
            matches!(a, Artefact::User { .. })
        });
        assert_eq!(
            filtered.artefacts,
            vec![Artefact::User { name: "u".into() }]
        );
    }

    #[test]
    fn a_perms_only_change_adopts_unless_buckle_wrote_the_file_before() {
        let path = PathBuf::from("/etc/x");
        let digest = buckle_core::Digest::of(b"x");
        let fresh = CxBuilder::new(FakeSystem::new()).build();
        let adopted_applied = file_written(&fresh, "s", &path, false, digest.clone(), None, true);
        assert!(
            adopted_applied
                .artefacts
                .iter()
                .all(|a| !matches!(a, Artefact::File { .. }))
        );
        assert_eq!(adopted_applied.artefacts, vec![adopted(&path)]);
        assert!(any_adopted(&adopted_applied.artefacts));
        assert!(adopted_applied.content_digest.is_none());

        let owned = CxBuilder::new(FakeSystem::new())
            .previous(record(
                "file.copy",
                "/etc/x",
                &super::super::written("w", &path, digest.clone(), None, true),
            ))
            .build();
        let kept = file_written(&owned, "s", &path, false, digest.clone(), None, true);
        assert_eq!(kept.artefacts, vec![Artefact::File { path: path.clone() }]);
        assert_eq!(kept.content_digest, Some(digest.clone()));

        let wrote = file_written(&fresh, "s", &path, true, digest, None, true);
        assert_eq!(wrote.artefacts, vec![Artefact::File { path }]);
    }
}
