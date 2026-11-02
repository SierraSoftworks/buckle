//! `docker.image`: pull (or remove) a container image.

use super::hostkit::{self, Ensure};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::Command;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::instrument;

/// Pulls a container image with Docker, or removes it.
///
/// ```yaml
/// - docker.image:
///     name: ghcr.io/example/profiler:1.4.0   # include the tag (or an @sha256 digest)
///     pull: missing                           # missing (default) | always
///     state: present                          # or absent
/// ```
///
/// With `pull: missing` the image is pulled only when `docker image inspect`
/// cannot find it. With `pull: always` the image is pulled on every apply so a
/// moving tag such as `latest` is refreshed; the task is then reported as
/// changed only if the image's `RepoDigests` differ before and after the pull,
/// but because the registry is not consulted in plan mode it always appears as
/// a pending change in plans. Pin tags or digests where that is not wanted.
///
/// Requires `docker` on the PATH (reported as blocked when missing). Removal
/// runs `docker image rm` for an image Buckle pulled; an image still used by a
/// container is left in place. It holds the `docker` lock.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
    #[serde(default)]
    pub pull: Pull,
    #[serde(default)]
    pub state: Ensure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Pull {
    Always,
    #[default]
    Missing,
}

pub struct DockerImage;
buckle_core::register_task!(DockerImage);

const ARTEFACT_PREFIX: &str = "docker.image: ";

/// `Some(sorted repo digests)` when the image exists locally.
async fn inspect(cx: &Cx, name: &str) -> Result<Option<Vec<String>>> {
    let out = cx
        .system
        .run(
            Command::new("docker")
                .args(["image", "inspect", "--format", "{{json .RepoDigests}}"])
                .arg(name),
        )
        .await?;
    if !out.success() {
        return Ok(None);
    }
    let mut digests: Vec<String> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    digests.sort();
    Ok(Some(digests))
}

fn validate(spec: &Spec) -> Result<()> {
    if spec.name.trim().is_empty()
        || spec.name.contains(char::is_whitespace)
        || spec.name.starts_with('-')
    {
        return Err(error::user(
            format!("'{}' is not a valid image name.", spec.name),
            &["Use a reference such as `ghcr.io/org/image:tag`."],
        ));
    }
    Ok(())
}

