use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("ref not found: {0}")]
    RefNotFound(String),
    #[error("path not found: {0}")]
    PathNotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// Objects a partial clone has not fetched yet; `api::with_repo` fetches them and retries.
    #[error("objects not in the cache: {0:?}")]
    MissingObjects(Vec<gix::ObjectId>),
}

impl AppError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::Internal(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::RefNotFound(_) | Self::PathNotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unavailable(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "5")],
                    Json(json!({ "error": self.to_string() })),
                )
                    .into_response();
            }
            Self::Internal(_) | Self::MissingObjects(_) => {
                tracing::error!("{self}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "internal error" })),
                )
                    .into_response();
            }
        };
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}
