//! Buckle uses [`human_errors`] throughout. User errors are misconfiguration and
//! expected environmental problems the operator can fix; system errors are bugs
//! and corruption, and are the ones reported to error tracking.

pub use human_errors::{Error, Kind, ResultExt, pretty, system, user, wrap_system, wrap_user};

/// The crate-wide result type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Conversions from foreign error types into [`Error`] with sensible kinds.
pub trait HumanizableError {
    fn to_human_error(self) -> Error;
}

impl HumanizableError for std::io::Error {
    fn to_human_error(self) -> Error {
        match self.kind() {
            std::io::ErrorKind::NotFound => wrap_user(
                self,
                "A file or directory that Buckle needed could not be found.",
                &["Check that the path exists and that Buckle has permission to read it."],
            ),
            std::io::ErrorKind::PermissionDenied => wrap_user(
                self,
                "Buckle was denied permission to access a file or directory.",
                &["Run Buckle with sufficient privileges, or adjust the permissions on the path."],
            ),
            _ => wrap_system(
                self,
                "An I/O operation failed unexpectedly.",
                &[
                    "Read the internal error message for details and report the issue if it persists.",
                ],
            ),
        }
    }
}

impl HumanizableError for serde_yaml::Error {
    fn to_human_error(self) -> Error {
        wrap_user(
            self,
            "A YAML document could not be parsed.",
            &["Check the file for syntax errors; the internal error names the line and column."],
        )
    }
}

impl HumanizableError for serde_json::Error {
    fn to_human_error(self) -> Error {
        wrap_user(
            self,
            "A JSON document could not be parsed.",
            &["Check the document for syntax errors; the internal error names the position."],
        )
    }
}

impl HumanizableError for minijinja::Error {
    fn to_human_error(self) -> Error {
        wrap_user(
            self,
            "A template could not be rendered.",
            &[
                "Check the template for syntax errors and that every variable it references is defined.",
                "Secret values may only be used in file templates and in fields declared as secret-capable.",
            ],
        )
    }
}

/// Result extension applying [`HumanizableError`].
pub trait HumanizableResult<T> {
    fn to_human_error(self) -> Result<T>;
}

impl<T, E: HumanizableError> HumanizableResult<T> for std::result::Result<T, E> {
    fn to_human_error(self) -> Result<T> {
        self.map_err(HumanizableError::to_human_error)
    }
}

/// Who should act on an [`ErrorReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Something the configuration author or operator can fix.
    User,
    /// Something in Buckle or the host that needs investigation.
    System,
}

/// A structured, serialisable rendering of an [`Error`]: its one-sentence
/// description, the chain of causes beneath it (outermost first) and the
/// advice attached anywhere in that chain (most specific first), so readers
/// never have to parse [`Error::message`]'s dump.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ErrorReport {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

impl ErrorReport {
    /// The report of `err`: [`Error::description`], every source beneath it
    /// (a wrapped [`Error`] contributes its description, never its dump) and
    /// [`Error::advice`].
    pub fn of(err: &Error) -> Self {
        let mut causes = Vec::new();
        let mut cause: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
        while let Some(e) = cause {
            let text = match e.downcast_ref::<Error>() {
                Some(inner) => inner.description(),
                None => e.to_string(),
            };
            if !text.is_empty() && causes.last() != Some(&text) {
                causes.push(text);
            }
            cause = e.source();
        }
        Self {
            kind: if err.is(Kind::User) {
                ErrorKind::User
            } else {
                ErrorKind::System
            },
            message: err.description(),
            causes,
            advice: err.advice().into_iter().map(str::to_owned).collect(),
        }
    }

