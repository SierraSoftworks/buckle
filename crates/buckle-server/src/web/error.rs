//! API errors as `ErrorBody` JSON.

use crate::rollout::{CommandError, CommandErrorKind};
use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};
use buckle_api::ErrorBody;

/// An error response.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: ErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, error: impl Into<String>, advice: &[&str]) -> Self {
        Self {
            status,
            body: ErrorBody {
                error: error.into(),
                advice: advice.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    /// 401: no valid session.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "You need to sign in.",
            &["Sign in through /api/v1/auth/login."],
        )
    }

    /// 403.
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            msg,
            &["Ask an administrator to grant you access."],
        )
    }

    /// 403 for a missing or wrong CSRF token.
    pub fn csrf() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "The request is missing its CSRF token.",
            &["Fetch /api/v1/csrf and send the token in the X-CSRF-Token header."],
        )
    }

    /// 404.
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg, &[])
    }

    /// 400.
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg, &[])
    }

    /// 409.
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, msg, &[])
    }

    /// 503.
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            msg,
            &["Try again shortly."],
        )
    }

    /// 429.
    pub fn too_many() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many requests.",
            &["Wait a minute and try again."],
        )
    }

    /// The JSON response.
    pub fn response(&self) -> HttpResponse {
        HttpResponse::build(self.status).json(&self.body)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.body.error)
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        self.status
    }

    fn error_response(&self) -> HttpResponse {
        self.response()
    }
}

fn advice_of(e: &buckle_core::Error) -> Vec<String> {
    e.advice().iter().map(|s| s.to_string()).collect()
}

impl From<buckle_core::Error> for ApiError {
    fn from(e: buckle_core::Error) -> Self {
        if e.is(buckle_core::error::Kind::User) {
            Self {
                status: StatusCode::BAD_REQUEST,
                body: ErrorBody {
                    error: e.description().to_string(),
                    advice: advice_of(&e),
                },
            }
        } else {
            tracing::error!(error = %e, "an API request failed");
            Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.description().to_string(),
                &["This is a bug or an infrastructure problem; check the server logs."],
            )
        }
    }
}

impl From<CommandError> for ApiError {
    fn from(e: CommandError) -> Self {
        let status = match e.kind {
            CommandErrorKind::NotFound => StatusCode::NOT_FOUND,
            CommandErrorKind::Conflict => StatusCode::CONFLICT,
            CommandErrorKind::Invalid => StatusCode::BAD_REQUEST,
            CommandErrorKind::Internal => return Self::from(e.error),
        };
        Self {
            status,
            body: ErrorBody {
                error: e.error.description().to_string(),
                advice: advice_of(&e.error),
            },
        }
    }
}
