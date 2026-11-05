//! The in-cluster executor for a cluster Buckle has no host near: a Nomad
//! job, or a Kubernetes Deployment, running the agent with
//! `identity: workload`, enrolled by its workload identity
//! (`agents.auth: workload_identity`). Rendered for the operator by
//! `GET /api/v1/targets/{id}/executor`; upgraded by re-running it with a new
//! image tag.

use crate::join::JoinSettings;
use buckle_api::{ClusterKind, ClusterSpec, ExecutorSpec};

/// The image the job runs: the server's release, else its major version.
pub fn image(settings: &JoinSettings, server_version: &str) -> String {
    let tag = settings.version.clone().unwrap_or_else(|| {
        server_version
            .trim_start_matches('v')
            .split(['.', '-'])
            .next()
            .unwrap_or("2")
            .to_owned()
    });
    format!("ghcr.io/sierrasoftworks/buckle:{tag}")
}

/// The executor job for `target`.
pub fn render(
    target: &str,
    spec: &ClusterSpec,
    settings: &JoinSettings,
    server_version: &str,
) -> Result<ExecutorSpec, String> {
    if spec.cluster.is_some() {
        return Err(format!(
            "{target} is a scoped target; its cluster's executor serves it."
        ));
    }
    if spec.kind == ClusterKind::Kubernetes {
        return Ok(kubernetes(target, spec, settings, server_version));
    }
    let mut trust = spec.trust.as_deref().cloned().unwrap_or_default();
    let subject = trust
        .subjects(spec.kind)
        .into_iter()
        .next()
        .unwrap_or_default();
    let (namespace, job_id) = subject
        .split_once('/')
        .map(|(n, j)| (n.to_owned(), j.to_owned()))
        .unwrap_or_else(|| ("default".to_owned(), subject.clone()));
    if trust.jwks_url.is_none() && trust.keys.is_none() {
        trust.jwks_url = spec
            .api
            .as_deref()
            .map(|api| format!("{}/.well-known/jwks.json", api.trim_end_matches('/')));
    }
    let host_id = format!("{target}-executor");
    let agent = agent_yaml(target, &host_id, settings);
    let job = nomad_job(
        &namespace,
        &job_id,
        &image(settings, server_version),
        &agent,
    );
    let policy_file = format!("{job_id}.policy.hcl");
    let job_file = format!("{job_id}.nomad.hcl");
    let commands = vec![
        format!(
            "nomad acl policy apply -namespace {namespace} -job {job_id} -description 'Buckle executor for {target}' {job_id} {policy_file}"
        ),
        format!("nomad job run {job_file}"),
    ];
    let mut notes = Vec::new();
    if !settings.workload_identity {
        notes.push("This server does not accept workload identities yet: add `workload_identity` to agents.auth in server.yaml and restart it.".to_owned());
    }
    if trust.jwks_url.is_none() && trust.keys.is_none() {
        notes.push("Give the cluster's key set as trust.keys (`curl $NOMAD_ADDR/.well-known/jwks.json`), or its URL as trust.jwks_url, so the server can check the executor's identity.".to_owned());
    }
    if spec.identity != "workload" {
        notes.push(format!(
            "The cluster's identity is `{}`; set it to `workload` so the executor uses the token Nomad gives the job.",
            spec.identity
        ));
    }
    notes.push("The policy can create jobs, volumes and variables in every namespace. Workloads declaring `namespaces` or `policies` need a management token, which the executor does not hold.".to_owned());
    Ok(ExecutorSpec {
        target: target.to_owned(),
        host_id,
        job_file,
        job,
        policy_file,
        policy: POLICY.to_owned(),
        commands,
        trust,
        notes,
    })
}

