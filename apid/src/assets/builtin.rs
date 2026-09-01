//! Verity-covered built-in SPA assets compiled into `apid`.

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;

use super::mime::{CacheClass, NOSNIFF, X_CONTENT_TYPE_OPTIONS};

const INDEX: &[u8] = include_bytes!("../../ui/dist/index.html");
const APP_JS: &[u8] = include_bytes!("../../ui/dist/assets/app.js");
const APP_CSS: &[u8] = include_bytes!("../../ui/dist/assets/app.css");

const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; style-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

pub async fn index() -> Response {
    asset(INDEX, "text/html; charset=utf-8", CacheClass::NoStore)
}

pub async fn app_js() -> Response {
    asset(
        APP_JS,
        "text/javascript; charset=utf-8",
        CacheClass::NoCache,
    )
}

pub async fn app_css() -> Response {
    asset(APP_CSS, "text/css; charset=utf-8", CacheClass::NoCache)
}

/// File-like misses stay misses; extensionless client routes load the SPA.
pub async fn fallback(OriginalUri(uri): OriginalUri) -> Response {
    let last = uri.path().rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    index().await
}

fn asset(bytes: &'static [u8], content_type: &'static str, cache: CacheClass) -> Response {
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(cache.header_value()),
    );
    headers.insert(
        HeaderName::from_static(X_CONTENT_TYPE_OPTIONS),
        HeaderValue::from_static(NOSNIFF),
    );
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

use axum::response::IntoResponse as _;
