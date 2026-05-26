#[derive(Debug)]
pub struct Error(human_errors::Error);

impl Error {
    pub fn description(&self) -> String {
        self.0.description()
    }

    #[allow(dead_code)]
    pub fn message(&self) -> String {
        self.0.message()
    }

    pub fn is_system(&self) -> bool {
        self.0.is(human_errors::Kind::System)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl From<human_errors::Error> for Error {
    fn from(error: human_errors::Error) -> Self {
        Self(error)
    }
}

fn leak_str(value: impl Into<String>) -> &'static str {
    Box::leak(value.into().into_boxed_str())
}

fn leak_advice(value: impl Into<String>) -> &'static [&'static str] {
    Box::leak(Box::new([leak_str(value)]))
}

pub fn user(message: impl Into<String>, advice: impl Into<String>) -> Error {
    human_errors::user(std::io::Error::other(message.into()), leak_advice(advice)).into()
}

pub fn user_with_internal(
    message: impl Into<String>,
    advice: impl Into<String>,
    err: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>> + 'static,
) -> Error {
    human_errors::wrap_user(err, message.into(), leak_advice(advice)).into()
}

pub fn system_with_internal(
    message: impl Into<String>,
    advice: impl Into<String>,
    err: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>> + 'static,
) -> Error {
    human_errors::wrap_system(err, message.into(), leak_advice(advice)).into()
}

pub fn detailed_message(message: impl Into<String>) -> std::io::Error {
    std::io::Error::other(message.into())
}

#[cfg(unix)]
mod nix;
mod serde;
mod std_io;
mod utf8;
