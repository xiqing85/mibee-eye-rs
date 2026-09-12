use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};

use rust_embed::Embed;

/// Embedded static assets (frontend files compiled into the binary).
#[derive(Embed)]
#[folder = "static/"]
pub struct StaticAsset;

/// Serve an embedded file by path, with SPA fallback to `index.html`.
pub async fn handle_embedded(req: Request<Body>) -> Response {
    let path = if req.uri().path() == "/" {
        "index.html"
    } else {
        // Strip leading '/'
        &req.uri().path()[1..]
    };

    // Try exact path match first.
    if let Some(content) = StaticAsset::get(path) {
        let mime = mime_type(path);
        return Response::builder()
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(content.data))
            .unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "build error").into_response()
            });
    }

    // SPA fallback: serve index.html for unrecognised paths.
    if let Some(content) = StaticAsset::get("index.html") {
        return Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
            .body(Body::from(content.data))
            .unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "build error").into_response()
            });
    }

    (StatusCode::NOT_FOUND, "Not found").into_response()
}

/// Guess MIME type from file extension.
fn mime_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[tokio::test]
    async fn test_serve_index_html() {
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let resp = handle_embedded(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_serve_index_html_direct() {
        let req = Request::builder()
            .uri("/index.html")
            .body(Body::empty())
            .unwrap();
        let resp = handle_embedded(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_spa_fallback() {
        // Unknown paths should fall back to index.html.
        let req = Request::builder()
            .uri("/some/unknown/path")
            .body(Body::empty())
            .unwrap();
        let resp = handle_embedded(req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
