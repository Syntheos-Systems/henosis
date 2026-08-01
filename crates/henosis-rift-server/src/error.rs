use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Stable JSON error envelope returned by every Rift API failure.
#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    /// Safe human-readable failure detail retained for compatibility.
    pub error: String,
    /// Stable machine-readable code for client behavior.
    pub code: String,
}

/// Errors returned by the Rift HTTP API.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Authentication required")]
    Unauthorized,

    #[error("Forbidden")]
    Forbidden,

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A public failure with an explicit status and stable client code.
    #[error("{message}")]
    Coded {
        /// HTTP response status.
        status: StatusCode,
        /// Stable machine-readable error code.
        code: &'static str,
        /// Safe human-readable detail.
        message: String,
    },
}

/// Stable constructors for managed-agent control failures.
impl AppError {
    /// Report an optimistic concurrency conflict without hiding the current revision.
    pub fn revision_conflict(current: Option<i64>) -> Self {
        Self::Coded {
            status: StatusCode::CONFLICT,
            code: "revision_conflict",
            message: match current {
                Some(revision) => format!("room roster changed at revision {revision}"),
                None => "room roster changed before the first revision".to_string(),
            },
        }
    }

    /// Report a harness, model, or typed setting that is unavailable on the host.
    pub fn capability_unavailable(message: impl Into<String>) -> Self {
        Self::Coded {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "capability_unavailable",
            message: message.into(),
        }
    }

    /// Report an opaque credential binding that cannot currently be mediated.
    pub fn credential_not_ready(message: impl Into<String>) -> Self {
        Self::Coded {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "credential_not_ready",
            message: message.into(),
        }
    }

    /// Report that standalone Rift has no Henosis runtime controller installed.
    pub fn managed_runtime_unavailable() -> Self {
        Self::Coded {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "managed_runtime_unavailable",
            message: "managed agent runtime is unavailable".to_string(),
        }
    }
}

/// Maps Rift application errors to safe HTTP responses.
impl IntoResponse for AppError {
    /// Converts an application error into its status and JSON error body.
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                self.to_string(),
            ),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden", self.to_string()),
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, "not_found", msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "bad_request", msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, "conflict", msg.clone()),
            AppError::Internal(msg) => {
                tracing::error!("Internal error: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error".into(),
                )
            }
            AppError::Database(e) => {
                tracing::error!("Database error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "database_error",
                    "Internal server error".into(),
                )
            }
            AppError::Coded {
                status,
                code,
                message,
            } => (*status, *code, message.clone()),
        };

        (
            status,
            Json(ApiErrorBody {
                error: message,
                code: code.to_string(),
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
/// Exercises stable managed-control error constructors.
mod tests {
    use super::*;

    /// Constructor variants retain the intended status and machine code.
    #[test]
    fn managed_error_codes_are_stable() {
        let cases = [
            (AppError::revision_conflict(Some(3)), StatusCode::CONFLICT, "revision_conflict"),
            (
                AppError::capability_unavailable("model unavailable"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "capability_unavailable",
            ),
            (
                AppError::credential_not_ready("binding unavailable"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "credential_not_ready",
            ),
            (
                AppError::managed_runtime_unavailable(),
                StatusCode::SERVICE_UNAVAILABLE,
                "managed_runtime_unavailable",
            ),
        ];
        for (error, expected_status, expected_code) in cases {
            match error {
                AppError::Coded { status, code, .. } => {
                    assert_eq!(status, expected_status);
                    assert_eq!(code, expected_code);
                }
                other => panic!("expected coded error, got {other:?}"),
            }
        }
    }
}
