use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// An API failure, sent to the client as an HTTP status and a JSON `error` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    NotFound(String),
    Bad(String),
    Forbidden(String),
    Conflict(String),
    Unauthorized,
    Internal(String),
}

impl From<ferro::Error> for ApiError {
    fn from(error: ferro::Error) -> Self {
        match error {
            ferro::Error::NoVersion(version) => ApiError::NotFound(format!("no version {version}")),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, what) = match self {
            ApiError::NotFound(what) => (StatusCode::NOT_FOUND, what),
            ApiError::Bad(what) => (StatusCode::UNPROCESSABLE_ENTITY, what),
            ApiError::Forbidden(what) => (StatusCode::FORBIDDEN, what),
            ApiError::Conflict(what) => (StatusCode::CONFLICT, what),
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "send x-user".to_string()),
            ApiError::Internal(what) => (StatusCode::INTERNAL_SERVER_ERROR, what),
        };
        (status, Json(json!({ "error": what }))).into_response()
    }
}
