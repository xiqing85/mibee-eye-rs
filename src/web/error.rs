use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::fmt;

/// Errors that can occur in the web server layer.
#[derive(Debug)]
pub enum WebError {
    /// I/O error (e.g. bind failure, TLS).
    Io(std::io::Error),
    /// Requested resource was not found.
    NotFound(String),
    /// Internal server error.
    Internal(String),
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WebError::Io(e) => write!(f, "I/O error: {e}"),
            WebError::NotFound(msg) => write!(f, "Not found: {msg}"),
            WebError::Internal(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

impl std::error::Error for WebError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WebError::Io(e) => Some(e),
            WebError::NotFound(_) | WebError::Internal(_) => None,
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            WebError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, self.to_string()),
            WebError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            WebError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };
        let body = serde_json::json!({ "error": message });
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn into_body(response: Response) -> axum::body::Body {
        response.into_body()
    }

    #[tokio::test]
    async fn test_not_found_maps_to_404() {
        let body = axum::body::to_bytes(
            into_body(WebError::NotFound("segment 42".into()).into_response()),
            usize::MAX,
        )
        .await
        .unwrap();
        // status is checked via the typed variant below; body carries the message
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "segment 42");
    }

    #[tokio::test]
    async fn test_status_codes_per_variant() {
        let io_err = WebError::Io(std::io::Error::other("bind failed"));
        assert_eq!(
            io_err.into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            WebError::NotFound("x".into()).into_response().status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            WebError::Internal("x".into()).into_response().status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn test_display_and_error_source() {
        let io_err = WebError::Io(std::io::Error::other("nope"));
        assert!(io_err.to_string().starts_with("I/O error:"));
        assert!(
            std::error::Error::source(&io_err).is_some(),
            "Io chains the source"
        );

        assert_eq!(
            WebError::NotFound("gone".into()).to_string(),
            "Not found: gone"
        );
        assert_eq!(
            WebError::Internal("boom".into()).to_string(),
            "Internal error: boom"
        );
        assert!(std::error::Error::source(&WebError::Internal("b".into())).is_none());
        assert!(std::error::Error::source(&WebError::NotFound("g".into())).is_none());
    }
}