/// The Kubernetes executor: a Deployment in the trusted subject's namespace
/// running as its ServiceAccount, with a projected token for enrolment.
fn kubernetes(
    target: &str,
    spec: &ClusterSpec,
    settings: &JoinSettings,
    server_version: &str,
) -> ExecutorSpec {
    let mut trust = spec.trust.as_deref().cloned().unwrap_or_default();
    let subject = trust
        .subjects(spec.kind)
        .into_iter()
        .next()
        .unwrap_or_default();
    let (namespace, account) = subject
        .strip_prefix("system:serviceaccount:")
        .and_then(|s| s.split_once(':'))
        .map(|(n, a)| (n.to_owned(), a.to_owned()))
        .unwrap_or_else(|| ("buckle-system".to_owned(), "buckle-executor".to_owned()));
    if trust.jwks_url.is_none() && trust.keys.is_none() {
        trust.jwks_url = spec
            .api
            .as_deref()
            .map(|api| format!("{}/openid/v1/jwks", api.trim_end_matches('/')));
    }
    let host_id = format!("{target}-executor");
    let agent = agent_config(target, &host_id, settings, "/var/lib/buckle", K8S_TOKEN);
    let job = kubernetes_manifest(
        &namespace,
        &account,
        &image(settings, server_version),
        &agent,
    );
    let policy = format!(
        r#"# Lets the executor apply every workload. Narrow it (a Role per namespace)
# once the workloads settle.
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {account}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: cluster-admin
subjects:
  - kind: ServiceAccount
    name: {account}
    namespace: {namespace}
"#
    );
    let policy_file = format!("{account}-rbac.yaml");
    let job_file = format!("{account}.yaml");
    let commands = vec![
        format!("kubectl apply -f {job_file}"),
        format!("kubectl apply -f {policy_file}"),
    ];
    let mut notes = Vec::new();
    if !settings.workload_identity {
        notes.push("This server does not accept workload identities yet: add `workload_identity` to agents.auth in server.yaml and restart it.".to_owned());
    }
    if trust.keys.is_none() {
        notes.push("The server reads the cluster's ServiceAccount keys from /openid/v1/jwks, which needs `kubectl create clusterrolebinding service-account-issuer-discovery --clusterrole=system:service-account-issuer-discovery --group=system:unauthenticated`; or pin them as trust.keys (`kubectl get --raw /openid/v1/jwks`).".to_owned());
    }
    if spec.identity != "workload" {
        notes.push(format!(
            "The cluster's identity is `{}`; set it to `workload` so the executor uses its ServiceAccount.",
            spec.identity
        ));
    }
    notes.push("The executor must reach the Buckle server: over the tailnet from a node with hostNetwork, through a Tailscale sidecar, or an egress proxy.".to_owned());
    ExecutorSpec {
        target: target.to_owned(),
        host_id,
        job_file,
        job,
        policy_file,
        policy,
        commands,
        trust,
        notes,
    }
}

/// Where the Deployment mounts the projected token the agent enrols with.
const K8S_TOKEN: &str = "/var/run/secrets/buckle/token";

fn kubernetes_manifest(namespace: &str, account: &str, image: &str, agent: &str) -> String {
    let agent = agent
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"apiVersion: v1
kind: Namespace
metadata:
  name: {namespace}
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {account}
  namespace: {namespace}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: {account}
  namespace: {namespace}
data:
  agent.yaml: |
{agent}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {account}
  namespace: {namespace}
spec:
  replicas: 1
  strategy: {{ type: Recreate }}
  selector:
    matchLabels: {{ app: {account} }}
  template:
    metadata:
      labels: {{ app: {account} }}
    spec:
      serviceAccountName: {account}
      containers:
        - name: agent
          image: {image}
          args: ["agent", "--config", "/etc/buckle/agent.yaml"]
          resources:
            requests: {{ cpu: 50m, memory: 64Mi }}
            limits: {{ memory: 256Mi }}
          volumeMounts:
            - {{ name: config, mountPath: /etc/buckle }}
            - {{ name: identity, mountPath: /var/run/secrets/buckle }}
            - {{ name: state, mountPath: /var/lib/buckle }}
      volumes:
        - name: config
          configMap: {{ name: {account} }}
        # The token the agent enrols with (agents.auth: workload_identity).
        - name: identity
          projected:
            sources:
              - serviceAccountToken: {{ audience: buckle, expirationSeconds: 3600, path: token }}
        - name: state
          emptyDir: {{}}
"#
    )
}