#[async_trait]
impl Task for DockerImage {
    type Spec = Spec;
    const KIND: &'static str = "docker.image";
    const LOCKS: &'static [&'static str] = &["docker"];
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Pull a container image (missing or always, detecting digest changes), or remove it.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        format!("image {}", spec.name)
    }

    #[instrument(name = "task.docker.image.plan", skip_all, fields(image = %spec.name, state = ?spec.state))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        validate(spec)?;
        if let Some(blocked) = hostkit::missing_tool(cx, "docker", "install Docker").await? {
            return Ok(blocked);
        }
        let present = inspect(cx, &spec.name).await?;
        Ok(match (spec.state, present, spec.pull) {
            (Ensure::Absent, Some(_), _) => Planned::change(Diff::Delete {
                summary: format!("remove image {}", spec.name),
            }),
            (Ensure::Absent, None, _) => Planned::NoOp,
            (Ensure::Present, None, _) => Planned::change(Diff::Create {
                summary: format!("pull image {}", spec.name),
                preview: None,
            }),
            (Ensure::Present, Some(_), Pull::Missing) => Planned::NoOp,
            (Ensure::Present, Some(_), Pull::Always) => Planned::change(Diff::Opaque {
                summary: format!(
                    "pull {} (always; changed only if its digest differs)",
                    spec.name
                ),
            }),
        })
    }

    #[instrument(name = "task.docker.image.apply", skip_all, fields(image = %spec.name, state = ?spec.state))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        validate(spec)?;
        hostkit::require_tool(cx, "docker", "install Docker").await?;
        let before = inspect(cx, &spec.name).await?;
        match spec.state {
            Ensure::Absent => {
                if before.is_none() {
                    return Ok(Applied::unchanged(format!(
                        "image {} was already absent",
                        spec.name
                    )));
                }
                hostkit::run_checked(cx, rm(&spec.name), "Removing the image").await?;
                Ok(Applied::changed(format!("removed image {}", spec.name)))
            }
            Ensure::Present => {
                if before.is_some() && spec.pull == Pull::Missing {
                    return Ok(hostkit::ownership::carry_forward(
                        cx,
                        Applied::unchanged(format!("image {} is already present", spec.name)),
                    ));
                }
                hostkit::run_checked(
                    cx,
                    Command::new("docker").arg("pull").arg(&spec.name),
                    "Pulling the image",
                )
                .await?;
                let after = inspect(cx, &spec.name).await?;
                let changed = before != after;
                let mut applied = if changed {
                    Applied::changed(format!("pulled {}", spec.name))
                } else {
                    Applied::unchanged(format!("{} is up to date", spec.name))
                };
                if before.is_none() {
                    applied = applied.with_artefact(Artefact::Other {
                        description: format!("{ARTEFACT_PREFIX}{}", spec.name),
                    });
                }
                // An image Buckle pulled first stays Buckle's when a later pull updates it.
                Ok(hostkit::ownership::carry_forward(cx, applied))
            }
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "docker image rm {} (only if Buckle pulled it)",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(name) = record.artefacts.iter().find_map(|a| match a {
            Artefact::Other { description } => {
                description.strip_prefix(ARTEFACT_PREFIX).map(str::to_owned)
            }
            _ => None,
        }) else {
            return Ok(Applied::skipped(
                "the image was present before Buckle managed it, left in place",
            ));
        };
        hostkit::require_tool(cx, "docker", "install Docker").await?;
        let out = cx.system.run(rm(&name)).await?;
        if out.success() {
            return Ok(Applied::changed(format!("removed image {name}")));
        }
        let stderr = out.stderr_str();
        if stderr.contains("No such image") {
            Ok(Applied::unchanged(format!(
                "image {name} was already absent"
            )))
        } else if stderr.contains("conflict")
            || stderr.contains("in use")
            || stderr.contains("must be forced")
        {
            Ok(Applied::skipped(format!(
                "image {name} is in use by a container, left in place"
            )))
        } else {
            Err(hostkit::command_failed(
                cx,
                "Removing the image",
                &format!("docker image rm {name}"),
                &out,
                false,
            ))
        }
    }
}

