use std::path::{Path, PathBuf};

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::response::problem;

pub(super) fn router(dir: PathBuf) -> Router {
    Router::new().fallback(get(move |uri: axum::http::Uri| {
        let dir = dir.clone();
        async move { serve_file(dir, uri).await }
    }))
}

async fn serve_file(root: PathBuf, uri: axum::http::Uri) -> Response {
    let Some(requested) = clean_path(uri.path()) else {
        return problem(StatusCode::BAD_REQUEST, "bad path");
    };
    let path = root.join(requested);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime_of(&path))],
            bytes,
        )
            .into_response(),
        Err(_) => problem(StatusCode::NOT_FOUND, "not found"),
    }
}

fn clean_path(path: &str) -> Option<&str> {
    let requested = path.trim_start_matches('/');
    let requested = if requested.is_empty() {
        "index.html"
    } else {
        requested
    };
    (!requested.split('/').any(|c| c == ".." || c == ".")).then_some(requested)
}

fn mime_of(path: &Path) -> HeaderValue {
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    HeaderValue::from_static(mime)
}
