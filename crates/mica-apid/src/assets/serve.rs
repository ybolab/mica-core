//! The asset router: `docs/design/api.md` §4.1 rule 4, §4.2 and §4.3's
//! application.
//!
//! [`fallback`] is mounted as the router's fallback, which is what makes §4.1's
//! precedence structural: axum matches declared routes before it consults a
//! fallback, so a bundle shipping a file at `api/v1/settings` or at `healthz`
//! cannot capture either because this function is never called for a path the
//! router matched. [`super::path::resolve`] also rejects repeated-separator and
//! encoded aliases of the reserved `api` and `_ui` roots; this is the boundary
//! guard for spellings Axum did not structurally claim, not a fallback into
//! either reserved domain. [`root`] is `GET /`, §4.1's single declared
//! exception: the active bundle's `index.html` when a bundle is active and its
//! index is readable, and a redirect to the reserved built-in `/_ui/` otherwise.
//!
//! [`fallback`] answers §4.2's five conditions in order — condition 1 is the
//! mounting above and costs no code, 2 is the method check, 3 and 4 are
//! [`offers_html`] and [`ends_in_a_route_segment`], 5 is whether
//! [`serve_index`] produced anything. A custom-SPA fallback exists only while
//! a readable custom index exists; without one the path is a 404 and the
//! browser enters the built-in application through `/` or `/_ui/`. §4.3 is
//! applied rather than re-decided: [`super::mime::content_type`]
//! and [`super::mime::cache_class`] answer the headers, `nosniff` goes on every
//! response this module builds, and every HTML document — bundle index, SPA
//! fallback and built-in UI alike — is `no-store`.

use std::fs;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, ALLOW, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};

use super::mime::{self, CacheClass, NOSNIFF, X_CONTENT_TYPE_OPTIONS};
use super::path::{self as asset_path};
use crate::bundle::{Manifest, Store};
use crate::routes::AppState;

/// The one file §5.3 requires at a bundle root, and the document §4.2's
/// fallback returns.
const INDEX: &str = "index.html";

/// The optional manifest §5.3 reads at install time; read here for the one
/// field §4.3 needs at request time.
const MANIFEST: &str = "mica-ui.json";

/// `GET /` — §4.1's one exception to rule 3.
///
/// There is no `Accept` condition and no extension condition here. §4.2's five
/// conditions govern the *fallback*; `/` is a declared route and §4.1 states
/// its rule in two branches and no more: the active bundle's index when a
/// bundle is active and its index is readable, a redirect to `/_ui/` otherwise.
pub async fn root(State(state): State<AppState>) -> Response {
    match active_root(state.bundles())
        .as_deref()
        .and_then(serve_index)
    {
        Some(response) => response,
        None => Redirect::to("/_ui/").into_response(),
    }
}

/// §4.1 rule 4: everything else, as the router's fallback.
pub async fn fallback(State(state): State<AppState>, request: Request) -> Response {
    // §4.2 condition 2. A `POST` that reaches the asset router is a client
    // error and gets 405 — never HTML, and never the fallback below.
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return method_not_allowed();
    }
    let accept = request
        .headers()
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok());
    respond(&state, request.uri().path(), accept).await
}

async fn respond(state: &AppState, request_path: &str, accept: Option<&str>) -> Response {
    let root = active_root(state.bundles());
    if let Some(root) = root.as_deref() {
        match asset_path::resolve(request_path, root) {
            // A hit. §6.1 class 4's inner-asset half: a file that will not
            // open is a 404 for that file and nothing else changes.
            Ok(file) if file.is_file() => return serve_file(root, &file).unwrap_or_else(not_found),
            // A directory is a miss. No bundle is served a directory listing,
            // and §4.2 decides what a miss becomes.
            Ok(_) => {}
            // A plain miss — the one rejection §4.2's fallback may answer.
            Err(rejection) if rejection.eligible_for_fallback() => {}
            // A guard fired: the request was hostile or malformed (§4.4). It
            // must never come back as `200 text/html`.
            Err(_) => return not_found(),
        }
    }
    // §4.2 conditions 3 and 4.
    if !offers_html(accept) || !ends_in_a_route_segment(request_path) {
        return not_found();
    }
    // Condition 5. A custom route is meaningful only when a readable custom
    // index exists; `/_ui` is the separate, unconditional built-in SPA.
    root.as_deref()
        .and_then(serve_index)
        .unwrap_or_else(not_found)
}

/// The resolved root of the active bundle, or `None` for §6.1 class 1.
///
/// The generation is read off `current` and the root is then rebuilt from it,
/// rather than canonicalising the link and serving wherever it points. The
/// difference is what happens to a `current` repointed outside the store over
/// a root shell: the link's target is never trusted as a root, so such a
/// pointer reads as "no bundle" and the site root redirects to `/_ui/`.
///
/// The root handed to [`asset_path::resolve`] is canonical, which §4.4
/// requires of the caller: the assertion is made against the resolved bundle
/// root and never against the appliance-managed `current` symlink.
///
/// An absent store, an absent `current` and a dangling `current` are one
/// answer, and none of them is an error.
fn active_root(store: &Store) -> Option<PathBuf> {
    let generation = store.active_generation().ok().flatten()?;
    store.bundle_dir(generation).canonicalize().ok()
}