fn rm(name: &str) -> Command {
    Command::new("docker").arg("image").arg("rm").arg(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::{FilePerms, Output};
    use buckle_core::testing::{FakeSystem, ok_output};
    use rstest::rstest;
    use serde_json::json;

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"name": "ghcr.io/x/app:1"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    fn marker(name: &str) -> String {
        format!("/fake-docker/{}", name.replace('/', "_"))
    }

    /// A docker whose local image store is files under /fake-docker, and whose
    /// registry serves the digest in `registry` (a mutable file, so tests can move a tag).
    fn docker(initial: Option<&str>) -> FakeSystem {
        let sys = FakeSystem::new().with_file(
            "/fake-registry/digest",
            "sha256:aaa",
            FilePerms::default(),
        );
        let sys = match initial {
            Some(d) => sys.with_file(marker("ghcr.io/x/app:1"), d, FilePerms::default()),
            None => sys,
        };
        let h = sys.clone();
        let inspect = move |cmd: &buckle_core::system::Command| {
            let name = cmd.args.last().unwrap();
            match h.file_string(marker(name)) {
                Some(d) => ok_output(&format!("[\"{name}@{d}\"]")),
                None => Output {
                    code: Some(1),
                    stdout: vec![],
                    stderr: b"Error: No such image".to_vec(),
                    timed_out: false,
                },
            }
        };
        let h = sys.clone();
        let pull = move |cmd: &buckle_core::system::Command| {
            let name = cmd.args.last().unwrap();
            let digest = h.file_string("/fake-registry/digest").unwrap();
            let _ = h
                .clone()
                .with_file(marker(name), digest, FilePerms::default());
            ok_output("pulled")
        };
        sys.on_command("docker", &["image", "inspect"], inspect)
            .on_command("docker", &["pull"], pull)
    }

    #[rstest]
    #[case::missing(docker(None), json!({}), Class::Change)]
    #[case::present(docker(Some("sha256:aaa")), json!({}), Class::NoOp)]
    #[case::always_is_pending(docker(Some("sha256:aaa")), json!({"pull": "always"}), Class::Change)]
    #[case::absent_present(docker(Some("sha256:aaa")), json!({"state": "absent"}), Class::Change)]
    #[case::absent_missing(docker(None), json!({"state": "absent"}), Class::NoOp)]
    #[case::no_docker(FakeSystem::new(), json!({}), Class::Blocked)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        assert_eq!(
            plan_class(&DockerImage, &cx(&sys), &s(extra)).await,
            expected
        );
    }

    #[tokio::test]
    async fn pulling_a_missing_image_changes_then_settles() {
        let sys = docker(None);
        let c = cx(&sys);
        let spec = s(json!({}));
        let applied = apply(&DockerImage, &c, &spec).await;
        assert!(applied.changed && applied.artefacts.len() == 1);
        assert!(
            sys.command_lines()
                .iter()
                .any(|l| l == "docker pull ghcr.io/x/app:1")
        );
        assert_eq!(plan_class(&DockerImage, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn always_pull_reports_change_only_when_the_digest_moves() {
        let sys = docker(Some("sha256:aaa"));
        let c = cx(&sys);
        let spec = s(json!({"pull": "always"}));
        let same = apply(&DockerImage, &c, &spec).await;
        assert!(!same.changed, "{}", same.summary);

        let _ = sys
            .clone()
            .with_file("/fake-registry/digest", "sha256:bbb", FilePerms::default());
        let planned = DockerImage.plan(&c, &spec).await.unwrap();
        let moved = DockerImage.apply(&c, &spec, &planned).await.unwrap();
        assert!(moved.changed);
        assert!(
            moved.artefacts.is_empty(),
            "an image that existed before is not Buckle's to remove"
        );
    }

    #[tokio::test]
    async fn absent_removes_the_image() {
        let sys = docker(Some("sha256:aaa")).on_command_ok("docker", &["image", "rm"], "Untagged");
        apply(&DockerImage, &cx(&sys), &s(json!({"state": "absent"}))).await;
        assert!(
            sys.command_lines()
                .iter()
                .any(|l| l == "docker image rm ghcr.io/x/app:1")
        );
    }

    #[tokio::test]
    async fn removal_handles_in_use_missing_and_adopted_images() {
        let sys = docker(None).on_command_ok("docker", &["image", "rm"], "Untagged");
        let c = cx(&sys);
        let rec = record(
            "docker.image",
            "k",
            &apply(&DockerImage, &c, &s(json!({}))).await,
        );
        assert!(DockerImage.remove(&c, &rec).await.unwrap().changed);

        let in_use = docker(None).on_command_fail(
            "docker",
            &["image", "rm"],
            1,
            "Error response from daemon: conflict: unable to remove repository reference",
        );
        let c = cx(&in_use);
        let rec = record(
            "docker.image",
            "k",
            &apply(&DockerImage, &c, &s(json!({}))).await,
        );
        let out = DockerImage.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("in use"));

        let gone =
            docker(None).on_command_fail("docker", &["image", "rm"], 1, "Error: No such image: x");
        let c = cx(&gone);
        let rec = record(
            "docker.image",
            "k",
            &apply(&DockerImage, &c, &s(json!({}))).await,
        );
        assert!(!DockerImage.remove(&c, &rec).await.unwrap().changed);

        let sys = docker(Some("sha256:aaa"));
        let rec = record("docker.image", "k", &Applied::unchanged("adopted"));
        let out = DockerImage.remove(&cx(&sys), &rec).await.unwrap();
        assert!(out.summary.contains("before Buckle"));
        assert!(!sys.command_lines().iter().any(|l| l.contains(" rm ")));
    }

    #[tokio::test]
    async fn pull_failures_carry_docker_output() {
        let sys = FakeSystem::new()
            .on_command_fail("docker", &["image", "inspect"], 1, "")
            .on_command_fail("docker", &["pull"], 1, "manifest unknown");
        let err = DockerImage
            .apply(&cx(&sys), &s(json!({})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("manifest unknown"));
    }

    #[tokio::test]
    async fn bad_names_are_rejected() {
        let sys = docker(None);
        assert!(
            DockerImage
                .plan(&cx(&sys), &s(json!({"name": "a b"})))
                .await
                .is_err()
        );
        assert!(
            DockerImage
                .plan(&cx(&sys), &s(json!({"name": "--rm"})))
                .await
                .is_err()
        );
    }
}
