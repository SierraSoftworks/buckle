//! The Add an agent wizard's decisions: which method to offer first, the
//! request the form makes, what the operator must know before pasting, and
//! when the new host has arrived.

use buckle_api::{HostSummary, JoinMethod, JoinOptions, JoinPlatform, JoinRequest, JoinTicket};

use super::labels::{Row, collect};

/// The method the wizard starts on: the server's preferred one.
pub fn default_method(options: &JoinOptions) -> JoinMethod {
    options
        .methods
        .first()
        .copied()
        .unwrap_or(JoinMethod::Token)
}

/// Whether the command will put the host on the tailnet itself.
pub fn provisions_tailnet(options: &JoinOptions, method: JoinMethod) -> bool {
    method == JoinMethod::Tailscale && options.tailscale.is_some()
}

/// Whether the admin must consent in Tailscale before the command exists.
pub fn needs_consent(options: &JoinOptions, method: JoinMethod) -> bool {
    method == JoinMethod::Tailscale && options.tailscale.as_ref().is_some_and(|t| t.consent)
}

/// Whether `host` is a name the server accepts: 1–128 of `[A-Za-z0-9._-]`.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 128
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The form as a request, or the first thing wrong with it.
pub fn request(
    platform: JoinPlatform,
    method: JoinMethod,
    host: &str,
    labels: &[Row],
    tags: &[String],
) -> Result<JoinRequest, String> {
    let host = host.trim();
    if !host.is_empty() && !valid_host(host) {
        return Err(format!(
            "'{host}' is not a host name: use letters, digits, '.', '_' and '-'."
        ));
    }
    let labels = collect(labels).map_err(|errors| {
        errors
            .into_iter()
            .next()
            .map(|e| e.message)
            .unwrap_or_default()
    })?;
    Ok(JoinRequest {
        platform,
        method,
        host_id: (!host.is_empty()).then(|| host.to_owned()),
        labels,
        tailscale_tags: tags.to_vec(),
    })
}

/// What the operator should know before pasting the command.
pub fn notes(platform: JoinPlatform, method: JoinMethod, options: &JoinOptions) -> Vec<String> {
    let mut out = vec![
        match platform {
            JoinPlatform::Windows => "Paste it into an elevated PowerShell (Run as administrator).",
            _ => "Paste it into a shell on the new host; it runs as root through sudo.",
        }
        .to_owned(),
    ];
    match (method, provisions_tailnet(options, method)) {
        (JoinMethod::Tailscale, true) if platform == JoinPlatform::Macos => out.push(
            "Install the Tailscale app first: the command joins the tailnet with it but cannot install it on macOS."
                .into(),
        ),
        (JoinMethod::Tailscale, true) => out.push(
            "It installs Tailscale if it is missing and joins the tailnet with a single-use key."
                .into(),
        ),
        (JoinMethod::Tailscale, false) => out.push(
            "The host must already be on the tailnet, with a node name matching the host name."
                .into(),
        ),
        (JoinMethod::Token, _) => {
            out.push("The host must reach the server's agent port directly.".into())
        }
    }
    out.push(
        "The command carries single-use keys: use it on one host, and do not share it.".into(),
    );
    out
}

