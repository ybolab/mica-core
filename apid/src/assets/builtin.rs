//! Verity-covered built-in SPA assets compiled into `apid` as a virtual tree.

use std::path::Path;

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;

use super::mime::{self, CacheClass, NOSNIFF, X_CONTENT_TYPE_OPTIONS};
use super::path::LogicalPath;

struct EmbeddedAsset {
    path: &'static str,
    bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/builtin_assets.rs"));

const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; style-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Stable built-in SPA entry at both `/_ui` and `/_ui/`.
pub async fn index() -> Response {
    lookup("index.html")
        .map(asset_response)
        .unwrap_or_else(internal_error)
}

/// One path below the already-selected `/_ui` namespace.
pub async fn serve(OriginalUri(uri): OriginalUri) -> Response {
    let Some(relative) = uri.path().strip_prefix("/_ui/") else {
        return not_found();
    };
    let logical = match LogicalPath::parse(relative) {
        Ok(logical) => logical,
        Err(_) => return not_found(),
    };
    if let Some(asset) = lookup(logical.as_str()) {
        return asset_response(asset);
    }

    let route_like = logical
        .as_str()
        .rsplit('/')
        .next()
        .is_some_and(|segment| !segment.contains('.'));
    if route_like {
        return index().await;
    }
    not_found()
}

fn lookup(path: &str) -> Option<&'static EmbeddedAsset> {
    BUILTIN_ASSETS
        .binary_search_by_key(&path, |asset| asset.path)
        .ok()
        .map(|index| &BUILTIN_ASSETS[index])
}

fn asset_response(asset: &'static EmbeddedAsset) -> Response {
    let cache = if asset.path == "index.html" {
        CacheClass::NoStore
    } else if asset.path.starts_with("assets/") {
        CacheClass::Immutable
    } else {
        CacheClass::NoCache
    };
    response(
        StatusCode::OK,
        Body::from(asset.bytes),
        Some(mime::content_type(Path::new(asset.path))),
        cache,
    )
}

fn not_found() -> Response {
    response(
        StatusCode::NOT_FOUND,
        Body::empty(),
        None,
        CacheClass::NoCache,
    )
}

fn internal_error() -> Response {
    response(
        StatusCode::INTERNAL_SERVER_ERROR,
        Body::empty(),
        None,
        CacheClass::NoStore,
    )
}

fn response(
    status: StatusCode,
    body: Body,
    content_type: Option<&'static str>,
    cache: CacheClass,
) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
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

#[cfg(test)]
pub(crate) fn embedded_paths() -> impl Iterator<Item = &'static str> {
    BUILTIN_ASSETS.iter().map(|asset| asset.path)
}