fn agent_yaml(target: &str, host_id: &str, settings: &JoinSettings) -> String {
    agent_config(
        target,
        host_id,
        settings,
        "/alloc/data/buckle",
        "/secrets/nomad_buckle.jwt",
    )
}

fn agent_config(
    target: &str,
    host_id: &str,
    settings: &JoinSettings,
    state_dir: &str,
    token_file: &str,
) -> String {
    let tls = match (&settings.ca_fingerprint, settings.insecure_plaintext) {
        (Some(fp), _) => format!("tls: {{ ca_fingerprint: \"{fp}\" }}\n"),
        (None, true) => "tls: { insecure_plaintext: true }\n".to_owned(),
        (None, false) => String::new(),
    };
    format!(
        "server: {server}\nhost_id: {host_id}\nstate_dir: {state_dir}\n{tls}labels: {{ executor: \"{target}\" }}\nworkload_identity:\n  token_file: {token_file}\n",
        server = settings.server_url,
    )
}

fn nomad_job(namespace: &str, job_id: &str, image: &str, agent: &str) -> String {
    let agent = agent
        .lines()
        .map(|l| format!("          {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"job "{job_id}" {{
  namespace = "{namespace}"
  type      = "service"

  group "executor" {{
    count = 1

    restart {{
      attempts = 10
      delay    = "15s"
      mode     = "delay"
    }}

    task "agent" {{
      driver = "docker"

      config {{
        image = "{image}"
        args  = ["agent", "--config", "/local/agent.yaml"]
      }}

      # The Task API token (NOMAD_TOKEN), bound to the job's ACL policy.
      identity {{
        env         = true
        change_mode = "restart"
      }}

      # The token the agent enrols with (agents.auth: workload_identity).
      identity {{
        name = "buckle"
        aud  = ["buckle"]
        ttl  = "1h"
        file = true
      }}

      env {{
        NOMAD_ADDR = "unix://${{NOMAD_SECRETS_DIR}}/api.sock"
      }}

      template {{
        destination = "local/agent.yaml"
        data        = <<-EOF
{agent}
        EOF
      }}

      resources {{
        cpu    = 100
        memory = 128
      }}
    }}
  }}
}}
"#
    )
}

/// What the executor may do: run jobs, volumes and variables everywhere.
const POLICY: &str = r#"namespace "*" {
  policy = "write"

  variables {
    path "*" {
      capabilities = ["read", "write", "list", "destroy"]
    }
  }
}

node_pool "*" {
  policy = "write"
}

host_volume "*" {
  policy = "write"
}

node {
  policy = "read"
}