    /// A user error with no causes or advice.
    pub fn user(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::User,
            message: message.into(),
            causes: Vec::new(),
            advice: Vec::new(),
        }
    }

    /// A system error with no causes or advice.
    pub fn system(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::System,
            message: message.into(),
            causes: Vec::new(),
            advice: Vec::new(),
        }
    }

    /// Adds advice lines.
    pub fn with_advice<S: Into<String>>(mut self, advice: impl IntoIterator<Item = S>) -> Self {
        self.advice.extend(advice.into_iter().map(Into::into));
        self
    }

    /// Adds cause lines.
    pub fn with_causes<S: Into<String>>(mut self, causes: impl IntoIterator<Item = S>) -> Self {
        self.causes.extend(causes.into_iter().map(Into::into));
        self
    }

    /// Wraps the report under a new top-level message: the old message
    /// becomes the outermost cause, the advice is kept.
    pub fn context(mut self, message: impl Into<String>) -> Self {
        let inner = std::mem::replace(&mut self.message, message.into());
        self.causes.insert(0, inner);
        self
    }

    /// The report with its message prefixed (`prefix: message`), for a
    /// module-level error that names the step it came from.
    pub fn prefixed(mut self, prefix: &str) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }

    /// The report with `f` applied to every text field (redaction).
    pub fn map_text(self, f: impl Fn(&str) -> String) -> Self {
        Self {
            kind: self.kind,
            message: f(&self.message),
            causes: self.causes.iter().map(|c| f(c)).collect(),
            advice: self.advice.iter().map(|a| f(a)).collect(),
        }
    }

    /// One line for logs and legacy string fields: the message, without the
    /// causes or advice.
    pub fn one_line(&self) -> String {
        self.message.clone()
    }
}

impl From<&Error> for ErrorReport {
    fn from(err: &Error) -> Self {
        Self::of(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn nested() -> Error {
        wrap_user(
            wrap_system(
                std::io::Error::other("disk full"),
                "The file could not be written.",
                &["Free some space on the disk."],
            ),
            "Applying file.copy[/etc/motd] failed.",
            &["Check the task's destination."],
        )
    }

    #[test]
    fn a_report_splits_description_causes_and_advice() {
        let report = ErrorReport::of(&nested());
        assert_eq!(report.kind, ErrorKind::User);
        assert_eq!(report.message, "Applying file.copy[/etc/motd] failed.");
        assert_eq!(
            report.causes,
            vec!["The file could not be written.", "disk full"]
        );
        assert_eq!(
            report.advice,
            vec![
                "Free some space on the disk.",
                "Check the task's destination."
            ]
        );
        assert!(
            !report.message.contains("To try and fix this"),
            "the message never carries the advice dump"
        );
    }

    #[rstest]
    #[case::user(user("bad config", &["fix it"]), ErrorKind::User, 0, 1)]
    #[case::system(system("bug", &[]), ErrorKind::System, 0, 0)]
    #[case::wrapped_io(
        std::io::Error::new(std::io::ErrorKind::NotFound, "gone").to_human_error(),
        ErrorKind::User,
        1,
        1
    )]
    fn kinds_and_counts_follow_the_error(
        #[case] err: Error,
        #[case] kind: ErrorKind,
        #[case] causes: usize,
        #[case] advice: usize,
    ) {
        let report = ErrorReport::of(&err);
        assert_eq!(report.kind, kind);
        assert_eq!(report.causes.len(), causes, "{report:?}");
        assert_eq!(report.advice.len(), advice, "{report:?}");
    }

    #[test]
    fn map_text_reaches_every_field_and_context_keeps_the_chain() {
        let report = ErrorReport::user("token s3cret rejected")
            .with_causes(["s3cret expired"])
            .with_advice(["rotate s3cret"])
            .map_text(|t| t.replace("s3cret", "[redacted]"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("s3cret"), "{json}");
        let wrapped = report.context("Step x failed.");
        assert_eq!(wrapped.message, "Step x failed.");
        assert_eq!(wrapped.causes[0], "token [redacted] rejected");
        assert_eq!(wrapped.one_line(), "Step x failed.");
    }

    #[test]
    fn empty_causes_and_advice_are_omitted_and_tolerated() {
        let json = serde_json::to_value(ErrorReport::system("boom")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "system", "message": "boom"})
        );
        let back: ErrorReport = serde_json::from_value(json).unwrap();
        assert!(back.causes.is_empty() && back.advice.is_empty());
    }
}
