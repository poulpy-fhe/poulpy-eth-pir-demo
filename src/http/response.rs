use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

pub(super) fn blob(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, octet_stream()),
            (header::CACHE_CONTROL, no_store()),
        ],
        bytes,
    )
        .into_response()
}

pub(super) fn problem(code: StatusCode, message: &str) -> Response {
    (code, [(header::CONTENT_TYPE, text())], message.to_string()).into_response()
}

pub(super) fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "directory not built yet")
}

pub(super) fn json() -> HeaderValue {
    HeaderValue::from_static("application/json")
}

pub(super) fn text() -> HeaderValue {
    HeaderValue::from_static("text/plain; charset=utf-8")
}

fn octet_stream() -> HeaderValue {
    HeaderValue::from_static("application/octet-stream")
}

pub(super) fn no_store() -> HeaderValue {
    HeaderValue::from_static("no-store")
}
