//! Formatting shared by every page, so that "never", "3m ago" and a short
//! digest read the same wherever they appear.

use chrono::{DateTime, Utc};

/// A duration in its single largest sensible unit: `45s`, `5m`, `2h`, `3d`.
/// The sign belongs to the phrase around it, so it is ignored.
pub fn short_duration(secs: i64) -> String {
    let abs = secs.unsigned_abs();
    match abs {
        0..=59 => format!("{abs}s"),
        60..=3_599 => format!("{}m", abs / 60),
        3_600..=86_399 => format!("{}h", abs / 3_600),
        _ => format!("{}d", abs / 86_400),
    }
}

/// How long a step took, to the precision that matters at its size:
/// `230ms`, `1.4s`, `45s`, `2m 05s`, `1h 02m`.
pub fn millis(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=9_999 => format!("{}.{}s", ms / 1_000, (ms % 1_000) / 100),
        10_000..=59_999 => format!("{}s", ms / 1_000),
        60_000..=3_599_999 => format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1_000),
        _ => format!("{}h {:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000),
    }
}

/// A countdown: `4:05`, `1:02:30`, never negative.
pub fn countdown(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3_600, (secs % 3_600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A configured length of time, as an operator wrote it: `15m`, `1h30m`.
pub fn config_duration(secs: u64) -> String {
    if secs == 0 {
        return "none".into();
    }
    let (h, m, s) = (secs / 3_600, (secs % 3_600) / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 {
        out.push_str(&format!("{s}s"));
    }
    out
}

/// A timestamp relative to `now`: `15m ago`, `in 5m`, or `now`.
pub fn relative(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = at.signed_duration_since(now).num_seconds();
    if secs.abs() < 1 {
        return "now".into();
    }
    let magnitude = short_duration(secs);
    if secs < 0 {
        format!("{magnitude} ago")
    } else {
        format!("in {magnitude}")
    }
}

/// [`relative`] for a timestamp that may be missing, which reads as an em
/// dash everywhere.
pub fn relative_opt(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    at.map(|at| relative(at, now)).unwrap_or_else(|| "—".into())
}

/// How old a reading is, as a bare magnitude (`3m`), or an em dash.
pub fn age(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    at.map(|at| short_duration(now.signed_duration_since(at).num_seconds().max(0)))
        .unwrap_or_else(|| "—".into())
}

/// An absolute time for a tooltip or an audit row.
pub fn timestamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

/// The first eight characters of a digest or revision id.
pub fn short(id: &str) -> &str {
    match id.char_indices().nth(8) {
        Some((i, _)) => &id[..i],
        None => id,
    }
}

/// How a rollout is named to people: its `word.word.word` name, or `#id`
/// for an id that has none. A name (from a `/rollouts/<name>` URL) is its
/// own label.
pub fn rollout_label(id: &str) -> String {
    if buckle_api::RolloutName::parse(id).is_some() {
        return id.to_string();
    }
    buckle_api::rollout_name(id).unwrap_or_else(|| format!("#{id}"))
}

/// The name of the rollout an audit subject is, when the subject is a
/// rollout id (a UUID): hosts and commits are subjects too, and a commit's
/// hex digits would otherwise name a rollout.
pub fn subject_rollout_name(subject: &str) -> Option<String> {
    let uuid = subject.len() == 36
        && subject.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    uuid.then(|| buckle_api::rollout_name(subject)).flatten()
}

/// The first seven characters of a commit, the way `git log --oneline` shows it.
pub fn commit(id: &str) -> &str {
    match id.char_indices().nth(7) {
        Some((i, _)) => &id[..i],
        None => id,
    }
}

/// A commit message's subject line.
pub fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// `1 host`, `3 hosts`.
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn durations_use_their_largest_unit() {
        assert_eq!(short_duration(0), "0s");
        assert_eq!(short_duration(59), "59s");
        assert_eq!(short_duration(60), "1m");
        assert_eq!(short_duration(3_599), "59m");
        assert_eq!(short_duration(3_600), "1h");
        assert_eq!(short_duration(86_400), "1d");
        assert_eq!(short_duration(-90), "1m");
    }

    #[test]
    fn step_durations_keep_the_precision_that_matters() {
        assert_eq!(millis(230), "230ms");
        assert_eq!(millis(1_480), "1.4s");
        assert_eq!(millis(45_000), "45s");
        assert_eq!(millis(125_000), "2m 05s");
        assert_eq!(millis(3_720_000), "1h 02m");
    }

    #[test]
    fn a_countdown_never_goes_below_zero() {
        assert_eq!(countdown(245), "4:05");
        assert_eq!(countdown(3_750), "1:02:30");
        assert_eq!(countdown(-5), "0:00");
    }

    #[test]
    fn configured_durations_read_like_the_config() {
        assert_eq!(config_duration(900), "15m");
        assert_eq!(config_duration(5_400), "1h30m");
        assert_eq!(config_duration(0), "none");
    }

    #[test]
    fn relative_times_say_which_side_of_now_they_are_on() {
        let now = Utc::now();
        assert_eq!(relative(now, now), "now");
        assert_eq!(relative(now - Duration::minutes(15), now), "15m ago");
        assert_eq!(relative(now + Duration::minutes(5), now), "in 5m");
        assert_eq!(relative_opt(None, now), "—");
        assert_eq!(age(Some(now - Duration::seconds(200)), now), "3m");
        // A reading stamped a moment in the future (clock skew) is not negative.
        assert_eq!(age(Some(now + Duration::seconds(3)), now), "0s");
    }

    #[test]
    fn rollouts_are_labelled_by_name_or_by_id() {
        let id = "3f2a9c1e-8b4d-4f6a-9c2e-1d7b5a3e9f01";
        assert_eq!(rollout_label(id), buckle_api::rollout_name(id).unwrap());
        assert_eq!(rollout_label("42"), "#42");
        let name = buckle_api::rollout_name(id).unwrap();
        assert_eq!(rollout_label(&name), name, "a name labels itself");
    }

    #[test]
    fn only_a_rollout_id_subject_is_named() {
        let id = "3f2a9c1e-8b4d-4f6a-9c2e-1d7b5a3e9f01";
        assert_eq!(subject_rollout_name(id), buckle_api::rollout_name(id));
        for other in [
            "pi-1",
            "3f2a9c1e8b4d4f6a9c2e1d7b5a3e9f0112345678",
            "0aa1b2c",
            "42",
        ] {
            assert_eq!(subject_rollout_name(other), None, "{other}");
        }
    }

    #[test]
    fn ids_are_shortened_without_splitting_characters() {
        assert_eq!(short("5b1c9e02aa77"), "5b1c9e02");
        assert_eq!(short("abc"), "abc");
        assert_eq!(commit("7c2e1f0a9"), "7c2e1f0");
        assert_eq!(first_line("subject\n\nbody"), "subject");
        assert_eq!(count(1, "host", "hosts"), "1 host");
        assert_eq!(count(3, "host", "hosts"), "3 hosts");
    }
}