quota {
  policy = "write"
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::ClusterTrust;
    use rstest::rstest;

    fn settings(fingerprint: Option<&str>, workload_identity: bool) -> JoinSettings {
        let mut s = crate::join::Joins::default().settings().clone();
        s.server_url = "https://buckle.example.ts.net:7700".into();
        s.insecure_plaintext = fingerprint.is_none();
        s.ca_fingerprint = fingerprint.map(str::to_owned);
        s.workload_identity = workload_identity;
        s
    }

    fn spec(api: Option<&str>, trust: Option<ClusterTrust>) -> ClusterSpec {
        ClusterSpec {
            kind: ClusterKind::Nomad,
            description: String::new(),
            api: api.map(str::to_owned),
            ui: None,
            nodes: None,
            executors: None,
            identity: "workload".into(),
            cluster: None,
            scope: Default::default(),
            trust: trust.map(Box::new),
        }
    }

    #[test]
    fn a_nomad_executor_enrols_with_its_identity_and_uses_the_task_api() {
        let out = render(
            "home-nomad",
            &spec(Some("http://10.0.0.1:4646/"), None),
            &settings(Some("sha256:abcd"), true),
            "2.4.1",
        )
        .unwrap();
        assert_eq!(out.host_id, "home-nomad-executor");
        for needle in [
            r#"job "buckle-executor""#,
            r#"namespace = "default""#,
            "ghcr.io/sierrasoftworks/buckle:2",
            r#"aud  = ["buckle"]"#,
            "token_file: /secrets/nomad_buckle.jwt",
            "host_id: home-nomad-executor",
            "ca_fingerprint: \"sha256:abcd\"",
            "api.sock",
        ] {
            assert!(out.job.contains(needle), "{needle} in\n{}", out.job);
        }
        assert_eq!(
            out.trust.jwks_url.as_deref(),
            Some("http://10.0.0.1:4646/.well-known/jwks.json")
        );
        assert!(out.commands[0].contains("-namespace default -job buckle-executor"));
        assert!(out.notes.iter().all(|n| !n.contains("agents.auth")));
    }

    #[test]
    fn the_job_follows_the_trusted_subject() {
        let trust = ClusterTrust {
            keys: Some(r#"{"keys":[{"kid":"a"}]}"#.into()),
            subjects: vec!["ops/runner".into()],
            ..Default::default()
        };
        let out = render(
            "lab",
            &spec(None, Some(trust)),
            &settings(None, true),
            "2.4.1",
        )
        .unwrap();
        assert!(out.job.contains(r#"job "runner""#) && out.job.contains(r#"namespace = "ops""#));
        assert!(out.job.contains("insecure_plaintext: true"));
        assert!(out.trust.jwks_url.is_none(), "pinned keys stay as they are");
        assert_eq!(out.job_file, "runner.nomad.hcl");
    }

    #[rstest]
    #[case::server_does_not_accept(false, None, "agents.auth")]
    #[case::no_keys(true, None, "trust.keys")]
    fn what_is_missing_is_noted(
        #[case] accepts: bool,
        #[case] api: Option<&str>,
        #[case] note: &str,
    ) {
        let out = render("c", &spec(api, None), &settings(None, accepts), "2.4.1").unwrap();
        assert!(
            out.notes.iter().any(|n| n.contains(note)),
            "{:?}",
            out.notes
        );
    }

    #[test]
    fn a_kubernetes_executor_runs_as_the_trusted_service_account() {
        let mut s = spec(Some("https://10.0.0.1:6443"), None);
        s.kind = ClusterKind::Kubernetes;
        let out = render("lab-k3s", &s, &settings(Some("sha256:abcd"), true), "2.4.1").unwrap();
        for needle in [
            "namespace: buckle-system",
            "serviceAccountName: buckle-executor",
            "audience: buckle",
            "token_file: /var/run/secrets/buckle/token",
            "host_id: lab-k3s-executor",
            "ghcr.io/sierrasoftworks/buckle:2",
        ] {
            assert!(out.job.contains(needle), "{needle} in\n{}", out.job);
        }
        assert!(out.policy.contains("name: cluster-admin"));
        assert_eq!(
            out.trust.jwks_url.as_deref(),
            Some("https://10.0.0.1:6443/openid/v1/jwks")
        );
        assert!(
            out.commands
                .iter()
                .all(|c| c.starts_with("kubectl apply -f"))
        );
        let manifests: Vec<serde_yaml::Value> = serde_yaml::Deserializer::from_str(&out.job)
            .map(|d| serde::Deserialize::deserialize(d).unwrap())
            .collect();
        assert_eq!(manifests.len(), 4, "the manifest is valid YAML");
    }

    #[rstest]
    #[case::scoped(ClusterKind::Nomad, Some("parent"), "scoped")]
    fn other_targets_have_no_executor_job(
        #[case] kind: ClusterKind,
        #[case] parent: Option<&str>,
        #[case] reason: &str,
    ) {
        let mut s = spec(None, None);
        s.kind = kind;
        s.cluster = parent.map(str::to_owned);
        let err = render("c", &s, &settings(None, true), "2.4.1").unwrap_err();
        assert!(err.contains(reason), "{err}");
    }
}
