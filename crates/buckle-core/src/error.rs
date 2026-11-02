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