/// The bundle's `index.html`, or `None` for §6.1 class 2 or 4.
///
/// Resolved through §4.4's rules like any other asset, so an `index.html` that
/// is a symlink or a directory is not served — which is §6.1 class 2's
/// "index is not a regular file", answered by the site-root redirect or a
/// custom-fallback 404, depending on which path was requested.
fn serve_index(root: &Path) -> Option<Response> {
    let index = asset_path::resolve_relative(INDEX, root).ok()?;
    serve_file(root, &index)
}

/// A file from the bundle, with §4.3's headers, or `None` when it will not
/// open.
fn serve_file(root: &Path, file: &Path) -> Option<Response> {
    // Assets are read whole into memory per request — simple, and right for
    // dashboard-sized files — which makes the file's size the request's
    // memory bill. Bundle installation validates entry types but not sizes,
    // so without a ceiling one oversized file in a bundle would let every
    // concurrent GET of it allocate that much heap. Refusal reads as "will
    // not open", the same answer an unreadable file gives.
    const MAX_ASSET_BYTES: u64 = 32 * 1024 * 1024;
    let len = fs::metadata(file).ok()?.len();
    if len > MAX_ASSET_BYTES {
        tracing::warn!(
            file = %file.display(),
            len,
            "asset exceeds the {MAX_ASSET_BYTES}-byte serving ceiling; treated as unservable"
        );
        return None;
    }
    let body = fs::read(file).ok()?;
    let relative = file.strip_prefix(root).ok()?;
    let class = mime::cache_class(relative, immutable_dir(root).as_deref());
    Some(asset_response(body, Some(mime::content_type(file)), class))
}

/// The directory §5.3's `mica-ui.json` declares immutable, for §4.3's third
/// cache class.
///
/// Read per request, from the served tree. The store validates and records the
/// manifest at install time but exposes only its name and version afterwards
/// (`bundle::ManifestSummary`), so this is the reading that reaches
/// `immutableDir`; caching it would be state needing invalidation on every
/// activation, and the immutable class is opt-in, so a bundle that declares
/// nothing pays one `ENOENT`.
fn immutable_dir(root: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(root.join(MANIFEST)).ok()?;
    let manifest: Manifest = serde_json::from_str(&text).ok()?;
    Some(PathBuf::from(manifest.immutable_dir))
}

/// §4.2 condition 3: the client asked for HTML.
///
/// The header must offer `text/html` explicitly, as `text/html` or as `text/*`.
/// A `*/*` does not count, and neither does an absent `Accept`. That is what
/// §4.2's acceptance property forces — "a request that a developer expected to
/// be JSON never returns HTML with a 200" — because the request reaching here
/// expecting JSON is commonly a `fetch()` that set no `Accept` at all, whose
/// default is `*/*`. Counting `*/*` as an offer of HTML hands exactly that
/// request a 200 and an HTML body.
///
/// The cost, stated: `curl https://<device>/settings/network` gets a 404 where
/// a browser at the same URL gets the application. Browsers always send an
/// explicit `text/html` on a navigation, so no navigation is affected, and `GET
/// /` is a declared route with no `Accept` condition, so the device root still
/// answers a bare `curl` with the bundle's index.
fn offers_html(accept: Option<&str>) -> bool {
    accept.is_some_and(|accept| {
        accept.split(',').any(|range| {
            let range = range.split(';').next().unwrap_or_default().trim();
            range.eq_ignore_ascii_case("text/html") || range.eq_ignore_ascii_case("text/*")
        })
    })
}

/// §4.2 condition 4: the final path segment contains no `.`.
///
/// Implemented as written. §4.2 names it a heuristic and names its cost — a UI
/// with a route literally called `/v1.2/report` gets a 404 where it wanted a
/// fallback — and neither is improved here.
///
/// A `%` in the segment counts as a `.` might. The condition is about the
/// decoded name, and a `%` that survives decoding is
/// [`asset_path::Rejection::ResidualEscape`], so it never reaches this
/// function: any `%` here was an escape and may decode to a dot. Reading it as
/// an asset is the direction that can only produce a 404, never HTML for
/// something that was a filename.
fn ends_in_a_route_segment(request_path: &str) -> bool {
    let last = request_path.rsplit('/').next().unwrap_or_default();
    !last.contains('.') && !last.contains('%')
}

/// 404 with an empty body (§4.2 condition 4).
fn not_found() -> Response {
    let mut response = asset_response(Vec::new(), None, CacheClass::NoCache);
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// 405 for a method §4.2 condition 2 does not admit.
fn method_not_allowed() -> Response {
    let mut response = asset_response(Vec::new(), None, CacheClass::NoCache);
    *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

/// Every response this module builds, with §4.3's two headers on it.
///
/// `content_type` is `None` for the empty-bodied refusals: there is no entity
/// to type. `nosniff` goes on those too — §4.3 asks for it on **every** asset
/// response, and a response with no content type is the last one that should
/// be sniffed.
///
/// A `HEAD` is answered with the body its `GET` would carry and hyper drops it
/// on the wire, which is how axum's own `get()` handles the method it also
/// accepts. The headers a client asked for are therefore the ones it gets.
fn asset_response(
    body: Vec<u8>,
    content_type: Option<&'static str>,
    class: CacheClass,
) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    if let Some(content_type) = content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(class.header_value()),
    );
    headers.insert(
        HeaderName::from_static(X_CONTENT_TYPE_OPTIONS),
        HeaderValue::from_static(NOSNIFF),
    );
    response
}
