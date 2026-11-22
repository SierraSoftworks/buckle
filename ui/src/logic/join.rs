//! The Add an agent wizard's decisions: which method to offer first, the
//! request the form makes, what the operator must know before pasting, and
//! when the new host has arrived.

use buckle_api::{HostSummary, JoinMethod, JoinOptions, JoinPlatform, JoinRequest, JoinTicket};

use std::collections::BTreeMap;

use super::labels::{Row, collect, is_reserved, reserved_values};

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

/// The form as a request, or the first thing wrong with it. `reserved` are
/// the values given for the labels the fleet reserves: the server sets
/// those on the host when it enrols, and they never appear in the command.
pub fn request(
    platform: JoinPlatform,
    method: JoinMethod,
    host: &str,
    labels: &[Row],
    tags: &[String],
    options: &JoinOptions,
    reserved: &BTreeMap<String, String>,
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
    if let Some(key) = labels
        .keys()
        .find(|k| is_reserved(k, &options.reserved_labels))
    {
        return Err(format!(
            "'{key}' is reserved: give its value under Reserved labels."
        ));
    }
    let server_labels = reserved_values(&options.reserved_labels, reserved)?;
    Ok(JoinRequest {
        platform,
        method,
        host_id: (!host.is_empty()).then(|| host.to_owned()),
        labels,
        server_labels,
        tailscale_tags: tags.to_vec(),
    })
}

/// What [`request`] says while a reserved label still lacks its value: the
/// form is unfinished, not wrong, so the wizard only keeps its button off.
pub fn unfinished(options: &JoinOptions, reserved: &BTreeMap<String, String>) -> Option<String> {
    reserved_values(&options.reserved_labels, reserved).err()
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
            reserved_labels: Vec::new(),
            verified_tags: None,
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
            reserved_labels: Vec::new(),
        }
    }

    /// The form's request on a fleet reserving `reserved`, given `given`.
    fn asked(
        host: &str,
        labels: &[Row],
        reserved: &[&str],
        given: &[(&str, &str)],
    ) -> Result<JoinRequest, String> {
        let mut o = options(false);
        o.reserved_labels = reserved.iter().map(|k| k.to_string()).collect();
        let given = given
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        request(
            JoinPlatform::Linux,
            JoinMethod::Token,
            host,
            labels,
            &[],
            &o,
            &given,
        )
    }

    fn row(key: &str, value: &str) -> Row {
        Row {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn the_form_becomes_a_request_with_blank_fields_left_out() {
        let r = asked("  ", &[row("role", "web"), row("", "")], &[], &[]).unwrap();
        assert_eq!(r.host_id, None);
        assert_eq!(r.labels.get("role").map(String::as_str), Some("web"));
        assert!(r.server_labels.is_empty());
    }

    #[test]
    fn reserved_labels_are_the_servers_to_set_and_each_needs_a_value() {
        let r = asked(
            "web-1",
            &[row("role", "web")],
            &["zone"],
            &[("zone", "public")],
        )
        .unwrap();
        assert_eq!(
            r.server_labels.get("zone").map(String::as_str),
            Some("public")
        );
        assert!(!r.labels.contains_key("zone"), "never in the command");
        for (labels, given, says) in [
            (vec![], vec![], "Give 'zone' a value"),
            (
                vec![row("Zone", "x")],
                vec![("zone", "public")],
                "is reserved",
            ),
        ] {
            let e = asked("web-1", &labels, &["zone"], &given).unwrap_err();
            assert!(e.contains(says), "{e}");
        }
        let mut o = options(false);
        assert_eq!(unfinished(&o, &BTreeMap::new()), None);
        o.reserved_labels = vec!["zone".into()];
        assert_eq!(
            unfinished(&o, &BTreeMap::new()),
            asked("web-1", &[], &["zone"], &[]).err(),
            "a value still to give is the form unfinished"
        );
    }

    #[test]
    fn bad_names_and_labels_are_reported() {
        assert!(asked("web 1", &[], &[], &[]).is_err());
        let e = asked("web-1", &[row("bad key", "x")], &[], &[]).unwrap_err();
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
            server_labels: Default::default(),
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