/// The host the ticket enrolled, once it has: the one it names, or else
/// any host first seen since the ticket was made.
pub fn enrolled<'a>(hosts: &'a [HostSummary], ticket: &JoinTicket) -> Option<&'a HostSummary> {
    hosts.iter().find(|h| match &ticket.host_id {
        Some(id) => &h.id == id && h.first_seen >= ticket.created_at,
        None => h.first_seen >= ticket.created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{Connection, HealthSummary, HostStatus, JoinState, TailscaleJoinOptions};
    use chrono::{DateTime, Utc};

    fn host(id: &str, first_seen: DateTime<Utc>) -> HostSummary {
        HostSummary {
            id: id.into(),
            kind: Default::default(),
            cluster: None,
            status: HostStatus::Active,
            connection: Connection::Online,
            agent_labels: Default::default(),
            labels: Default::default(),
            effective_labels: Default::default(),
            groups: vec![],
            platform: None,
            arch: None,
            agent_version: None,
            applied_revision: None,
            health: HealthSummary::Unknown,
            health_at: None,
            drifted: false,
            drift_at: None,
            first_seen,
            last_seen: None,
            notes: String::new(),
            upgrade: None,
            cluster_status: None,
        }
    }

    fn options(provisioning: bool) -> JoinOptions {
        JoinOptions {
            methods: vec![JoinMethod::Tailscale, JoinMethod::Token],
            server_url: "http://buckle:7700".into(),
            tailscale: provisioning.then(|| TailscaleJoinOptions {
                tags: vec!["tag:buckle".into()],
                consent: true,
            }),
            ttl_seconds: 3600,
            approval_required: false,
        }
    }

    fn row(key: &str, value: &str) -> Row {
        Row {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn the_form_becomes_a_request_with_blank_fields_left_out() {
        let r = request(
            JoinPlatform::Linux,
            JoinMethod::Token,
            "  ",
            &[row("role", "web"), row("", "")],
            &[],
        )
        .unwrap();
        assert_eq!(r.host_id, None);
        assert_eq!(r.labels.get("role").map(String::as_str), Some("web"));
    }

    #[test]
    fn bad_names_and_labels_are_reported() {
        assert!(request(JoinPlatform::Linux, JoinMethod::Token, "web 1", &[], &[]).is_err());
        let e = request(
            JoinPlatform::Linux,
            JoinMethod::Token,
            "web-1",
            &[row("bad key", "x")],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("bad key"), "{e}");
    }

    #[test]
    fn only_an_oauth_app_needs_consent() {
        let mut client = options(true);
        client.tailscale.as_mut().unwrap().consent = false;
        for (o, method, expected) in [
            (options(true), JoinMethod::Tailscale, true),
            (options(true), JoinMethod::Token, false),
            (client, JoinMethod::Tailscale, false),
            (options(false), JoinMethod::Tailscale, false),
        ] {
            assert_eq!(needs_consent(&o, method), expected, "{method:?}");
        }
    }

    #[test]
    fn notes_warn_about_what_the_platform_needs() {
        let mac = notes(JoinPlatform::Macos, JoinMethod::Tailscale, &options(true));
        assert!(mac.iter().any(|n| n.contains("Tailscale app")));
        let windows = notes(JoinPlatform::Windows, JoinMethod::Token, &options(false));
        assert!(windows[0].contains("elevated PowerShell"));
        let joined = notes(JoinPlatform::Linux, JoinMethod::Tailscale, &options(false));
        assert!(
            joined
                .iter()
                .any(|n| n.contains("already be on the tailnet"))
        );
    }

    #[test]
    fn a_new_host_counts_as_enrolled_only_after_the_ticket() {
        let at = Utc::now();
        let fleet = [
            host("old", at - chrono::Duration::days(1)),
            host("web-1", at),
        ];
        let mut ticket = JoinTicket {
            id: "j".into(),
            platform: JoinPlatform::Linux,
            method: JoinMethod::Token,
            host_id: Some("web-1".into()),
            labels: Default::default(),
            created_by: "a".into(),
            created_at: at,
            expires_at: at,
            state: JoinState::Ready {
                command: String::new(),
            },
        };
        assert_eq!(
            enrolled(&fleet, &ticket).map(|h| h.id.as_str()),
            Some("web-1")
        );
        ticket.host_id = None;
        assert_eq!(
            enrolled(&fleet, &ticket).map(|h| h.id.as_str()),
            Some("web-1"),
            "any new host"
        );
        ticket.created_at += chrono::Duration::seconds(1);
        assert!(
            enrolled(&fleet, &ticket).is_none(),
            "seen before the ticket"
        );
    }
}
