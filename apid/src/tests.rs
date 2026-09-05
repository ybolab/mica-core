//! Route-level tests driving the router directly with the fake settings
//! backend; no network or bus daemon involved.
//!
//! The exceptions are [`power_bus`] and [`settings_signal`], which drive the
//! real D-Bus client against a fake mosd on a private bus, because what they
//! assert lives below the fake backend's trait.

mod broken_classes;
mod claim;
mod diagnostics;
mod power_bus;
mod provisioning_api;
mod reset;
mod settings_signal;
mod update_api;

use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::header::{
    ACCEPT, ALLOW, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, COOKIE, LOCATION, RETRY_AFTER,
    SET_COOKIE,
};
use axum::http::{HeaderName, Request, Response, StatusCode};
use serde_json::json;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::assets::serve;
use crate::auth;
use crate::bundle::Store;
use crate::routes::{AppState, app};
use crate::settings_api::{FakeSettings, InvalidTaskPayload, SettingsApi};
use crate::task_registry::TaskRecord;

const SIGNING_KEY: [u8; 32] = [7u8; 32];

fn test_app(tree: serde_json::Value) -> (Router, Arc<FakeSettings>) {
    let fake = Arc::new(FakeSettings::new(tree));
    let state = AppState::new(fake.clone(), SIGNING_KEY);
    (app(state), fake)
}

fn unconfigured_tree() -> serde_json::Value {
    json!({ "hostname": "mos", "network": {}, "access": {} })
}

fn configured_tree(password: &str) -> serde_json::Value {
    let hash = auth::hash_password(password).unwrap();
    json!({
        "hostname": "mos",
        "network": {},
        "access": { "webAdmin": { "password_hash": hash } },
    })
}

// Log in against a configured tree and return the session cookie value.
async fn login(router: &Router, password: &str) -> String {
    let response = json_request(
        router,
        "POST",
        "/api/v1/session",
        json!({ "password": password }),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    session_cookie_value(&response)
}

async fn body_string(response: Response<axum::body::Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

// A bounded call is not an ordinary connectivity failure: mosd may still be
// applying a write after apid gives the request back, so the response must
// say "not confirmed" (504) rather than "unreachable" (503).
#[tokio::test]
async fn a_mosd_call_timeout_has_its_own_api_classification() {
    let err = anyhow::Error::new(crate::bus_client::MosdCallTimeout::new(
        "SetSettings",
        std::time::Duration::from_secs(5),
    ));

    let response = crate::routes::bus_api_error(&err, Some("hostname"));
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert!(response.headers().get(RETRY_AFTER).is_none());
    let body: serde_json::Value =
        serde_json::from_str(&body_string(response).await).expect("JSON envelope");
    assert_eq!(body["error"]["code"], "mosd_timeout");
    assert_eq!(body["error"]["path"], "hostname");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("may still be running")),
        "the timeout must not claim the operation failed: {body}"
    );
}

// A task payload that reached apid but does not match the bus contract is a
// daemon failure, not a connectivity outage, and must match OpenAPI's 500.
#[tokio::test]
async fn an_invalid_task_payload_is_a_mosd_failure() {
    let parse_error =
        serde_json::from_str::<TaskRecord>("{}").expect_err("an empty object is not a task record");
    let err = anyhow::Error::new(InvalidTaskPayload(parse_error));

    let response = crate::routes::bus_api_error(&err, None);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(response.headers().get(RETRY_AFTER).is_none());
    let body: serde_json::Value =
        serde_json::from_str(&body_string(response).await).expect("JSON envelope");
    assert_eq!(body["error"]["code"], "mosd_failed");
    assert_eq!(body["error"]["source"], "mosd");
}

async fn send(router: &Router, request: Request<Body>) -> Response<axum::body::Body> {
    router.clone().oneshot(request).await.unwrap()
}

async fn get(router: &Router, path: &str, cookie: Option<&str>) -> Response<axum::body::Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, format!("apid_session={cookie}"));
    }
    send(router, builder.body(Body::empty()).unwrap()).await
}

async fn post_form(
    router: &Router,
    path: &str,
    body: &str,
    cookie: Option<&str>,
) -> Response<axum::body::Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, format!("apid_session={cookie}"));
    }
    send(router, builder.body(Body::from(body.to_string())).unwrap()).await
}

async fn json_request(
    router: &Router,
    method: &str,
    path: &str,
    body: serde_json::Value,
    cookie: Option<&str>,
    csrf: Option<&str>,
) -> Response<axum::body::Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, format!("apid_session={cookie}"));
    }
    if let Some(csrf) = csrf {
        builder = builder.header("x-csrf-token", csrf);
    }
    send(router, builder.body(Body::from(body.to_string())).unwrap()).await
}

async fn zip_request(
    router: &Router,
    path: &str,
    bytes: Vec<u8>,
    cookie: &str,
    csrf: Option<&str>,
) -> Response<axum::body::Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(CONTENT_TYPE, "application/zip")
        .header(COOKIE, format!("apid_session={cookie}"));
    if let Some(csrf) = csrf {
        builder = builder.header("x-csrf-token", csrf);
    }
    send(router, builder.body(Body::from(bytes)).unwrap()).await
}

fn location(response: &Response<axum::body::Body>) -> &str {
    response.headers().get(LOCATION).unwrap().to_str().unwrap()
}

// The `apid_session=<value>` part of the `Set-Cookie` response header.
fn session_cookie_value(response: &Response<axum::body::Body>) -> String {
    let header = response
        .headers()
        .get(SET_COOKIE)
        .expect("Set-Cookie header")
        .to_str()
        .unwrap();
    let (pair, attrs) = header.split_once(';').unwrap();
    for attr in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(attrs.contains(attr), "cookie should carry {attr}: {header}");
    }
    pair.strip_prefix("apid_session=").unwrap().to_string()
}

#[tokio::test]
async fn session_api_reports_setup_and_authenticates_the_browser() {
    let (setup_router, _) = test_app(unconfigured_tree());
    let setup = get(&setup_router, "/api/v1/session", None).await;
    assert_eq!(setup.status(), StatusCode::OK);
    let setup: serde_json::Value =
        serde_json::from_str(&body_string(setup).await).expect("session JSON");
    assert_eq!(setup, json!({ "state": "setup" }));

    let (router, _) = test_app(configured_tree("hunter2secret"));
    let anonymous = get(&router, "/api/v1/session", None).await;
    assert_eq!(anonymous.status(), StatusCode::OK);
    let anonymous: serde_json::Value =
        serde_json::from_str(&body_string(anonymous).await).expect("session JSON");
    assert_eq!(anonymous, json!({ "state": "unauthenticated" }));

    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    assert_eq!(login.status(), StatusCode::CREATED);
    let cookie = session_cookie_value(&login);
    let login_body: serde_json::Value =
        serde_json::from_str(&body_string(login).await).expect("session JSON");
    assert_eq!(login_body["state"], "authenticated");
    let csrf = login_body["csrfToken"]
        .as_str()
        .expect("authenticated session carries CSRF token");
    assert_eq!(csrf.len(), 64);

    let authenticated = get(&router, "/api/v1/session", Some(&cookie)).await;
    assert_eq!(authenticated.status(), StatusCode::OK);
    let authenticated: serde_json::Value =
        serde_json::from_str(&body_string(authenticated).await).expect("session JSON");
    assert_eq!(authenticated, login_body);
}

#[tokio::test]
async fn cookie_api_access_requires_csrf_only_for_mutations() {
    let (router, fake) = test_app(configured_tree("hunter2secret"));
    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    let cookie = session_cookie_value(&login);
    let login_body: serde_json::Value =
        serde_json::from_str(&body_string(login).await).expect("session JSON");
    let csrf = login_body["csrfToken"].as_str().unwrap();

    let read = get(&router, "/api/v1/meta", Some(&cookie)).await;
    assert_eq!(read.status(), StatusCode::OK);

    let missing = json_request(
        &router,
        "PUT",
        "/api/v1/settings/hostname",
        json!("new-host"),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(missing.status(), StatusCode::FORBIDDEN);
    let missing: serde_json::Value =
        serde_json::from_str(&body_string(missing).await).expect("API error JSON");
    assert_eq!(missing["error"]["code"], "csrf_invalid");

    let accepted = json_request(
        &router,
        "PUT",
        "/api/v1/settings/hostname",
        json!("new-host"),
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    assert_eq!(
        fake.get_settings("hostname").await.unwrap(),
        json!("new-host")
    );
}

#[tokio::test]
async fn network_api_reports_configured_and_observed_interfaces() {
    let (router, fake) = test_app(json!({
        "hostname": "mos",
        "network": { "eth0": { "dhcp": true } },
        "access": {
            "webAdmin": { "password_hash": auth::hash_password("hunter2secret").unwrap() }
        }
    }));
    fake.set_state_entry(
        "network",
        json!({
            "interfaceCount": 2,
            "interfaces": [
                {
                    "index": 1,
                    "name": "lo",
                    "operationalState": "carrier",
                    "carrierState": "carrier"
                },
                {
                    "index": 2,
                    "name": "eth0",
                    "operationalState": "routable",
                    "carrierState": "carrier",
                    "addresses": [{ "Address": [192, 0, 2, 10], "PrefixLength": 24 }]
                }
            ]
        }),
    );
    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    let cookie = session_cookie_value(&login);

    let response = get(&router, "/api/v1/network", Some(&cookie)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(response).await).expect("network JSON");
    assert_eq!(body["configuredCount"], 1);
    assert_eq!(body["configured"]["eth0"]["dhcp"], true);
    assert_eq!(body["observed"]["available"], true);
    assert_eq!(body["observed"]["interfaceCount"], 2);
    assert_eq!(body["observed"]["interfaces"][1]["name"], "eth0");
    assert_eq!(
        body["observed"]["interfaces"][1]["operationalState"],
        "routable"
    );
}

// Uptime reaches the pane from mosd's live-state tree and from nowhere else:
// a backend with no `uptime` state renders the unavailable notice, where a
// handler that still read `/proc/uptime` for itself would render a real
// number on any Linux host.

// The reproduction, end to end: the form accepts `eth0.100`, the
// write reaches the single key `eth0.100`, and the pane reads it back.
//
// The write path is quoted, so `split_path` yields two segments and not
// three; the value the daemon deserializes is an `IfaceSettings` and not an
// `IfaceSettings` carrying an unknown field `100`, which is what
// `deny_unknown_fields` used to refuse.

// The path apid builds for a dotted name is a path the real settings model
// accepts — the half that lived below the fake backend.
//
// `FakeSettings` answers for the bus, not for `mosd_settings`; this drives
// the string apid actually sends into [`mosd_settings::Settings`], whose
// `IfaceSettings` carries `deny_unknown_fields`, and records the unquoted
// spelling the task filed as the failure it still is.
#[test]
fn the_dotted_iface_path_apid_builds_is_accepted_by_the_settings_model() {
    let value = json!({ "dhcp": true });

    let mut settings = mosd_settings::Settings::default();
    settings
        .set(r#"network."eth0.100""#, value.clone())
        .unwrap();
    assert_eq!(
        settings.network.keys().collect::<Vec<_>>(),
        vec!["eth0.100"]
    );
    assert!(settings.network["eth0.100"].dhcp);

    // The recorded spelling: three segments, so `100` is offered as a
    // field of `IfaceSettings` and refused.
    let err = mosd_settings::Settings::default().set("network.eth0.100", value);
    assert!(
        matches!(&err, Err(mosd_settings::SettingsError::Validation { message, .. }) if message.contains("unknown field `100`")),
        "{err:?}"
    );
}

// SSH pane

// Real `ssh-keygen` output, the same three keys `mosd/mosd/src/reconciler/
// sshd.rs` tests against, so both sides of the D-Bus boundary are exercised
// with identical input. Public keys are not secrets; these correspond to no
// device and the private halves were discarded at generation.
const REAL_ED25519_LINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIL99V7xPTOP3jZjnbVPM7xC+ckwzkOQPalUpsvtPzYo8 rfct-034-test-ed25519";
const REAL_RSA_LINE: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDT2F3imgGgI+xGNSQI+0alU1qRwyU3gCc8wU6msXSzZsVc8OYlg4VIqxsV/GLpBmgRz5lGoxjTT2TU0t1VwaMs845NqRIWzpG88ohD1LMn7RnUrNTxf4syFuvmELmYstqMfc6Q6rApqFoA6023Rl2orgd8N3SQ2wPAw8Rk9OLwim9/R7tX8C8FTbnMtepzTvOUNGTDAaKYhTZZnZpsGCwKa9f2aWyaS2XqLwn9uWpmHRUAkV10l45W2rLhnceejwwHotlZUIAFt8rlmS1ojRaLWqECVAuO5CDTt64KLLRniw8yHIYsWkeVsHZXCxq+J7oUVI3ogOSYs1M4I2eFCccD rfct-034-test-rsa";

// Fingerprints as `ssh-keygen -lf` printed them for the three keys above.
// Comparing apid's fingerprint against values that came out of OpenSSH is the
// point: a fingerprint checked only against itself proves nothing, and this
// one is the handle a removal is addressed by.
const REAL_ED25519_FINGERPRINT: &str = "SHA256:HrgN3GLi6Mop2uSRjgOoxImM8zRkFmgqCKoeGD9QOaM";
const REAL_RSA_FINGERPRINT: &str = "SHA256:zv0xTYuVTo5pFpcl/svzzz/vJFvoguWxKlghlXQS1bE";
const REAL_ED25519_SECOND_FINGERPRINT: &str = "SHA256:d7yiR/zCsNFh8WmU6CGLWEG5vE06icIelqVoNc8TT2E";

// Percent-encode one form value.
//
// Everything outside the unreserved set is escaped, including the space, so a
// case that is about a stray space or a control character survives the trip
// to the handler as the byte it is meant to be.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// The canonical `<type> <blob>` half of a full `ssh-keygen` line.
fn canonical(line: &str) -> String {
    let mut fields = line.splitn(3, ' ');
    let key_type = fields.next().unwrap();
    let blob = fields.next().unwrap();
    format!("{key_type} {blob}")
}

// The comment half of a full `ssh-keygen` line.
fn comment_of(line: &str) -> &str {
    line.splitn(3, ' ').nth(2).unwrap()
}

// A configured tree carrying an `access.ssh` subtree holding `keys`.
fn ssh_tree(keys: serde_json::Value) -> serde_json::Value {
    let hash = auth::hash_password("hunter2secret").unwrap();
    json!({
        "hostname": "mos",
        "network": {},
        "access": {
            "webAdmin": { "password_hash": hash },
            "ssh": {
                "enabled": false,
                "port": 22,
                "permitRootLogin": true,
                "passwordAuthentication": true,
                "listenAddresses": [],
                "authorizedKeys": keys,
            },
        },
    })
}

// The stored form of one parsed key: comment split out of the key text.
fn stored_key(line: &str) -> serde_json::Value {
    json!({ "key": canonical(line), "comment": comment_of(line) })
}

// One of an unbounded family of distinct, structurally real ed25519 key lines.
//
// Derived from the committed fixture by overwriting the last byte of its
// 32-byte public key, so every line parses, declares `ssh-ed25519` inside its
// blob the way `parse_authorized_key` requires, and differs from every other.
// A cap test needs distinct keys specifically: repeating one fixture would
// meet the duplicate rule long before the bound.
//
// The private halves were never generated, so none of these authorises
// anything anywhere.
fn generated_key_line(index: u8) -> String {
    let blob = REAL_ED25519_LINE
        .split(' ')
        .nth(1)
        .expect("the fixture is `<type> <blob> <comment>`");
    let mut bytes = mosd_settings::decode_base64(blob).expect("the fixture blob decodes");
    let last = bytes.len() - 1;
    bytes[last] = index;
    format!("ssh-ed25519 {}", mosd_settings::encode_base64_nopad(&bytes))
}

// The stored key list, as JSON.
async fn stored_key_list(fake: &FakeSettings) -> serde_json::Value {
    fake.get_settings("access.ssh.authorizedKeys")
        .await
        .unwrap()
}

#[tokio::test]
async fn a_running_task_missing_after_resubscribe_becomes_interrupted() {
    let (tree, token) = with_token(ssh_tree(json!([])));
    let fake = Arc::new(FakeSettings::new(tree));
    let state = AppState::new(fake, SIGNING_KEY);
    let registry = state.task_registry().clone();
    registry.subscribed();
    registry.update(TaskRecord {
        id: "task-before-restart".to_string(),
        operation: "settings-write".to_string(),
        dot_path: "access.ssh.enabled".to_string(),
        source: ":1.9".to_string(),
        status: "running".to_string(),
        enqueued_at: "2026-08-31T00:00:00.000Z".to_string(),
        started_at: Some("2026-08-31T00:00:01.000Z".to_string()),
        finished_at: None,
        outcome: None,
        message: None,
        folded_count: 0,
    });
    registry.lapsed();
    registry.subscribed();
    assert_eq!(registry.get("task-before-restart"), None);

    let router = app(state);
    let response = bearer(&router, "GET", "/api/v1/tasks/task-before-restart", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let task = body_json(response).await;
    assert_eq!(task["status"], "finished", "{task}");
    assert_eq!(task["outcome"], "interrupted", "{task}");
    assert!(task["finishedAt"].as_str().is_some(), "{task}");
}

// The asset router: §4.1 precedence, the reserved `/api/` subtree, §4.2's SPA
// fallback and §4.3's headers as applied.

// What a browser sends on a navigation.
const BROWSER_ACCEPT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";

// The served set §2.1 owns. Passed in because this phase does not define one
// and must not invent it; a bundle declaring `v1` intersects it.
const SERVED: &[&str] = &["v1"];

// Stage `files` as generation 1 and activate it, returning the store's root.
//
// Installation goes through `bundle::Store::activate` rather than writing
// `bundles/1` and `current` by hand, so the tree these tests serve is a tree
// §5.3 accepted: validated, mode-normalised, digested and pointed at by a
// renamed symlink.
fn install_bundle(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("temp bundle store");
    let store = Store::new(dir.path());
    let staging = store.staging_dir(1);
    for (relative, contents) in files {
        let path = staging.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).expect("create staged parent");
        std::fs::write(path, contents).expect("write staged file");
    }
    store.activate(1, SERVED).expect("activate the staged tree");
    dir
}

// Every regular file in the installed tree, relative to the bundle root,
// sorted.
fn installed_files(root: &Path) -> Vec<String> {
    let store = Store::new(root);
    let generation = store
        .active_generation()
        .expect("read current")
        .expect("a bundle is active");
    let bundle = store.bundle_dir(generation);
    let mut found = Vec::new();
    let mut stack = vec![bundle.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read installed dir") {
            let entry = entry.expect("installed dir entry");
            if entry.file_type().expect("entry type").is_dir() {
                stack.push(entry.path());
            } else {
                found.push(
                    entry
                        .path()
                        .strip_prefix(&bundle)
                        .expect("inside the bundle")
                        .display()
                        .to_string(),
                );
            }
        }
    }
    found.sort();
    found
}

// The router as shipped, with the bundle store rooted at `bundle_root`.
fn test_app_serving(tree: serde_json::Value, bundle_root: &Path) -> Router {
    let fake = Arc::new(FakeSettings::new(tree));
    // A fixed uptime, so the status pane renders its uptime line (which
    // `without_the_uptime_line` requires) from the fake like everything else.
    fake.set_state_entry("uptime", json!(90_061));
    app(AppState::new(fake, SIGNING_KEY).with_bundle_root(bundle_root))
}

#[tokio::test]
async fn ui_boundary_selects_custom_at_root_and_always_reserves_builtin_ui() {
    let empty = TempDir::new().unwrap();
    let router = test_app_serving(configured_tree("hunter2secret"), empty.path());
    let root = get(&router, "/", None).await;
    assert_eq!(root.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&root), "/_ui/");
    for path in ["/_ui", "/_ui/"] {
        let builtin = get(&router, path, None).await;
        assert_eq!(builtin.status(), StatusCode::OK, "{path}");
        assert_eq!(
            builtin.headers().get(CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert!(body_string(builtin).await.contains("/_ui/assets/"));
    }
    let old_builtin = request(&router, "GET", "/ui", None, Some(BROWSER_ACCEPT)).await;
    assert_eq!(old_builtin.status(), StatusCode::NOT_FOUND);

    let builtin_js = crate::assets::builtin::embedded_paths()
        .find(|path| path.ends_with(".js"))
        .expect("the built-in VFS contains JavaScript");
    let custom_shadow = format!("_ui/{builtin_js}");
    let builtin_url = format!("/_ui/{builtin_js}");

    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom-root</title>"),
        (&custom_shadow, "CUSTOM-SHADOW"),
    ]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let root = get(&router, "/", None).await;
    assert_eq!(root.status(), StatusCode::OK);
    assert!(body_string(root).await.contains("custom-root"));
    let response = get(&router, &builtin_url, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!body_string(response).await.contains("CUSTOM-SHADOW"));
}

#[tokio::test]
async fn builtin_prefix_releases_ui_to_the_custom_owner() {
    let builtin_js = crate::assets::builtin::embedded_paths()
        .find(|path| path.ends_with(".js"))
        .expect("the built-in VFS contains JavaScript");
    let custom_shadow = format!("_ui/{builtin_js}");
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom-root</title>"),
        ("ui/asset.js", "CUSTOM-UI-ASSET"),
        (&custom_shadow, "CUSTOM-BUILTIN-SHADOW"),
    ]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());

    let custom = get(&router, "/ui/asset.js", None).await;
    assert_eq!(custom.status(), StatusCode::OK);
    assert_eq!(body_string(custom).await, "CUSTOM-UI-ASSET");

    let custom_route = request(&router, "GET", "/ui", None, Some(BROWSER_ACCEPT)).await;
    assert_eq!(custom_route.status(), StatusCode::OK);
    assert!(body_string(custom_route).await.contains("custom-root"));

    let builtin = get(&router, &format!("/_ui/{builtin_js}"), None).await;
    assert_eq!(builtin.status(), StatusCode::OK);
    assert!(!body_string(builtin).await.contains("CUSTOM-BUILTIN-SHADOW"));
}

#[test]
fn built_in_vfs_contains_sorted_split_output() {
    let paths = crate::assets::builtin::embedded_paths().collect::<Vec<_>>();
    assert!(paths.contains(&"index.html"), "{paths:?}");
    assert!(
        paths.len() > 3,
        "the built-in VFS must not regress to a fixed three-file set: {paths:?}"
    );
    assert!(
        paths.windows(2).all(|pair| pair[0] < pair[1]),
        "generated VFS paths must be sorted: {paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|path| path.starts_with("assets/zh-cn-") && path.ends_with(".js")),
        "the Chinese catalog must remain a lazy chunk: {paths:?}"
    );
    for path in paths {
        assert!(
            crate::assets::path::LogicalPath::parse(path).is_ok(),
            "generated asset is not a safe logical path: {path}"
        );
    }
}

#[tokio::test]
async fn built_in_vfs_serves_every_asset_with_owner_local_cache_rules() {
    let empty = TempDir::new().unwrap();
    let router = test_app_serving(configured_tree("hunter2secret"), empty.path());

    for path in crate::assets::builtin::embedded_paths() {
        let response = get(&router, &format!("/_ui/{path}"), None).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert!(response.headers().contains_key(CONTENT_TYPE), "{path}");
        assert_eq!(
            header_value(&response, CACHE_CONTROL),
            if path == "index.html" {
                "no-store"
            } else if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            },
            "{path}"
        );
        assert_eq!(
            header_value(&response, HeaderName::from_static("x-content-type-options")),
            "nosniff",
            "{path}"
        );
    }

    let fallback = get(&router, "/_ui/network", None).await;
    assert_eq!(fallback.status(), StatusCode::OK);
    assert_eq!(
        header_value(&fallback, CONTENT_TYPE),
        "text/html; charset=utf-8"
    );
    assert_eq!(header_value(&fallback, CACHE_CONTROL), "no-store");

    for path in ["/_ui/assets/missing.js", "/_ui/%252e%252e"] {
        let response = get(&router, path, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(header_value(&response, CACHE_CONTROL), "no-cache", "{path}");
        assert_eq!(body_string(response).await, "", "{path}");
    }
}

#[tokio::test]
async fn ui_selection_is_managed_only_through_the_csrf_protected_api() {
    let bundle = install_bundle(&[("index.html", "<!doctype html><title>custom-root</title>")]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    let cookie = session_cookie_value(&login);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(login).await).expect("session JSON");
    let csrf = body["csrfToken"].as_str().unwrap();

    let status = get(&router, "/api/v1/ui", Some(&cookie)).await;
    assert_eq!(status.status(), StatusCode::OK);
    let status: serde_json::Value =
        serde_json::from_str(&body_string(status).await).expect("UI status JSON");
    assert_eq!(status["mode"], "custom");
    assert_eq!(status["custom"]["generation"], 1);
    assert_eq!(status["availableCustom"]["generation"], 1);
    assert_eq!(status["availableCustom"]["usable"], true);

    let refused = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/active",
        serde_json::Value::Null,
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let accepted = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/active",
        serde_json::Value::Null,
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted: serde_json::Value =
        serde_json::from_str(&body_string(accepted).await).expect("UI status JSON");
    assert_eq!(accepted["mode"], "builtIn");
    assert_eq!(accepted["availableCustom"]["generation"], 1);
    assert_eq!(accepted["availableCustom"]["usable"], true);
    assert_eq!(
        get(&router, "/", None).await.status(),
        StatusCode::SEE_OTHER
    );

    let refused = json_request(
        &router,
        "PUT",
        "/api/v1/ui/active",
        json!({ "generation": 1 }),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    let accepted = json_request(
        &router,
        "PUT",
        "/api/v1/ui/active",
        json!({ "generation": 1 }),
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted: serde_json::Value =
        serde_json::from_str(&body_string(accepted).await).expect("UI status JSON");
    assert_eq!(accepted["mode"], "custom");
    assert_eq!(accepted["custom"]["generation"], 1);
    let root = get(&router, "/", None).await;
    assert_eq!(root.status(), StatusCode::OK);
    assert!(body_string(root).await.contains("custom-root"));
}

fn ui_package_bytes(api_version: &str) -> Vec<u8> {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("assets")).unwrap();
    std::fs::write(
        source.join("index.html"),
        "<!doctype html><title>uploaded</title>",
    )
    .unwrap();
    std::fs::write(source.join("assets/app.js"), "console.log('uploaded')").unwrap();
    std::fs::write(
        source.join("mos-ui.json"),
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "name": "uploaded",
            "version": "1.0.0",
            "immutableDir": "assets",
            "apiVersions": [api_version],
        }))
        .unwrap(),
    )
    .unwrap();
    let package = temp.path().join("uploaded.mos-ui.zip");
    mos_ui_bundle::pack(&source, &package).unwrap();
    std::fs::read(package).unwrap()
}

#[tokio::test]
async fn ui_package_upload_installs_without_activation_and_supports_explicit_lifecycle() {
    let bundle_root = TempDir::new().unwrap();
    let router = test_app_serving(configured_tree("hunter2secret"), bundle_root.path());
    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    let cookie = session_cookie_value(&login);
    let body: serde_json::Value = serde_json::from_str(&body_string(login).await).unwrap();
    let csrf = body["csrfToken"].as_str().unwrap();
    let package = ui_package_bytes("v1");

    let missing_csrf = zip_request(
        &router,
        "/api/v1/ui/bundles",
        package.clone(),
        &cookie,
        None,
    )
    .await;
    assert_eq!(missing_csrf.status(), StatusCode::FORBIDDEN);

    let incompatible = zip_request(
        &router,
        "/api/v1/ui/bundles",
        ui_package_bytes("v999"),
        &cookie,
        Some(csrf),
    )
    .await;
    assert_eq!(incompatible.status(), StatusCode::CONFLICT);
    let incompatible: serde_json::Value =
        serde_json::from_str(&body_string(incompatible).await).unwrap();
    assert_eq!(incompatible["error"]["code"], "ui_package_conflict");

    let uploaded = zip_request(
        &router,
        "/api/v1/ui/bundles",
        package.clone(),
        &cookie,
        Some(csrf),
    )
    .await;
    assert_eq!(uploaded.status(), StatusCode::CREATED);
    let uploaded: serde_json::Value = serde_json::from_str(&body_string(uploaded).await).unwrap();
    assert!(uploaded["activeGeneration"].is_null());
    assert_eq!(uploaded["bundles"][0]["generation"], 1);
    assert_eq!(uploaded["bundles"][0]["name"], "uploaded");
    assert_eq!(uploaded["bundles"][0]["digest"].as_str().unwrap().len(), 64);
    assert!(uploaded["bundles"][0]["compressedBytes"].as_u64().unwrap() > 0);
    assert!(uploaded["bundles"][0]["expandedBytes"].as_u64().unwrap() > 0);
    assert_eq!(
        get(&router, "/", None).await.status(),
        StatusCode::SEE_OTHER
    );

    let duplicate = zip_request(&router, "/api/v1/ui/bundles", package, &cookie, Some(csrf)).await;
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);

    let activated = json_request(
        &router,
        "PUT",
        "/api/v1/ui/active",
        json!({ "generation": 1 }),
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(activated.status(), StatusCode::OK);
    assert!(
        body_string(get(&router, "/", None).await)
            .await
            .contains("uploaded")
    );

    let active_delete = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/bundles/1",
        serde_json::Value::Null,
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(active_delete.status(), StatusCode::CONFLICT);

    let deactivated = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/active",
        serde_json::Value::Null,
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(deactivated.status(), StatusCode::OK);
    let deleted = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/bundles/1",
        serde_json::Value::Null,
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn the_ui_api_refuses_a_corrupt_retained_bundle_without_changing_root() {
    let bundle = install_bundle(&[("index.html", "<!doctype html><title>custom</title>")]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    let cookie = session_cookie_value(&login);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(login).await).expect("session JSON");
    let csrf = body["csrfToken"].as_str().unwrap();

    let deactivated = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/active",
        serde_json::Value::Null,
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(deactivated.status(), StatusCode::OK);
    std::fs::write(
        Store::new(bundle.path()).bundle_dir(1).join("index.html"),
        "changed",
    )
    .expect("corrupt retained bundle");

    let status = get(&router, "/api/v1/ui", Some(&cookie)).await;
    assert_eq!(status.status(), StatusCode::OK);
    let status: serde_json::Value =
        serde_json::from_str(&body_string(status).await).expect("UI status JSON");
    assert_eq!(status["availableCustom"]["usable"], false);
    assert_eq!(
        status["availableCustom"]["unavailableReason"],
        "digestMismatch"
    );

    let refused = json_request(
        &router,
        "PUT",
        "/api/v1/ui/active",
        json!({ "generation": 1 }),
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    let refused: serde_json::Value =
        serde_json::from_str(&body_string(refused).await).expect("API error JSON");
    assert_eq!(refused["error"]["code"], "custom_ui_unavailable");
    assert_eq!(
        get(&router, "/", None).await.status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn ui_selection_records_deactivation_activation_and_no_op() {
    let bundle = install_bundle(&[("index.html", "<!doctype html><title>custom</title>")]);
    let audit_dir = TempDir::new().unwrap();
    let fake = Arc::new(FakeSettings::new(configured_tree("hunter2secret")));
    let router = app(AppState::new(fake, SIGNING_KEY)
        .with_persistence(audit_dir.path())
        .with_bundle_root(bundle.path()));
    let login = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    let cookie = session_cookie_value(&login);
    let body: serde_json::Value =
        serde_json::from_str(&body_string(login).await).expect("session JSON");
    let csrf = body["csrfToken"].as_str().unwrap();

    let response = json_request(
        &router,
        "DELETE",
        "/api/v1/ui/active",
        serde_json::Value::Null,
        Some(&cookie),
        Some(csrf),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK, "DELETE");

    let first = json_request(
        &router,
        "PUT",
        "/api/v1/ui/active",
        json!({ "generation": 1 }),
        Some(&cookie),
        Some(csrf),
    );
    let second = json_request(
        &router,
        "PUT",
        "/api/v1/ui/active",
        json!({ "generation": 1 }),
        Some(&cookie),
        Some(csrf),
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.status(), StatusCode::OK, "first parallel PUT");
    assert_eq!(second.status(), StatusCode::OK, "second parallel PUT");

    let outcomes: Vec<String> = audit_lines(audit_dir.path())
        .into_iter()
        .filter(|line| line["event"] == "custom-ui")
        .map(|line| line["outcome"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(outcomes, ["deactivated", "activated", "no-op"]);
}

#[tokio::test]
async fn legacy_form_mutations_are_not_routes() {
    let empty = TempDir::new().unwrap();
    let fake = Arc::new(FakeSettings::new(configured_tree("hunter2secret")));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY).with_bundle_root(empty.path()));
    for path in [
        "/containers/enable",
        "/mqtt/enable",
        "/setup",
        "/ssh/enable",
        "/ssh/password",
        "/ssh/keys/add",
        "/ssh/keys/remove",
        "/network",
        "/network/peers/add",
        "/network/peers/remove",
        "/hostname",
        "/password",
        "/power/reboot",
        "/power/poweroff",
        "/login",
        "/logout",
        "/builtin/deactivate",
        "/builtin/tokens",
        "/builtin/tokens/revoke",
    ] {
        let response = post_form(&router, path, "enabled=on", None).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
    }
    assert!(fake.set_paths().is_empty());
}

// The root asset router without the structural `/api` declaration.
//
// The shared path layer still refuses a literal or encoded reserved first
// segment. This is intentional defence in depth for ambiguous spellings that
// Axum did not classify, not a route to API content.
fn asset_router_without_the_api_reservation(bundle_root: &Path) -> Router {
    let fake = Arc::new(FakeSettings::new(configured_tree("hunter2secret")));
    let state = AppState::new(fake, SIGNING_KEY).with_bundle_root(bundle_root);
    Router::new().fallback(serve::fallback).with_state(state)
}

async fn request(
    router: &Router,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    accept: Option<&str>,
) -> Response<axum::body::Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, format!("apid_session={cookie}"));
    }
    if let Some(accept) = accept {
        builder = builder.header(ACCEPT, accept);
    }
    send(router, builder.body(Body::empty()).unwrap()).await
}

fn header_value(response: &Response<axum::body::Body>, name: HeaderName) -> String {
    response
        .headers()
        .get(&name)
        .unwrap_or_else(|| panic!("response carries {name}"))
        .to_str()
        .unwrap()
        .to_string()
}

// §4.1 rule 1, stated the way §4.2 condition 1 needs it: a bundle that
// **actually contains** files under `api/` cannot serve one.
//
// The bundle really has them — `installed_files` lists them out of the
// installed tree — and the same router serves `/decoy.txt` from the same
// bundle, so the answers below are the reservation and not an empty
// directory. `api/versions` shadows a route that now exists, so its
// assertion is that the declared handler answered rather than that nothing
// did.
// `each_guard_is_exercised_by_exactly_one_hostile_feature` in `assets::path`
// is the discipline this follows: the assertion has to distinguish the guard
// from its absence.
#[tokio::test]
async fn a_bundle_cannot_shadow_the_reserved_api_subtree() {
    const VERSIONS_BYTES: &str = "BUNDLE-SHADOWS-API-VERSIONS";
    const SETTINGS_BYTES: &str = "BUNDLE-SHADOWS-API-V1-SETTINGS";

    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("decoy.txt", "the bundle is reachable"),
        ("api/versions", VERSIONS_BYTES),
        ("api/v1/settings", SETTINGS_BYTES),
    ]);

    // The files are in the installed tree, not merely in the staged one.
    let listed = installed_files(bundle.path());
    assert!(
        listed.contains(&"api/versions".to_string())
            && listed.contains(&"api/v1/settings".to_string()),
        "the installed bundle must actually contain the shadowing files: {listed:?}"
    );

    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    // The bundle is reachable through this very router, so a 404 under `/api/`
    // cannot be explained by the bundle not being served.
    let response = request(&router, "GET", "/decoy.txt", Some(&cookie), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "the bundle is reachable");

    // The declared route answers with its own document, not with the file the
    // bundle put in its way.
    let response = request(&router, "GET", "/api/versions", Some(&cookie), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        !body.contains(VERSIONS_BYTES),
        "/api/versions answered with the bundle's own bytes: {body}"
    );
    assert_eq!(body, VERSIONS_BODY);

    for (path, bytes) in [("/api/v1/settings", SETTINGS_BYTES)] {
        let response = request(&router, "GET", path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        let status = response.status();
        let content_type = header_value(&response, CONTENT_TYPE);
        let cache_control = header_value(&response, CACHE_CONTROL);
        let body = body_string(response).await;

        // Asserted first, and on the body rather than on the status, so that
        // deleting the reservation fails this test **with the bundle's own
        // bytes printed** rather than with a bare `200 != 404`. The guard is
        // then distinguishable from its absence by reading the failure.
        assert!(
            !body.contains(bytes),
            "{path}: the reserved subtree answered with the bundle's own bytes: {body}"
        );
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(content_type, "application/json", "{path}");
        assert_eq!(cache_control, "no-store", "{path}");
        let envelope: serde_json::Value = serde_json::from_str(&body).expect("§2.4 envelope");
        assert_eq!(envelope["error"]["code"], "not_found", "{path}");
        assert_eq!(envelope["error"]["source"], "apid", "{path}");
        assert!(
            envelope["error"]["message"].is_string(),
            "{path}: §2.4 requires a message"
        );
    }
}

// Reserved ownership is based on one canonical URL spelling. Repeated
// leading separators and percent-encoded reserved names must be rejected,
// never decoded by the root asset resolver into a second spelling of `/api`
// or `/_ui`. Prefix lookalikes and `/ui` remain ordinary custom-UI paths.
#[tokio::test]
async fn ambiguous_reserved_prefixes_cannot_cross_asset_roots() {
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("api/versions", "CUSTOM-API-ALIAS"),
        ("_ui/assets/app.js", "CUSTOM-UI-ALIAS"),
        ("ui/asset.js", "CUSTOM-UI"),
        ("apiary/asset.js", "CUSTOM-APIARY"),
        ("uikit/asset.js", "CUSTOM-UIKIT"),
    ]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    for path in [
        "//api/versions",
        "/%61pi/versions",
        "/%5fui/assets/app.js",
        "/_ui/%252e%252e",
        "/_ui/%2e%2e/api",
    ] {
        let response = request(&router, "GET", path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must terminate in its originally selected ownership domain"
        );
        let body = body_string(response).await;
        assert!(!body.contains("CUSTOM-"), "{path} crossed into custom UI");
        assert!(
            !body.contains("<title>mos console</title>"),
            "{path} used a guarded miss as built-in SPA navigation"
        );
    }

    for (path, expected) in [
        ("/apiary/asset.js", "CUSTOM-APIARY"),
        ("/ui/asset.js", "CUSTOM-UI"),
        ("/uikit/asset.js", "CUSTOM-UIKIT"),
    ] {
        let response = request(&router, "GET", path, Some(&cookie), None).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(body_string(response).await, expected, "{path}");
    }
}

// Even if a caller constructs the root asset router without the structural
// `/api` reservation, the shared logical-path guard will not expose a bundle's
// reserved first segment.
#[tokio::test]
async fn root_asset_resolver_fails_closed_without_the_api_router() {
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("api/versions", "BUNDLE-SHADOWS-API-VERSIONS"),
    ]);
    let unreserved = asset_router_without_the_api_reservation(bundle.path());

    let response = request(&unreserved, "GET", "/api/versions", None, None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_string(response).await, "");
}

// The reservation covers the subtree and every method, for every path the
// API does not declare. The declared paths are asserted separately, below.
#[tokio::test]
async fn the_api_reservation_answers_every_shape_with_the_envelope() {
    let bundle = install_bundle(&[("index.html", "<!doctype html><title>custom</title>")]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    for (method, path) in [
        ("GET", "/api"),
        ("GET", "/api/"),
        ("GET", "/api/v1"),
        ("GET", "/api/versions/extra"),
        ("GET", "/api/v1/settings"),
        // `/api/v1/actions/reboot` was here until it was declared, and
        // its prefix and trailing-slash spelling take its place for the reason
        // the WiFi collection's did: neither is a route this router serves, so
        // both must still reach the reservation rather than the three action
        // routes beside them.
        ("GET", "/api/v1/actions"),
        ("GET", "/api/v1/actions/"),
        // `/api/v1/wifi/client/networks` was here until it was declared. Its prefix and its trailing-slash spelling took its place, and
        // they are the more useful cases: neither is a route this router
        // serves, so both must still reach the reservation rather than the
        // collection beside them.
        ("GET", "/api/v1/wifi/client"),
        ("GET", "/api/v1/wifi/client/networks/"),
        ("POST", "/api/v1/settings"),
    ] {
        let response = request(&router, method, path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {path} must be the reserved subtree's own 404"
        );
        assert_eq!(
            header_value(&response, CONTENT_TYPE),
            "application/json",
            "{method} {path}"
        );
        let envelope: serde_json::Value =
            serde_json::from_str(&body_string(response).await).expect("§2.4 envelope");
        assert_eq!(envelope["error"]["code"], "not_found", "{method} {path}");
    }
}

// §2.1's two discovery endpoints, and what the rest of the reserved subtree
// still answers now that two of its paths are declared.

// The exact document §2.1's discovery table gives for the served set.
const VERSIONS_BODY: &str = r#"{"versions":["v1"],"current":"v1"}"#;

// The exact document §2.1's discovery table gives for `/api/v1/meta`.
//
// Built from `mosd_settings::SCHEMA_VERSION` rather than from a literal,
// which is the whole point of the field: a schema bump moves this expectation
// and the handler together, and a hand-copied number in either is what fails.
fn meta_body() -> String {
    format!(
        r#"{{"api":"v1","settingsSchemaVersion":{},"daemon":"apid"}}"#,
        mosd_settings::SCHEMA_VERSION
    )
}

// Both headers §4.3 asks of every `/api/` response, successes included.
fn assert_api_headers(response: &Response<axum::body::Body>, context: &str) {
    assert_eq!(
        header_value(response, CONTENT_TYPE),
        "application/json",
        "{context}"
    );
    assert_eq!(
        header_value(response, CACHE_CONTROL),
        "no-store",
        "{context}"
    );
}

// The `error` object of a §2.4 envelope.
async fn envelope(response: Response<axum::body::Body>) -> serde_json::Value {
    let body = body_string(response).await;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("§2.4 envelope, got: {body}"));
    parsed["error"].clone()
}

// §2.1: unauthenticated, and the answer is the table's document exactly.
#[tokio::test]
async fn api_versions_answers_the_served_set_without_a_session() {
    let (router, _) = test_app(configured_tree("hunter2secret"));

    let response = get(&router, "/api/versions", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/versions");
    assert_eq!(body_string(response).await, VERSIONS_BODY);
}

// The first of §2.1's two reasons the endpoint is unauthenticated: a
// factory-fresh device has no `access.webAdmin`, so the gate is in setup mode
// and sends everything else to `/setup`.

// The hand-off is above the gate's `GetSettings("access")` call, so the
// question "which versions does this device serve?" is still answerable when
// mosd is not answering.

// §2.1's second discovery endpoint, answered for a valid session.
#[tokio::test]
async fn api_v1_meta_answers_for_a_session() {
    let (tree, token) = with_token(configured_tree("hunter2secret"));
    let (router, _) = test_app(tree);

    let response = bearer(&router, "GET", "/api/v1/meta", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/meta");
    assert_eq!(body_string(response).await, meta_body());
}

// §3.1's trap, refused: a client that follows the gate's redirect lands on
// `GET /login`, which answers **200 with HTML**, so a script reads the whole
// exchange as success. The answer is §2.4's envelope with the status that
// matches it.
#[tokio::test]
async fn api_v1_meta_without_a_session_is_401_and_the_envelope() {
    let (router, _) = test_app(configured_tree("hunter2secret"));

    let response = get(&router, "/api/v1/meta", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_api_headers(&response, "/api/v1/meta");
    let error = envelope(response).await;
    assert_eq!(error["code"], "not_authenticated");
    assert_eq!(error["source"], "apid");
    assert!(error["message"].is_string(), "§2.4 requires a message");
    // §2.4 defines `path` as the settings dot-path at fault, and a request
    // that failed to authenticate names none.
    assert_eq!(error.get("path"), None);
}

// Setup mode is the branch a path-prefix implementation breaks: no session
// can exist there, and the gate sends everything it still owns to `/setup`.
#[tokio::test]
async fn api_v1_meta_is_401_in_setup_mode_too() {
    let (router, _) = test_app(unconfigured_tree());

    let response = get(&router, "/api/v1/meta", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_api_headers(&response, "/api/v1/meta");
    assert_eq!(envelope(response).await["code"], "not_authenticated");
}

// The declared paths are the only ones that changed. Every other path under
// `/api` has **one** answer — the subtree's own 404, byte for byte — with a
// session, without one, and in setup mode alike.
//
// The bare family prefixes are members of this class rather than exceptions
// to it. axum's `{*path}` wildcard matches at least one character, so
// `/api/v1/settings` and `/api/v1/settings/` name no dot-path and reach the
// not-found handler.
//
// Until the fix landed this test was
// `every_other_api_path_keeps_both_of_its_answers` and asserted **two**
// answers: this 404 with a session, and the gate's redirect to `/login` or
// `/setup` without one. The session arm is unchanged — the same handler
// answers it and the assertion below is the one it always carried — and the
// other two arms are what M2 closed. The reason the old comment gave for the
// redirect, that the gate's predicate has to agree with the router or an
// unauthenticated request would be handed to a route that does not exist,
// stopped applying with it: being handed to a route that does not exist is
// the intended outcome now, because the not-found handler is a route and it
// answers in §2.4's envelope.
#[tokio::test]
async fn every_other_api_path_has_one_answer_in_every_mode() {
    // `/api/v1/ssh/authorized-keys` left this list when the API declared
    // it: it is now a served collection, and the test that holds its answers
    // is `the_ssh_key_collection_lists_adds_and_removes`.
    //
    // `/api/v1/actions/reboot` left it the same way when the API declared
    // it. A `GET` on it is now a declared route answering 405 rather than an
    // undeclared path answering 404, which is the whole of what M7 changed
    // about it; the tests that hold its answers are
    // `a_wrong_method_on_a_declared_api_route_answers_the_envelope` for the
    // 405 and `the_power_routes_answer_202_like_the_form_path` for the POST.
    const UNDECLARED: [&str; 4] = [
        "/api/v1/settings",
        "/api/v1/settings/",
        "/api/v1/state",
        "/api/v1/state/",
    ];

    let (router, _) = test_app(configured_tree("hunter2secret"));
    let cookie = login(&router, "hunter2secret").await;
    let (fresh, _) = test_app(unconfigured_tree());

    for path in UNDECLARED {
        let expected = json!({
            "error": {
                "code": "not_found",
                "message": format!("no API route at {path}"),
                "source": "apid",
            }
        })
        .to_string();

        // With a session, without one, and on a device that has no admin
        // password at all: the reserved subtree's own envelope, byte for byte,
        // three times. The router is the same one in the first two cases and a
        // freshly built one in setup mode, which is the case a path-prefix
        // gate used to break.
        for (mode, response) in [
            ("with a session", get(&router, path, Some(&cookie)).await),
            ("without one", get(&router, path, None).await),
            ("in setup mode", get(&fresh, path, None).await),
        ] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path} {mode}");
            assert_api_headers(&response, path);
            assert_eq!(body_string(response).await, expected, "{path} {mode}");
        }
    }
}

// `mosd/apid/openapi.json` is the bytes `apid --openapi` prints.
//
// A local `cargo test` failure and not only a CI one: whoever changed a route
// is the person holding the command that regenerates the file.
#[test]
fn the_committed_openapi_document_is_the_generated_one() {
    assert_eq!(
        include_str!("../openapi.json"),
        crate::openapi::document_json()
    );
}

#[test]
fn the_openapi_document_covers_browser_ui_and_live_network_routes() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    let session = &document["paths"]["/api/v1/session"];
    for method in ["get", "post", "delete"] {
        assert!(
            session[method].is_object(),
            "missing session {method}: {session}"
        );
    }
    assert!(document["paths"]["/api/v1/ui"]["get"].is_object());
    assert!(document["paths"]["/api/v1/ui/active"]["put"].is_object());
    assert!(document["paths"]["/api/v1/ui/active"]["delete"].is_object());
    assert!(
        document["components"]["schemas"]["UiStatus"]["properties"]["availableCustom"].is_object()
    );
    assert_eq!(
        document["components"]["schemas"]["CustomUiUnavailableReason"]["enum"],
        json!([
            "missingActivationRecord",
            "unsafeTree",
            "indexUnavailable",
            "manifestInvalid",
            "digestMismatch",
            "incompatible",
        ])
    );

    let network = &document["paths"]["/api/v1/network"];
    assert!(
        network["get"].is_object(),
        "missing live network read: {network}"
    );
    assert!(
        network["put"].is_object(),
        "missing network write: {network}"
    );
    assert!(
        document["components"]["schemas"]["NetworkOverview"]["properties"]["observed"].is_object()
    );
    assert!(document["components"]["schemas"]["SetupToken"]["properties"]["csrfToken"].is_object());
}

// The document describes the served surface, §3.1's outcome included: a
// client that reads only `openapi.json` has to be able to learn that
// `/api/v1/meta` can answer 401.
#[test]
fn the_openapi_document_covers_the_declared_routes() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    assert!(
        document["paths"]["/api/versions"]["get"]["responses"]["200"].is_object(),
        "{document}"
    );
    let meta = &document["paths"]["/api/v1/meta"]["get"]["responses"];
    assert!(meta["200"].is_object(), "{meta}");
    assert!(meta["401"].is_object(), "{meta}");
}

// §4.1 rules 2 and 3: a declared route wins structurally, and the bundle
// files of the same name are never consulted.

// §4.2 condition 2. Anything that is not `GET` or `HEAD` and reaches the
// asset router is a client error, and it is never HTML.
#[tokio::test]
async fn a_write_method_reaching_the_asset_router_is_405_and_never_html() {
    let bundle = install_bundle(&[("index.html", "<!doctype html><title>custom</title>")]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let response = request(
            &router,
            method,
            "/settings/network",
            Some(&cookie),
            Some(BROWSER_ACCEPT),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} /settings/network"
        );
        assert_eq!(header_value(&response, ALLOW), "GET, HEAD", "{method}");
        assert!(response.headers().get(CONTENT_TYPE).is_none(), "{method}");
        assert_eq!(body_string(response).await, "", "{method}");
    }
}

// §4.2 condition 3. This is the condition that separates a navigation from a
// data call when both are `GET`, and it is the whole of §4.2's stated
// property: a request a developer expected to be JSON never comes back as
// HTML with a 200.
#[tokio::test]
async fn a_json_client_never_gets_the_spa_fallback() {
    let bundle = install_bundle(&[("index.html", "<!doctype html><title>custom</title>")]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    // Three shapes of data call, and none of them may come back as HTML: the
    // explicit one §4.2 names, the `*/*` a `fetch()` sends when it sets no
    // `Accept`, and no header at all.
    for accept in [Some("application/json"), Some("*/*"), None] {
        let response = request(&router, "GET", "/settings/network", Some(&cookie), accept).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{accept:?}");
        assert_eq!(body_string(response).await, "", "{accept:?}");
    }

    // The same path, asked for as a navigation.
    for accept in [BROWSER_ACCEPT, "text/html"] {
        let response = request(
            &router,
            "GET",
            "/settings/network",
            Some(&cookie),
            Some(accept),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{accept}");
        assert_eq!(
            body_string(response).await,
            "<!doctype html><title>custom</title>",
            "{accept}"
        );
    }
}

// §4.2 condition 4, implemented as the heuristic §4.2 names: a final segment
// with a `.` is a filename, and a miss on a filename is a 404 with an empty
// body even for a browser navigation.
#[tokio::test]
async fn a_dotted_final_segment_misses_with_an_empty_body() {
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("assets/app.a1b2c3.js", "//real"),
    ]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    let response = request(
        &router,
        "GET",
        "/assets/app.deadbeef.js",
        Some(&cookie),
        Some(BROWSER_ACCEPT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_string(response).await, "");

    // A path with no dot in its final segment is a client-side route.
    let response = request(
        &router,
        "GET",
        "/settings/network",
        Some(&cookie),
        Some(BROWSER_ACCEPT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // And the file that does exist is served, so the 404 above is a miss and
    // not the extension being refused.
    let response = request(
        &router,
        "GET",
        "/assets/app.a1b2c3.js",
        Some(&cookie),
        Some(BROWSER_ACCEPT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "//real");
}

// §4.2 condition 5, and §6.1 classes 1 and 2: with no readable index the
// answer is the **built-in UI**, not a 404 and not a 500.

// §4.1's `/` exception, both branches.

// §4.3 as applied: `nosniff` on every asset response, the content type from
// the allowlist, and the cache class per §4.3's table — including the
// manifest's opt-in immutable directory.
#[tokio::test]
async fn every_asset_response_carries_nosniff_and_its_cache_class() {
    let manifest = r#"{"name":"custom","version":"1.0",
        "immutableDir":"assets","apiVersions":["v1"]}"#;
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("mos-ui.json", manifest),
        ("assets/app.a1b2c3.js", "//real"),
        ("assets/logo.svg", "<svg/>"),
        ("robots.txt", "User-agent: *"),
        ("data.bin", "\u{0}\u{1}"),
    ]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    for (path, content_type, cache_control) in [
        ("/index.html", "text/html; charset=utf-8", "no-store"),
        (
            "/assets/app.a1b2c3.js",
            "text/javascript; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
        (
            "/assets/logo.svg",
            "image/svg+xml",
            "public, max-age=31536000, immutable",
        ),
        ("/robots.txt", "text/plain; charset=utf-8", "no-cache"),
        ("/data.bin", "application/octet-stream", "no-cache"),
    ] {
        let response = request(&router, "GET", path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            header_value(&response, CONTENT_TYPE),
            content_type,
            "{path}"
        );
        assert_eq!(
            header_value(&response, CACHE_CONTROL),
            cache_control,
            "{path}"
        );
        assert_eq!(
            header_value(&response, HeaderName::from_static("x-content-type-options")),
            "nosniff",
            "{path}"
        );
    }

    // The SPA fallback is an HTML document and is `no-store` with it — §4.3's
    // first row names it explicitly, because a cached index makes a new bundle
    // invisible however correctly its assets are named.
    let response = request(
        &router,
        "GET",
        "/settings/network",
        Some(&cookie),
        Some(BROWSER_ACCEPT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, CACHE_CONTROL), "no-store");
    assert_eq!(
        header_value(&response, HeaderName::from_static("x-content-type-options")),
        "nosniff"
    );

    // A refusal is an asset response too.
    let response = request(
        &router,
        "GET",
        "/assets/missing.js",
        Some(&cookie),
        Some(BROWSER_ACCEPT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        header_value(&response, HeaderName::from_static("x-content-type-options")),
        "nosniff"
    );
}

// §4.4's suite, at the router rather than at `assets::path`: a hostile request
// is a 404 and is never answered by §4.2's fallback, even though every one of
// these satisfies §4.2's own five conditions.
#[tokio::test]
async fn a_hostile_path_is_404_and_never_the_spa_fallback() {
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("etc/passwd", "decoy"),
    ]);
    let store = Store::new(bundle.path());
    std::os::unix::fs::symlink("/etc/passwd", store.bundle_dir(1).join("leak")).unwrap();

    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    for path in [
        "/../../etc/passwd",
        "/%2e%2e%2fetc%2fpasswd",
        "/%252e%252e%2fetc%2fpasswd",
        "/index%00",
        "/leak",
    ] {
        let response = request(&router, "GET", path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let body = body_string(response).await;
        assert_eq!(body, "", "{path} must have an empty body");
        assert!(!body.contains("root:"), "{path} read the real /etc/passwd");
    }
}

// `HEAD` is §4.2 condition 2's other admitted method, and it answers with the
// headers its `GET` would carry.
#[tokio::test]
async fn head_is_admitted_and_carries_the_same_headers_as_get() {
    let bundle = install_bundle(&[
        ("index.html", "<!doctype html><title>custom</title>"),
        ("assets/app.a1b2c3.js", "//real"),
    ]);
    let router = test_app_serving(configured_tree("hunter2secret"), bundle.path());
    let cookie = login(&router, "hunter2secret").await;

    let head = request(
        &router,
        "HEAD",
        "/assets/app.a1b2c3.js",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        header_value(&head, CONTENT_TYPE),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(header_value(&head, CACHE_CONTROL), "no-cache");
    assert_eq!(
        header_value(&head, HeaderName::from_static("x-content-type-options")),
        "nosniff"
    );
}

// Every line of the audit log under `dir`, parsed, oldest first.
fn audit_lines(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.join("audit.log"))
        .expect("the audit log exists")
        .lines()
        .map(|line| serde_json::from_str(line).expect("every audit line parses as JSON"))
        .collect()
}

// The `(event, outcome)` pairs of `lines`, for order-sensitive assertions.
fn audit_events(lines: &[serde_json::Value]) -> Vec<(String, String)> {
    lines
        .iter()
        .map(|line| {
            (
                line["event"].as_str().expect("event").to_string(),
                line["outcome"].as_str().expect("outcome").to_string(),
            )
        })
        .collect()
}

// The MQTT pane

// A settings tree with the mqtt subtree, authenticated as `ssh_tree`.
fn mqtt_tree(enabled: bool) -> serde_json::Value {
    let hash = auth::hash_password("hunter2secret").unwrap();
    json!({
        "hostname": "mos",
        "network": {},
        "access": { "webAdmin": { "password_hash": hash } },
        "mqtt": {
            "enabled": enabled,
            "listen": { "address": "127.0.0.1", "port": 1883 },
            "auth": { "enabled": false },
        },
    })
}

// The live-state subtree mosd's mqtt reconciler publishes, copied **verbatim**
// from the reconciler's own expectation of it.
//
// Source: `mosd/mosd/src/reconciler/mqtt.rs`, test
// `live_state_names_both_units_and_the_config_path` -- its assertions on
// `configPath`, `listen.address`, `listen.port`, `auth.enabled` and `units`,
// for `settings(true, "127.0.0.1", 1883, false)`. The exact key set is pinned
// separately there by `the_published_shape_is_the_contract_with_the_apid_pane`,
// which names this file as the consumer.
//
// Copy it; do not adjust it. A fixture written here to match what the pane
// reads is a test of the pane against itself: it cannot detect that it
// disagrees with the producer, so both crates stay green while the pane
// renders "unknown" for every value and the open-listener warning cannot fire
// at all. This is still a second copy in a second crate -- apid and mosd talk
// over a bus and share no type -- but a named source makes the copy
// auditable.
//
// One field is necessarily not verbatim: `configPath` is the reconciler's own
// `config_path`, which is a `tempfile` directory in that test, so the
// production default (`DEFAULT_CONFIG_PATH`, same file) stands in for it.
const MQTT_PUBLISHED_STATE: &str = r#"{
    "enabled": true,
    "listen": { "address": "127.0.0.1", "port": 1883 },
    "auth": { "enabled": false },
    "configPath": "/run/mos/mqtt-broker.toml",
    "units": [
        {
            "unit": "mos-mqtt-broker.service",
            "activeState": "active",
            "unitFileState": "enabled-runtime"
        },
        {
            "unit": "mos-mqttd.service",
            "activeState": "active",
            "unitFileState": "enabled-runtime"
        }
    ]
}"#;

// The published state with both units in the state a working switch produces.
//
// The *structure* always comes from [`MQTT_PUBLISHED_STATE`]; only values are
// substituted, so no test here can quietly reintroduce a shape the reconciler
// does not publish. Both units follow the switch, because one switch drives
// both halves.
fn mqtt_state(enabled: bool, address: &str, port: u64, auth_enabled: bool) -> serde_json::Value {
    let active_state = if enabled { "active" } else { "inactive" };
    mqtt_state_with_units(
        enabled,
        address,
        port,
        auth_enabled,
        active_state,
        active_state,
    )
}

// The same with both units' `activeState` chosen, each written into the entry
// that carries its own name.
//
// Selecting the entry rather than indexing it is the point: the pane does the
// same, so a fixture that reordered `units` would still describe the units it
// means to describe.
fn mqtt_state_with_units(
    enabled: bool,
    address: &str,
    port: u64,
    auth_enabled: bool,
    broker_state: &str,
    bridge_state: &str,
) -> serde_json::Value {
    let mut state: serde_json::Value =
        serde_json::from_str(MQTT_PUBLISHED_STATE).expect("the golden published state parses");
    state["enabled"] = json!(enabled);
    state["listen"]["address"] = json!(address);
    state["listen"]["port"] = json!(port);
    state["auth"]["enabled"] = json!(auth_enabled);
    set_unit_state(&mut state, "mos-mqtt-broker.service", broker_state);
    set_unit_state(&mut state, "mos-mqttd.service", bridge_state);
    state
}

// Write one `units` entry's `activeState`, found by its `unit` field.
fn set_unit_state(state: &mut serde_json::Value, unit: &str, active_state: &str) {
    let entry = state["units"]
        .as_array_mut()
        .expect("the golden `units` is an array")
        .iter_mut()
        .find(|entry| entry["unit"] == json!(unit))
        .unwrap_or_else(|| panic!("the golden state publishes no unit named {unit}"));
    entry["activeState"] = json!(active_state);
}

#[tokio::test]
async fn the_mqtt_open_listener_warning_tracks_the_configuration_not_the_page() {
    // Three configurations that must NOT warn, so the warning means something
    // when it does appear. A pane that warned on everything would train an
    // operator to ignore it.
    let quiet = [
        // Loopback with auth off: the default, and unreachable from off-host.
        (true, "127.0.0.1", false),
        // Loopback v6, same reasoning -- the broker treats both as loopback.
        (true, "::1", false),
        // Off-host WITH auth: a deliberate, defended configuration.
        (true, "0.0.0.0", true),
    ];
    for (enabled, address, auth_enabled) in quiet {
        let (router, fake) = test_app(mqtt_tree(enabled));
        fake.set_state_entry("mqtt", mqtt_state(enabled, address, 1883, auth_enabled));
        let cookie = login(&router, "hunter2secret").await;
        let body = body_string(get(&router, "/mqtt", Some(&cookie)).await).await;
        assert!(
            !body.contains("accepts unauthenticated connections"),
            "{address} with auth={auth_enabled} must not warn: {body}"
        );
    }

    // And with the switch off there is no listener to warn about: the broker
    // is not running, so an open bind in the last published state describes
    // something that has already stopped.
    let (router, fake) = test_app(mqtt_tree(false));
    fake.set_state_entry("mqtt", mqtt_state(false, "0.0.0.0", 1883, false));
    let cookie = login(&router, "hunter2secret").await;
    let body = body_string(get(&router, "/mqtt", Some(&cookie)).await).await;
    assert!(
        !body.contains("accepts unauthenticated connections"),
        "a stopped broker must not be reported as accepting connections: {body}"
    );
}

// Every `post(...)` route registered in `routes.rs` appears in
// [`ALL_MUTATIONS`], which is what the authentication tests iterate.
//
// This is a test about the test list. The auth coverage above enumerates
// paths by hand, so a new mutating route is authenticated by the middleware
// but never *asserted* to be -- and the day the middleware is refactored,
// nothing fails. Reading the router's own source closes that gap.

// §2.2's two read-only resource roots.

// A tree in the shape §2.2's inventory describes, carrying every one of the
// four redacted field names — at three depths and inside an array — so a walk
// over the responses below proves the denylist covers all of them.
//
// The admin hash is a real one so `login` works against this tree; every
// other secret is a marker string, which is what the "no plaintext survived"
// assertions look for.
fn secret_tree(password: &str) -> serde_json::Value {
    json!({
        "hostname": "mos",
        // The field the denylist's fail-closed entry exists for. No shipped
        // schema has it — `WireguardConfig` carries no private key and never
        // will — so the fixture plants the hypothetical the entry guards
        // against: a settings tree that somehow holds one must not serve it.
        "network": {
            "wg0": { "kind": "wireguard", "privateKey": "wg-plaintext-marker" },
        },
        "access": {
            "webAdmin": { "password_hash": auth::hash_password(password).unwrap() },
            "device": { "passwordHash": "device-plaintext-marker" },
            "ssh": {
                "enabled": true,
                "authorizedKeys": [
                    { "comment": "laptop", "hash": "keyhash-plaintext-marker" },
                ],
            },
            // The one settings field that really is named `hash`: a bearer
            // token digest, inside an array, under the subtree the auth gate
            // reads on every request.
            "apiTokens": [
                {
                    "id": "3f2a9c41",
                    "name": "ci-deploy",
                    "hash": "token-digest-plaintext-marker",
                    "created": 1_700_000_000,
                },
            ],
        },
        "wifi": {
            "ap": { "ssid": "mos-ap", "psk": "ap-plaintext-marker" },
            "client": {
                "networks": [
                    { "ssid": "home", "psk": "home-plaintext-marker" },
                    {
                        "ssid": "work",
                        "psk": "work-plaintext-marker",
                        "extra": { "hash": "deep-plaintext-marker" },
                    },
                ],
            },
        },
    })
}

// The live-state entry the state tests read, carrying all five names too:
// §2.2 states the redaction rule for the settings root, and this campaign
// extends it to the state root, so the state root is held to the same proof.
fn secret_state_entry() -> serde_json::Value {
    json!({
        "psk": "state-ap-plaintext-marker",
        "privateKey": "state-private-plaintext-marker",
        "peers": [
            { "ssid": "home", "psk": "state-peer-plaintext-marker" },
            { "id": "laptop", "hash": "state-hash-plaintext-marker" },
        ],
        "admin": {
            "passwordHash": "state-camel-plaintext-marker",
            "nested": { "password_hash": "state-snake-plaintext-marker" },
        },
    })
}

// Every marker string [`secret_tree`] and [`secret_state_entry`] plant.
const PLAINTEXT_MARKERS: [&str; 12] = [
    "token-digest-plaintext-marker",
    "wg-plaintext-marker",
    "state-private-plaintext-marker",
    "device-plaintext-marker",
    "keyhash-plaintext-marker",
    "ap-plaintext-marker",
    "home-plaintext-marker",
    "work-plaintext-marker",
    "deep-plaintext-marker",
    "state-ap-plaintext-marker",
    "state-peer-plaintext-marker",
    "state-hash-plaintext-marker",
];

// The field names §2.2's redaction rule names, `privateKey` included.
const SECRET_FIELD_NAMES: [&str; 5] =
    ["psk", "passwordHash", "password_hash", "hash", "privateKey"];

// The sentinel a redacted field carries.
const REDACTED: &str = "<redacted>";

// Collect every secret-bearing field in `value` — at any depth, inside arrays
// included — as `(name, value)` pairs.
//
// Written independently of the redactor under test: it walks the *response*,
// so a redactor that missed a branch is caught by the value it left behind
// rather than by agreeing with itself.
fn secret_fields(value: &serde_json::Value, found: &mut Vec<(String, serde_json::Value)>) {
    match value {
        serde_json::Value::Object(fields) => {
            for (name, child) in fields {
                if SECRET_FIELD_NAMES.contains(&name.as_str()) {
                    found.push((name.clone(), child.clone()));
                } else {
                    secret_fields(child, found);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                secret_fields(item, found);
            }
        }
        _ => {}
    }
}

// §2.2: the dot-path IS the resource identifier, so the body is exactly what
// `GetSettings("<dot-path>")` returns.
#[tokio::test]
async fn the_settings_root_answers_the_dot_paths_value_for_a_session() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, _) = test_app(tree);

    let response = bearer(&router, "GET", "/api/v1/settings/hostname", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/settings/hostname");
    assert_eq!(body_string(response).await, r#""mos""#);

    // A subtree, and a scalar reached through one: the passthrough has no
    // shape of its own to impose.
    let response = bearer(&router, "GET", "/api/v1/settings/access.ssh", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(value["enabled"], json!(true));

    let response = bearer(
        &router,
        "GET",
        "/api/v1/settings/access.ssh.enabled",
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "true");
}

// The second root, which is a different tree in mosd and so a different route
// here (§2.2): untyped, in memory, and written only from inside mosd.
#[tokio::test]
async fn the_state_root_answers_the_dot_paths_value_for_a_session() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_state_entry("hostname", json!({ "applied": "mos" }));

    let response = bearer(&router, "GET", "/api/v1/state/hostname", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/state/hostname");
    assert_eq!(body_string(response).await, r#"{"applied":"mos"}"#);

    let response = bearer(&router, "GET", "/api/v1/state/hostname.applied", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, r#""mos""#);
}

// The time-status surface (PLAN-044): authenticated, read-only, and mosd's
// classification passed through rather than re-derived here.
#[tokio::test]
async fn the_time_status_route_answers_mosds_classification_read_only() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_time_status(json!({
        "status": "offline-degraded",
        "synchronized": false,
    }));

    let response = bearer(&router, "GET", "/api/v1/time/status", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/time/status");
    let status = body_json(response).await;
    assert_eq!(status["status"], "offline-degraded");
    assert_eq!(status["synchronized"], json!(false));

    // Unauthenticated is 401 like every management read.
    let response = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/time/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(envelope(response).await["code"], "not_authenticated");

    // Read-only: there is no verb here that could pause synchronization, so a
    // write is §2.4's method_not_allowed envelope, not a 404.
    let response = bearer_json(&router, "PUT", "/api/v1/time/status", &token, "{}").await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(envelope(response).await["code"], "method_not_allowed");
}

// The storage surface (PLAN-049): authenticated, read-only, and mosd's
// observation passed through rather than re-derived here.
#[tokio::test]
async fn the_storage_status_route_answers_mosds_observation_read_only() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_storage_status(json!({
        "tiers": [{
            "name": "data",
            "role": "ext4",
            "present": true,
            "mount": "/srv",
            "readOnly": false,
            "space": { "totalBytes": 1000, "usedBytes": 850, "freeBytes": 100, "reservedBytes": 50, "usedPercent": 85 },
            "pressure": "warning",
            "updateWorkspace": { "reservedBytes": 268435456, "available": false },
            "check": { "unit": "systemd-fsck@dev-mmcblk0p11.service", "result": "success", "exitStatus": 1 },
        }],
        "media": [{ "name": "nvme0n1", "kind": "nvme", "health": { "supported": false, "reason": "no SMART reader" } }],
        "policy": { "warningPercent": 80, "criticalPercent": 90 },
        "lifecycle": { "backupRestore": "unsupported", "encryption": "unsupported" },
    }));

    let response = bearer(&router, "GET", "/api/v1/storage/status", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/storage/status");
    let status = body_json(response).await;
    assert_eq!(status["tiers"][0]["name"], "data");
    assert_eq!(status["tiers"][0]["pressure"], "warning");
    assert_eq!(status["tiers"][0]["space"]["reservedBytes"], 50);
    assert_eq!(status["tiers"][0]["updateWorkspace"]["available"], false);
    // An unsupported metric reaches the client as unsupported, not as an
    // omission that reads like health.
    assert_eq!(status["media"][0]["health"]["supported"], false);
    assert_eq!(status["lifecycle"]["encryption"], "unsupported");

    // Unauthenticated is 401 like every management read.
    let response = send(
        &router,
        Request::builder()
            .method("GET")
            .uri("/api/v1/storage/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(envelope(response).await["code"], "not_authenticated");

    // Read-only: no verb here could rewrite the layout, so a write is
    // §2.4's method_not_allowed envelope rather than a 404.
    for method in ["PUT", "POST", "DELETE", "PATCH"] {
        let response = bearer_json(&router, method, "/api/v1/storage/status", &token, "{}").await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the storage status"
        );
        assert_eq!(envelope(response).await["code"], "method_not_allowed");
    }
}

// RFCT-285's last acceptance bullet, as a gate rather than a promise: normal
// apid exposes NO generic format or repartition action.
//
// Two halves, because either alone is weak. The document scan proves no
// DECLARED route names one of these operations — and the document is asserted
// elsewhere to be exactly what the handlers generate, so it is the route
// table. The probe proves the paths a client would actually try are not
// served by some route the scan's vocabulary missed.
#[tokio::test]
async fn normal_apid_exposes_no_format_or_repartition_action() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");
    let paths = document["paths"].as_object().expect("paths is an object");

    // The search space is populated, and it contains the storage surface
    // this test is about: a scan over an empty or storage-less document
    // would pass forever while proving nothing.
    assert!(
        paths.len() > 20,
        "the document lists too few paths: {paths:?}"
    );
    assert!(
        paths.contains_key("/api/v1/storage/status"),
        "the storage surface is missing, so this scan is not scanning it: {paths:?}"
    );

    const FORBIDDEN: [&str; 12] = [
        "format",
        "repartition",
        "partition",
        "mkfs",
        "fdisk",
        "resize",
        "wipe",
        "erase",
        "lvm",
        "raid",
        // The status surface now describes mounts, so the API must not grow
        // a verb that moves one: PLAN-063's units own the mounts, not apid.
        "mount",
        "unmount",
    ];
    for path in paths.keys() {
        let lowered = path.to_ascii_lowercase();
        for word in FORBIDDEN {
            assert!(
                !lowered.contains(word),
                "the API declares `{path}`, which names the `{word}` operation this product does not have"
            );
        }
    }

    // And the paths a client would guess are not served at all. Every method,
    // because a route that answered a POST while refusing a GET would still
    // be a destructive surface.
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, _fake) = test_app(tree);
    const GUESSES: [&str; 8] = [
        "/api/v1/storage/format",
        "/api/v1/storage/repartition",
        "/api/v1/storage/partitions",
        "/api/v1/actions/format",
        "/api/v1/actions/factory-reset",
        "/api/v1/storage/wipe",
        "/api/v1/storage/mount",
        "/api/v1/storage/namespaces",
    ];
    for path in GUESSES {
        for method in ["GET", "POST", "PUT", "DELETE"] {
            let response = bearer_json(&router, method, path, &token, "{}").await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{method} {path} is served by something"
            );
            assert_eq!(
                envelope(response).await["code"],
                "not_found",
                "{method} {path}"
            );
        }
    }
}

// The two roots are separate: a settings dot-path is not a state dot-path,
// and the routes do not fall back to each other.
#[tokio::test]
async fn the_two_roots_do_not_answer_for_each_other() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_state_entry("hostname", json!({ "applied": "mos" }));

    // `hostname` exists in both, with different values.
    let settings = bearer(&router, "GET", "/api/v1/settings/hostname", &token).await;
    let state = bearer(&router, "GET", "/api/v1/state/hostname", &token).await;
    assert_ne!(
        body_string(settings).await,
        body_string(state).await,
        "one root answered for the other"
    );

    // `network` exists only in the settings tree, so the state root must fail
    // rather than serve the settings value.
    let response = bearer(&router, "GET", "/api/v1/state/network", &token).await;
    assert_ne!(response.status(), StatusCode::OK);
}

// The document describes the served surface: a client reading only
// `openapi.json` has to learn both families and every outcome they have.
#[test]
fn the_openapi_document_covers_the_resource_routes() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    for path in ["/api/v1/settings/{path}", "/api/v1/state/{path}"] {
        let responses = &document["paths"][path]["get"]["responses"];
        for status in ["200", "401", "422", "500", "503"] {
            assert!(
                responses[status].is_object(),
                "{path} is missing its {status}: {document}"
            );
        }
    }

    // The two fixed-path status routes name no dot-path, so they carry no
    // 422; every other outcome a client has to handle is still declared.
    for path in ["/api/v1/time/status", "/api/v1/storage/status"] {
        let responses = &document["paths"][path]["get"]["responses"];
        for status in ["200", "401", "500", "503", "504"] {
            assert!(
                responses[status].is_object(),
                "{path} is missing its {status}: {document}"
            );
        }
        assert!(
            document["paths"][path].get("put").is_none()
                && document["paths"][path].get("post").is_none()
                && document["paths"][path].get("delete").is_none(),
            "{path} declares a write verb: {document}"
        );
    }

    // §2.2's sentinel is a value a client can receive, so the schema of the
    // body has to say so; a client that has not been told treats
    // `"<redacted>"` as the credential.
    assert!(
        document["components"]["schemas"]["ResourceValue"]["description"]
            .as_str()
            .is_some_and(|text| text.contains(REDACTED)),
        "the resource body's schema does not describe the redaction sentinel: {document}"
    );
}

// §2.2's redaction rule, driven from the failing side: every field the
// denylist names, at every depth the tree puts one and inside the arrays the
// dot-path syntax cannot address, comes back as the sentinel.
//
// The rule is fail-open — a secret-bearing field under a name not on the list
// is served — so this test is the mitigation §2.2 asks for. It walks the
// response rather than checking known locations, so a field added to the
// fixture is covered without editing an assertion here.
#[tokio::test]
async fn every_redacted_field_name_comes_back_redacted_from_the_settings_root() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, _) = test_app(tree);

    // Three subtrees rather than one, because the whole-tree dot-path is `""`
    // and this route family takes a non-empty one. Between them they hold all
    // five names.
    let mut found = Vec::new();
    let mut bodies = String::new();
    for path in [
        "/api/v1/settings/access",
        "/api/v1/settings/wifi",
        "/api/v1/settings/network",
    ] {
        let response = bearer(&router, "GET", path, &token).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body = body_string(response).await;
        secret_fields(&serde_json::from_str(&body).unwrap(), &mut found);
        bodies.push_str(&body);
    }

    let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
    for name in SECRET_FIELD_NAMES {
        assert!(
            names.contains(&name),
            "the fixture no longer carries a `{name}` field, so this test does not cover it: {names:?}"
        );
    }
    for (name, value) in &found {
        assert_eq!(value, &json!(REDACTED), "`{name}` was served in the clear");
    }
    // The walk only sees fields it recognises. This sees the bytes.
    for marker in PLAINTEXT_MARKERS {
        assert!(
            !bodies.contains(marker),
            "`{marker}` reached the wire: {bodies}"
        );
    }
}

// A settings read of `access` never carries a token digest.
//
// The general rule is asserted above by walking every field name on the
// denylist. This one names the field that made the rule load-bearing rather
// than precautionary: `access.apiTokens[].hash` is the first field of the
// settings schema actually named `hash`, it holds a credential digest, and it
// sits in the subtree the auth gate reads on every single request -- so a
// regression here is a digest served to every authenticated caller and to
// every future bearer-token holder.
//
// The subtree and the entry and the field are all asserted, because the three
// break differently: a denylist entry removed, a walk that stops at an array,
// and a dot-path that names the field directly and so has no field name left
// to key on.
#[tokio::test]
async fn a_settings_read_of_access_never_carries_a_token_digest() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, _) = test_app(tree);

    // The subtree the gate reads.
    let response = bearer(&router, "GET", "/api/v1/settings/access", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        !body.contains("token-digest-plaintext-marker"),
        "a token digest reached the wire: {body}"
    );

    // The entry is still served -- the id, the name and the clock reading are
    // what `GET /api/v1/tokens` lists -- so this is redaction and not removal.
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let entry = &value["apiTokens"][0];
    assert_eq!(entry["id"], json!("3f2a9c41"));
    assert_eq!(entry["name"], json!("ci-deploy"));
    assert_eq!(entry["created"], json!(1_700_000_000));
    assert_eq!(entry["hash"], json!(REDACTED));

    // The array on its own, which is the walk's array branch with nothing
    // above it to have caught the field first.
    let response = bearer(&router, "GET", "/api/v1/settings/access.apiTokens", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        !body.contains("token-digest-plaintext-marker"),
        "the token array served the digest: {body}"
    );

    // There is no dot-path that reaches one entry: the syntax has no array
    // indexing, which is why the denylist is by field name and not by path.
    let response = bearer(
        &router,
        "GET",
        "/api/v1/settings/access.apiTokens.0.hash",
        &token,
    )
    .await;
    let status = response.status();
    assert_ne!(
        status,
        StatusCode::OK,
        "an indexed dot-path resolved: {}",
        body_string(response).await
    );
}

// The same rule on the state root. §2.2 states it for the settings root only;
// this campaign extends it, because a denylist that covers one root while the
// other serves the same field names verbatim is a hole with a tested-looking
// lid.
#[tokio::test]
async fn the_state_root_is_redacted_by_the_same_rule() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_state_entry("wifiAp", secret_state_entry());

    let response = bearer(&router, "GET", "/api/v1/state/wifiAp", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;

    let mut found = Vec::new();
    secret_fields(&serde_json::from_str(&body).unwrap(), &mut found);
    let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
    for name in SECRET_FIELD_NAMES {
        assert!(names.contains(&name), "not covered: {name} in {names:?}");
    }
    for (name, value) in &found {
        assert_eq!(value, &json!(REDACTED), "`{name}` was served in the clear");
    }
    for marker in PLAINTEXT_MARKERS {
        assert!(
            !body.contains(marker),
            "`{marker}` reached the wire: {body}"
        );
    }
}

// The structural walk keys on a field name, and a dot-path that names a
// secret field directly leaves no field name in the value: the response is
// the bare hash. So the requested path is redacted as well as the tree.
#[tokio::test]
async fn a_dot_path_that_names_a_secret_field_answers_the_sentinel() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_state_entry("wifiAp", secret_state_entry());

    for path in [
        "/api/v1/settings/access.webAdmin.password_hash",
        "/api/v1/settings/access.device.passwordHash",
        "/api/v1/settings/wifi.ap.psk",
        "/api/v1/settings/network.wg0.privateKey",
        "/api/v1/state/wifiAp.psk",
        "/api/v1/state/wifiAp.privateKey",
        "/api/v1/state/wifiAp.admin.passwordHash",
    ] {
        let response = bearer(&router, "GET", path, &token).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            body_string(response).await,
            format!(r#""{REDACTED}""#),
            "{path}"
        );
    }
}

// A `zbus::Error::MethodError` naming `name`, with `message` as the body mosd
// sent back.
//
// Constructed rather than provoked: `FakeSettings` returns plain `anyhow`
// errors, which are §2.4's `mosd_unreachable` fallback row and cannot reach
// the other three.
fn method_error(name: &'static str, message: &str) -> zbus::Error {
    let reply_to = zbus::message::Message::method_call("/com/mos/mosd", "GetSettings")
        .expect("a well-formed method call")
        .build(&())
        .expect("an empty body serialises");
    let name = zbus::names::ErrorName::try_from(name).expect("a well-formed fdo error name");
    zbus::Error::MethodError(name.into(), Some(message.to_string()), reply_to)
}

// A [`SettingsApi`] whose resource reads fail with the error the test chose.
//
// `access` and the whole tree still read, because that is what the gate and
// `login_submit` need to get a session as far as a route that fails.
struct FailingSettings {
    tree: serde_json::Value,
    /// The fdo error name mosd answered with, or `None` for a failure that
    /// never reached mosd at all.
    fdo_name: Option<&'static str>,
}

impl FailingSettings {
    fn error(&self) -> anyhow::Error {
        match self.fdo_name {
            Some(name) => method_error(name, MOSD_MESSAGE).into(),
            None => anyhow::anyhow!("no connection to mosd"),
        }
    }
}

// The text mosd is pretending to have sent, which §2.4 requires apid to carry
// through untouched.
const MOSD_MESSAGE: &str = "invalid settings value at `network.eth0.100`: unknown field `100`";

#[async_trait::async_trait]
impl SettingsApi for FailingSettings {
    async fn get_settings(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        if path.is_empty() || path == "access" {
            return Ok(if path.is_empty() {
                self.tree.clone()
            } else {
                self.tree["access"].clone()
            });
        }
        Err(self.error())
    }

    /// The settings root stopped being read-only, and this
    /// fixture answers the write the same way it answers a read: §2.4's
    /// classification is exactly what the write route has to inherit.
    async fn set_settings(
        &self,
        _path: &str,
        _value: &serde_json::Value,
    ) -> anyhow::Result<String> {
        Err(self.error())
    }

    async fn get_state(&self, _path: &str) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn get_time_status(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn get_storage_status(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn get_system_info(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn get_telemetry(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn get_observed_network(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn get_failure_evidence(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    /// The API gave these three a route each, so they answer the failure
    /// rather than panicking. `reboot` and `power_off` are called from a
    /// detached task whose result only reaches a log, so what they return
    /// changes no response; an `unreachable!` in them would abort that task
    /// instead, which is a panic in a fixture rather than a failed assertion in
    /// a test.
    async fn reboot(&self) -> anyhow::Result<()> {
        Err(self.error())
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        Err(self.error())
    }

    async fn set_transient_root_password(&self, _password: &str) -> anyhow::Result<String> {
        Err(self.error())
    }

    /// The one write this fixture *does* answer, because §2.4's classification
    /// is exactly what the rotate route has to inherit from the read routes.
    async fn rotate_wireguard_key(&self, _iface: &str) -> anyhow::Result<String> {
        Err(self.error())
    }

    // The update cluster inherits the same classification through
    // `update_bus_error`, so this fixture answers all six the same way.
    async fn get_update_state(&self) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn check_update(&self) -> anyhow::Result<()> {
        Err(self.error())
    }

    async fn fetch_update(&self) -> anyhow::Result<()> {
        Err(self.error())
    }

    async fn install_update(&self, _bundle: &str) -> anyhow::Result<()> {
        Err(self.error())
    }

    async fn mark_update(&self, _state: &str, _slot: &str) -> anyhow::Result<(String, String)> {
        Err(self.error())
    }

    async fn set_reboot_override(&self, _seconds: u32) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }

    async fn clear_update_suppression(&self, _version: &str) -> anyhow::Result<serde_json::Value> {
        Err(self.error())
    }
}

// A router whose resource reads fail the way `fdo_name` says, plus a session
// cookie for it.
async fn failing_app(fdo_name: Option<&'static str>) -> (Router, String) {
    // Seed the automation credential directly because this fixture refuses
    // the settings write a token mint would require.
    let (entry, wire) = seeded_token(0);
    let mut tree = configured_tree("hunter2secret");
    tree["access"]["apiTokens"] = json!([entry]);
    let api = Arc::new(FailingSettings { tree, fdo_name });
    let router = app(AppState::new(api, SIGNING_KEY));
    (router, wire)
}

// The premise the classification rests on: `err.into()` in `bus_client.rs`
// converts a `zbus::Error` to `anyhow::Error` through the blanket `From`,
// which STORES the concrete error rather than flattening it, so the fdo name
// is still there to be recovered. If this ever stops holding, every row of
// §2.4's table below collapses into the fallback and the tests would say so
// one at a time; this says it once, in the one sentence it depends on.
#[test]
fn the_zbus_error_survives_the_conversion_to_anyhow() {
    let err: anyhow::Error = method_error("org.freedesktop.DBus.Error.InvalidArgs", "boom").into();
    let recovered = err
        .downcast_ref::<zbus::Error>()
        .expect("the conversion kept the zbus error");
    match recovered {
        zbus::Error::MethodError(name, message, _) => {
            assert_eq!(name.as_str(), "org.freedesktop.DBus.Error.InvalidArgs");
            assert_eq!(message.as_deref(), Some("boom"));
        }
        other => panic!("the variant changed: {other:?}"),
    }
}

// §2.4's table, row by row: mosd classifies, apid translates the
// classification, and mosd's message is carried through verbatim.
//
// No row is route-dependent any more. fdo `InvalidArgs` was 422
// `settings_rejected` everywhere except the live-state route, where apid
// rewrote it to 404 because `GetState` had a single producer for the name;
// `GetState` now raises `MOSD_NOT_FOUND` for a dot-path that resolves to
// nothing, so the same name means the same thing on both routes and the
// table is read straight.
#[tokio::test]
async fn each_fdo_error_name_gets_its_own_envelope() {
    for (fdo_name, code, status) in [
        (
            "com.mos.mosd1.Error.NotFound",
            "settings_not_found",
            StatusCode::NOT_FOUND,
        ),
        (
            "com.mos.mosd1.Error.ReadOnly",
            "settings_read_only",
            StatusCode::CONFLICT,
        ),
        (
            "org.freedesktop.DBus.Error.InvalidArgs",
            "settings_rejected",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "org.freedesktop.DBus.Error.IOError",
            "settings_io",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            "org.freedesktop.DBus.Error.Failed",
            "mosd_failed",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        for path in ["/api/v1/settings/wifi.ap", "/api/v1/state/wifiAp"] {
            let (router, token) = failing_app(Some(fdo_name)).await;
            let response = bearer(&router, "GET", path, &token).await;
            assert_eq!(response.status(), status, "{fdo_name} at {path}");
            assert_api_headers(&response, path);
            assert_eq!(
                response.headers().get(axum::http::header::RETRY_AFTER),
                None,
                "only the unreachable class carries Retry-After: {fdo_name}"
            );
            let error = envelope(response).await;
            assert_eq!(error["code"], code, "{fdo_name}");
            assert_eq!(error["source"], "mosd", "{fdo_name}");
            // §2.4: apid substituting its own phrasing would hide every
            // message mosd learns to produce.
            assert_eq!(error["message"], MOSD_MESSAGE, "{fdo_name}");
            // §2.4's optional member, which these routes DO name.
            assert_eq!(
                error["path"],
                json!(path.rsplit('/').next().unwrap()),
                "{fdo_name}"
            );
        }
    }
}

// The fallback row, and the only one whose `source` is apid: the call could
// not be made at all, which is a statement about this server rather than
// about the request. 503, because apid itself is up and answering.
#[tokio::test]
async fn an_unreachable_mosd_is_503_with_retry_after() {
    // No `MethodError` at all, and a `MethodError` under a name §2.4's table
    // does not list: both are the fallback.
    for fdo_name in [None, Some("org.freedesktop.DBus.Error.UnknownObject")] {
        for path in ["/api/v1/settings/wifi.ap", "/api/v1/state/wifiAp"] {
            let (router, token) = failing_app(fdo_name).await;
            let response = bearer(&router, "GET", path, &token).await;
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{fdo_name:?} at {path}"
            );
            assert_api_headers(&response, path);
            assert_eq!(
                header_value(&response, axum::http::header::RETRY_AFTER),
                "5",
                "{fdo_name:?} at {path}"
            );
            let error = envelope(response).await;
            assert_eq!(error["code"], "mosd_unreachable");
            assert_eq!(error["source"], "apid");
            assert!(error["message"].is_string());
        }
    }
}

// The HTML half of the same condition: a pane whose mosd call fails answers
// **503 with `Retry-After`**, exactly like the API path above, so one outage
// no longer reports as 502 on one surface and 503 on the other.

// A dot-path that does not exist answers **404 `settings_not_found`**, no
// longer 422: mosd names `SettingsError::NotFound` with its own error name
// (`com.mos.mosd1.Error.NotFound`), so a missing path and a bad value stop
// sharing a code. The 422 assertion beside it is the control: a rejection
// that IS a rejection still reports as one.
#[tokio::test]
async fn a_dot_path_that_does_not_exist_is_404_and_a_rejection_stays_422() {
    let (router, token) = failing_app(Some("com.mos.mosd1.Error.NotFound")).await;
    let response = bearer(&router, "GET", "/api/v1/settings/no.such.path", &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["path"], json!("no.such.path"));

    let (router, token) = failing_app(Some("org.freedesktop.DBus.Error.InvalidArgs")).await;
    let response = bearer(&router, "GET", "/api/v1/settings/no.such.path", &token).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_rejected");
    assert_eq!(error["path"], json!("no.such.path"));
}

// A live-state dot-path that does not resolve answers **404
// `settings_not_found`**, the same code the settings tree gives the same
// condition -- not the 422 §2.4's table gives fdo `InvalidArgs`.
//
// **The status, the code and the envelope are unchanged; what carries them
// is not.** mosd used to raise fdo `InvalidArgs` for a state path that does
// not resolve, and apid answered 404 by reading that name against a fact
// about `GetState` -- that it had exactly one producer for it -- which was
// true but private to this one route. mosd's `get_state` now raises
// `MOSD_NOT_FOUND`, the name every other read on the bus already uses for a
// path that names nothing, so §2.4's shared classifier answers this without a
// special case.
//
// The second half is the control, and it is the assertion that would have
// failed before: fdo `InvalidArgs` on the state route is now a plain 422,
// because no rewrite is left to intercept it.
#[tokio::test]
async fn a_state_dot_path_that_does_not_resolve_is_404_not_422() {
    let (router, token) = failing_app(Some("com.mos.mosd1.Error.NotFound")).await;

    let response = bearer(&router, "GET", "/api/v1/state/no.such.path", &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["path"], json!("no.such.path"));

    let (router, token) = failing_app(Some("org.freedesktop.DBus.Error.InvalidArgs")).await;
    let response = bearer(&router, "GET", "/api/v1/state/no.such.path", &token).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_rejected");

    let response = bearer(&router, "GET", "/api/v1/settings/no.such.path", &token).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_rejected");
}

// §3.1's trap again, for the routes this campaign adds: an unauthenticated
// resource read answers §2.4's envelope with a 401 and **never** a redirect,
// in both gate modes. They inherit it from `ApiSession`; inheriting is not
// the same as being asserted.
#[tokio::test]
async fn the_resource_routes_are_401_without_a_session_in_both_gate_modes() {
    const PATHS: [&str; 2] = ["/api/v1/settings/hostname", "/api/v1/state/hostname"];

    let (configured, _) = test_app(secret_tree("hunter2secret"));
    let (fresh, _) = test_app(unconfigured_tree());

    for (mode, router) in [("configured", &configured), ("setup mode", &fresh)] {
        for path in PATHS {
            let response = get(router, path, None).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{path} in {mode}"
            );
            assert_eq!(
                response.headers().get(LOCATION),
                None,
                "{path} in {mode} answered a redirect, which a script reads as success"
            );
            assert_api_headers(&response, path);
            let error = envelope(response).await;
            assert_eq!(error["code"], "not_authenticated", "{path} in {mode}");
            assert_eq!(error["source"], "apid", "{path} in {mode}");
            // §2.4's `path` is the dot-path at fault, and a request that failed
            // to authenticate never named one: the read did not happen.
            assert_eq!(error.get("path"), None, "{path} in {mode}");
        }
    }
}

// The three spellings of each resource root are one string plus two suffixes.
//
// The router, the OpenAPI attribute and the gate predicate each need a
// different one, and a typo in any of them would serve a path the document
// does not describe or hand off a path the router does not have.
#[test]
fn the_resource_path_spellings_agree() {
    for (prefix, route, doc) in [
        crate::routes::SETTINGS_SPELLINGS,
        crate::routes::STATE_SPELLINGS,
    ] {
        assert_eq!(route, format!("{prefix}{{*path}}"));
        assert_eq!(doc, format!("{prefix}{{path}}"));
    }
}

// The admin password can be changed after setup, on both surfaces.

// POST a JSON body, the shape the API's one write route takes.
async fn post_json(
    router: &Router,
    path: &str,
    body: &str,
    cookie: Option<&str>,
) -> Response<axum::body::Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(COOKIE, format!("apid_session={cookie}"));
    }
    send(router, builder.body(Body::from(body.to_string())).unwrap()).await
}

// A wrong current password writes nothing and the old credential stands.
//
// The current password is demanded even though the caller holds a session: a
// session is a browser artifact that outlives the moment of typing, and an
// unattended browser must not be enough to rotate the one credential on the
// management surface.

// The decided semantics, end to end: the new hash lands in the settings
// tree, every other session is invalidated, and the acting session survives.

// A mismatched confirmation is refused before the current password is even
// looked at, in the same shape as the setup wizard's refusal.

// The API half of the same refusal: §2.4's envelope, `wrong_password`, and
// nothing written.
#[tokio::test]
async fn the_api_password_change_rejects_a_wrong_current_password() {
    let (tree, token) = with_token(configured_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/actions/change-password",
        &token,
        r#"{"currentPassword":"not-the-password","newPassword":"newsecret9"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_api_headers(&response, "/api/v1/actions/change-password");
    let error = envelope(response).await;
    assert_eq!(error["code"], "wrong_password");
    assert_eq!(error["source"], "apid");
    assert!(fake.set_paths().is_empty());
}

// The API half of the success: 204, the hash written, and **every** browser
// session dropped.
//
// "the calling session kept" until M9, and it cannot be kept now:
// the caller authenticates with a bearer, so there is no calling session to
// name. The name of this test moved with the assertion.

// A new password under eight characters is refused with `validation_failed`,
// the same floor the setup wizard enforces.
#[tokio::test]
async fn the_api_password_change_rejects_a_short_new_password() {
    let (tree, token) = with_token(configured_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/actions/change-password",
        &token,
        r#"{"currentPassword":"hunter2secret","newPassword":"short"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert!(fake.set_paths().is_empty());
}

// §3.1's trap, held for the one write route: unauthenticated is §2.4's 401
// envelope in both gate modes, never a redirect a script reads as success.
#[tokio::test]
async fn the_api_password_change_is_401_without_a_session_in_both_gate_modes() {
    let (configured, _) = test_app(configured_tree("hunter2secret"));
    let (fresh, _) = test_app(unconfigured_tree());

    for (mode, router) in [("configured", &configured), ("setup mode", &fresh)] {
        let response = post_json(
            router,
            "/api/v1/actions/change-password",
            r#"{"currentPassword":"hunter2secret","newPassword":"newsecret9"}"#,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{mode}");
        assert_eq!(response.headers().get(LOCATION), None, "{mode}");
        let error = envelope(response).await;
        assert_eq!(error["code"], "not_authenticated", "{mode}");
    }
}

// A body that is not the declared shape answers §2.4's envelope rather than
// axum's plain-text rejection.
#[tokio::test]
async fn the_api_password_change_rejects_a_malformed_body_with_the_envelope() {
    let (tree, token) = with_token(configured_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/actions/change-password",
        &token,
        r#"{"currentPassword":"hunter2secret"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_api_headers(&response, "/api/v1/actions/change-password");
    let error = envelope(response).await;
    assert_eq!(error["code"], "request_invalid");
    assert_eq!(error["source"], "apid");
    assert!(fake.set_paths().is_empty());
}

// The gate's cache of the `access` subtree.
//
// The subscription itself — the proxy's `#[zbus(signal)]` member feeding the
// cache over a real bus — is exercised in `tests/settings_signal.rs`. Here
// the cache's route-level contract is driven through the real router, with
// the subscription state set by hand where a watcher would set it.

// The lockout rule at the route level: with no live subscription every
// unauthenticated request reads the bus — the pre-cache behaviour, and the
// fallback the rule demands; with one, the first request fills the cache and
// the rest are served from it; an invalidation forces exactly one re-read;
// a lapse falls all the way back to direct reads.

// The password change against the cache — the sequence named as
// the hard case, made real by the change-password route: the flow itself
// verifies against the bus even while the cache is primed, its write drops
// the cached snapshot without waiting for the `SettingsChanged` round trip,
// and the next unauthenticated request re-reads and observes the
// post-change tree.

// Completing setup IS the setup-mode decision changing under the gate — the
// exact decision the cache must never serve stale. The wizard's
// `access.webAdmin` write drops the cache, so the next unauthenticated
// request re-reads and redirects to `/login`, not back into `/setup`.

// The typed network pane, the rotate-key route, and the two
// live-state fields M5 added.

// A settings tree with `network` entries of every kind, in the shape schema
// v7 stores them.
//
// `eth1` carries no addressing at all, which is what a bridge port is; `wg0`
// carries a peer whose key is a real 32-byte base64 value, so a rejection in
// these tests is a verdict on the code under test and not on a malformed
// fixture.
fn kinds_tree(password: &str) -> serde_json::Value {
    json!({
        "hostname": "mos",
        "network": {
            "eth0": { "dhcp": true },
            "eth1": { "dhcp": false },
            "eth0.100": {
                "kind": "vlan",
                "dhcp": false,
                "static": { "address": "192.168.100.2/24", "dns": [] },
                "vlan": { "parent": "eth0", "id": 100 },
            },
            "br0": { "kind": "bridge", "dhcp": true, "bridge": { "ports": ["eth1"] } },
            "wg0": {
                "kind": "wireguard",
                "dhcp": false,
                "static": { "address": "10.8.0.2/24", "dns": [] },
                "wireguard": {
                    "listenPort": 51820,
                    "peers": [{
                        "publicKey": PEER_KEY,
                        "allowedIps": ["10.8.0.0/24"],
                        "endpoint": "vpn.example.net:51820",
                    }],
                },
            },
        },
        "access": { "webAdmin": { "password_hash": auth::hash_password(password).unwrap() } },
    })
}

// A syntactically valid X25519 public key: 32 bytes in padded base64.
//
// Its private half was never generated — this is 32 constant bytes — so it
// authorises nothing anywhere. It exists so that a rejection in these tests is
// a verdict on the rule under test rather than on the shape of the value.
const PEER_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

// A second one, distinct from [`PEER_KEY`], for the add/remove tests.
const OTHER_PEER_KEY: &str = "AgICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgI=";

// The live-state object mosd's network reconciler publishes for `kinds_tree`,
// including the two fields M5 added: `kind` on every entry and `publicKey` on
// the tunnel.
//
// There is no private key in it because mosd never puts one there — the state
// tree is served over D-Bus and over `GET /api/v1/state/network`.
fn network_state() -> serde_json::Value {
    json!({
        "eth0": { "file": "50-mos-eth0.network", "dhcp": true, "kind": "physical" },
        "eth1": { "file": "50-mos-eth1.network", "dhcp": false, "kind": "physical" },
        "eth0.100": { "file": "50-mos-eth0.100.network", "dhcp": false, "kind": "vlan" },
        "br0": { "file": "50-mos-br0.network", "dhcp": true, "kind": "bridge" },
        "wg0": {
            "file": "50-mos-wg0.network",
            "dhcp": false,
            "kind": "wireguard",
            "publicKey": PEER_KEY,
        },
    })
}

// A router over [`kinds_tree`] with [`network_state`] published, plus a
// session cookie for it.
async fn kinds_app() -> (Router, Arc<FakeSettings>, String, String) {
    // Both credentials, since M9 split them: the panes in this
    // cluster take the cookie and the `/api/v1/network` routes beside them take
    // the bearer, and several tests assert the two surfaces agree. The token is
    // seeded into the tree rather than minted through the pane because most of
    // those tests assert `set_paths()` exactly, and a mint is a write.
    let (tree, token) = with_token(kinds_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_state_entry("network", network_state());
    let cookie = login(&router, "hunter2secret").await;
    (router, fake, cookie, token)
}

// The pane renders one typed form per kind, filled in from the stored entry.

// The live-state reader: `kind` for every entry and `publicKey` for the
// tunnel, which are the two fields M5 added to the per-interface object.

// mosd having published no state yet is a fact the pane states, not a 502:
// the stored configuration is still worth showing.

// One entry whose body this pane cannot read must not blank out the others.

// The three virtual kinds, written through the quoted-path writer as the
// typed bodies mosd deserializes.

// Saving a tunnel from the form keeps the peers the form does not carry.
//
// The failure this pins is silent: changing a listen port would otherwise
// disconnect every far end, and the pane would report "Settings saved."

// A value left in another kind's box is never written: the form renders all
// four groups at once, and only the group the submitted kind names is read.

// An interface with DHCP off and no address is an interface with no
// addressing, which is exactly what a bridge port is.
//
// It used to be a 422. It cannot stay one: a bridge port must carry neither
// `dhcp` nor `static`, and it must already be a declared entry before a bridge
// may name it, so refusing this body made a bridge unbuildable through the
// pane.

// The reconciler's cross-field rules, echoed by the pane for a readable error.
//
// Each row is a rule `validate_network` enforces in mosd. The pane is not the
// boundary — the settings file is writable without apid — so this asserts the
// echo, and that nothing was written when it fired.

// Peers are added and removed at the peer list's own dot-path, quoted when
// the interface name carries a dot.

// A dotted tunnel name reaches the peer list as one quoted segment.

// the sweep, settled by running it: the
// pane's peer-add for an interface that is **not a declared network entry**
// neither refuses nor 404s -- it succeeds, and writes a `network.wg9` entry
// of the default kind carrying a WireGuard block.
//
// That finding was recorded there explicitly as a reading of the write path
// and *not* as an observed run, and this is the run. The chain it names:
// `stored_peers` answers an empty list rather than an error for an unknown
// interface, `write_peers` writes straight to the peer list's own dot-path,
// `validate_peers` never looks at the interface, and the settings setter
// creates missing intermediates by documented contract.
//
// The pane is left as it is -- M6 fixes this structurally on the API side,
// where `POST /api/v1/network/{iface}/peers` answers 404 before anything is
// written. Its paired test is
// `the_api_peer_add_refuses_an_undeclared_interface_where_the_pane_writes_one`.

// A peer the reconciler would refuse is refused here first, and the refusal
// never echoes the key.
//
// The reconciler names a bad peer by its index for a reason it states: an
// operator who pasted a *private* key into the field would otherwise find it
// in the error text. The echo keeps that property.

// Removing a peer nobody has is an error rather than a silent no-op rewrite.

// Adding a peer twice is refused: two `[WireGuardPeer]` sections with one
// public key is a tunnel whose far end is described twice.

// The rotate-key route.

// The route in the three spellings that have to agree: the constant the
// router registers, what a caller sends, and what the document describes.
const ROTATE_PATH: &str = "/api/v1/actions/wireguard/wg0/rotate-key";

// §2.1's action route: mosd draws the key, and the body carries its public
// half and nothing else.
#[tokio::test]
async fn the_rotate_route_answers_the_new_public_key() {
    let (router, fake, _cookie, token) = kinds_app().await;

    let response = bearer_form(&router, ROTATE_PATH, &token, "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, ROTATE_PATH);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();

    assert_eq!(
        fake.rotations(),
        vec![(
            "wg0".to_string(),
            body["publicKey"].as_str().unwrap().to_string()
        )]
    );
    // The whole body, by identity: a member added here would be a member
    // shipped to every client, and the one member that must never appear is a
    // private key.
    assert_eq!(
        body.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["publicKey"],
        "{body}"
    );
}

// It rotates and it does not write: the settings tree holds no key, so there
// is nothing there for a rotation to change.
#[tokio::test]
async fn a_rotation_writes_nothing_to_the_settings_tree() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = fake.get_settings("network.wg0").await.unwrap();

    let response = bearer_form(&router, ROTATE_PATH, &token, "").await;
    assert_eq!(response.status(), StatusCode::OK);

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(fake.get_settings("network.wg0").await.unwrap(), before);
}

// §2.4's classification, inherited whole by the action route: mosd's fdo error
// name decides the status and the code, and the envelope names the settings
// dot-path at fault.
#[tokio::test]
async fn the_rotate_routes_failures_take_the_shared_envelope() {
    for (fdo_name, code, status) in [
        // the correction, on the apid side: **no apid logic
        // changed**. mosd split its one `InvalidArgs` into a not-found for an
        // undeclared entry and an `InvalidArgs` for one of the wrong kind, and
        // the classifier below already mapped both names. This row is the
        // proof that it did.
        (
            Some("com.mos.mosd1.Error.NotFound"),
            "settings_not_found",
            StatusCode::NOT_FOUND,
        ),
        (
            Some("org.freedesktop.DBus.Error.InvalidArgs"),
            "settings_rejected",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            Some("org.freedesktop.DBus.Error.IOError"),
            "settings_io",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (
            Some("org.freedesktop.DBus.Error.Failed"),
            "mosd_failed",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (None, "mosd_unreachable", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let (router, token) = failing_app(fdo_name).await;
        let response = bearer_form(&router, ROTATE_PATH, &token, "").await;
        assert_eq!(response.status(), status, "{fdo_name:?}");
        assert_api_headers(&response, ROTATE_PATH);
        let error = envelope(response).await;
        assert_eq!(error["code"], code, "{fdo_name:?}");
        // The dot-path at fault is the entry whose kind mosd refused, not the
        // HTTP path: §2.4's member is a settings dot-path.
        assert_eq!(error["path"], json!("network.wg0"), "{fdo_name:?}");
    }
}

// A dotted tunnel name reaches the envelope as a quoted segment, because that
// is the dot-path an operator would type at the settings route.
#[tokio::test]
async fn the_rotate_envelope_quotes_a_dotted_interface_name() {
    let (router, token) = failing_app(Some("org.freedesktop.DBus.Error.InvalidArgs")).await;
    let response = bearer_form(
        &router,
        "/api/v1/actions/wireguard/wg.0/rotate-key",
        &token,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["path"], json!(r#"network."wg.0""#));
}

// §3.1's trap, for the one route this milestone adds: an unauthenticated call
// answers §2.4's envelope with a 401 and **never** a redirect, in both gate
// modes. It is a POST, so a client that followed the gate's 303 would land on
// `GET /login`, read 200, and believe it had rotated a key.
#[tokio::test]
async fn the_rotate_route_is_401_without_a_session_in_both_gate_modes() {
    let (configured, _) = test_app(kinds_tree("hunter2secret"));
    let (fresh, _) = test_app(unconfigured_tree());

    for (mode, router) in [("configured", &configured), ("setup mode", &fresh)] {
        let response = post_form(router, ROTATE_PATH, "", None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{mode}");
        assert_eq!(
            response.headers().get(LOCATION),
            None,
            "{mode} answered a redirect, which a script reads as success"
        );
        assert_api_headers(&response, mode);
        assert_eq!(
            envelope(response).await["code"],
            "not_authenticated",
            "{mode}"
        );
    }
}

// The gate hands off exactly what the router serves, and nothing else: an
// interface name carrying a path separator is not this route.
#[tokio::test]
async fn a_rotate_path_with_an_extra_segment_is_the_subtrees_404() {
    let (router, _, cookie, _token) = kinds_app().await;
    for path in [
        "/api/v1/actions/wireguard/a/b/rotate-key",
        "/api/v1/actions/wireguard/wg0/rotate-key/extra",
        "/api/v1/actions/wireguard/wg0",
    ] {
        let response = post_form(&router, path, "", Some(&cookie)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        assert_eq!(envelope(response).await["code"], "not_found", "{path}");
    }
}

// The gate and the router agree about the empty interface segment, which is a
// path this router really serves: `{iface}` matches zero characters where
// `{*path}` matches at least one.
//
// The consequence is what is asserted: an unauthenticated call answers §2.4's
// envelope rather than the gate's redirect, exactly as the named interface
// does, and mosd is what refuses the empty name.
#[tokio::test]
async fn the_empty_interface_segment_is_the_route_and_not_a_redirect() {
    const EMPTY: &str = "/api/v1/actions/wireguard//rotate-key";

    let (router, _) = test_app(kinds_tree("hunter2secret"));
    let response = post_form(&router, EMPTY, "", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers().get(LOCATION), None);
    assert_eq!(envelope(response).await["code"], "not_authenticated");

    let (router, token) = failing_app(Some("org.freedesktop.DBus.Error.InvalidArgs")).await;
    let response = bearer_form(&router, EMPTY, &token, "").await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["path"], json!("network."));
}

// There is no GET on it. A rotation replaces a tunnel's identity, so nothing
// that merely follows a link may perform one.
#[tokio::test]
async fn the_rotate_route_has_no_get() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let response = bearer(&router, "GET", ROTATE_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(fake.rotations().is_empty());
}

// The document describes the route this milestone adds, with every outcome it
// has: a client reading only `openapi.json` has to learn them.
#[test]
fn the_openapi_document_covers_the_rotate_route() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    let responses =
        &document["paths"]["/api/v1/actions/wireguard/{iface}/rotate-key"]["post"]["responses"];
    // The 404 arrived later: an interface that is not a declared entry
    // names nothing, which is what every other read on this API already
    // answered 404 for.
    for status in ["200", "401", "404", "422", "500", "503"] {
        assert!(
            responses[status].is_object(),
            "the rotate route is missing its {status}: {document}"
        );
    }
    // And it is a POST only: a documented GET would be a contract for a route
    // that does not exist.
    assert!(
        document["paths"]["/api/v1/actions/wireguard/{iface}/rotate-key"]["get"].is_null(),
        "{document}"
    );
    // The success body carries the public half and no other member.
    let properties = &document["components"]["schemas"]["WireguardRotation"]["properties"];
    assert_eq!(
        properties.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["publicKey"],
        "{document}"
    );
}

// The fail-closed guard, driven from the failing side: a `privateKey` planted
// in either tree comes back as the sentinel, and its value reaches no surface
// this daemon serves.
//
// Nothing in the shipped schema produces such a field. That is the point: the
// denylist entry exists so that the day one appears, it is already covered.

// The live-state fields M5 added reach the API surface: `kind` on every entry
// and `publicKey` on the tunnel, passed through untouched.
#[tokio::test]
async fn the_state_route_serves_the_kind_and_the_public_key() {
    let (router, _, _cookie, token) = kinds_app().await;

    let response = bearer(&router, "GET", "/api/v1/state/network", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["eth0"]["kind"], json!("physical"));
    assert_eq!(body["eth0.100"]["kind"], json!("vlan"));
    assert_eq!(body["br0"]["kind"], json!("bridge"));
    assert_eq!(body["wg0"]["kind"], json!("wireguard"));
    assert_eq!(body["wg0"]["publicKey"], json!(PEER_KEY));
    // No entry carries a private key, because mosd publishes none.
    for (name, entry) in body.as_object().unwrap() {
        assert!(
            entry.get("privateKey").is_none(),
            "{name} carries a private key: {entry}"
        );
    }
}

// `GET /api/v1/health` (§2.4 case 3) and §2.4's envelope on a method a
// declared `/api/` route does not serve.

// A tree with an admin password, and a live-state tree carrying the `uptime`
// key mosd serves at read time.
fn health_app(uptime: u64) -> (Router, Arc<FakeSettings>) {
    let (router, fake) = test_app(configured_tree("hunter2secret"));
    fake.set_state_entry("uptime", json!(uptime));
    (router, fake)
}

// §2.4 case 3's first shape, exactly: `apid` ok, `mosd` ok, and `checkedAt`
// carrying the appliance's uptime.
//
// `checkedAt` is asserted as a JSON **number**, which is the decision
// The measurement records: there is no trusted wall clock in this
// crate, and the one clock there is — `GET /api/v1/state/uptime` — is a bare
// count of whole seconds. A health answer stamped any other way would be
// stamped with a clock this appliance does not have.

// The case the route exists for: mosd is dead and the answer is still **200**.
//
// A 503 here would be indistinguishable from the endpoint itself being down,
// which is the confusion §2.4 case 3 says the route removes. The fixture is
// the discriminating one: `FailingSettings` answers `GetSettings("access")` —
// so the gate is satisfied, a session mints, and the access cache is warm —
// and fails every state read. A health route reading a cached flag instead of
// the bus would report `ok` here.
#[tokio::test]
async fn health_reports_an_unreachable_mosd_and_still_answers_200() {
    let (router, token) = failing_app(None).await;

    let response = bearer(&router, "GET", "/api/v1/health", &token).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a dead mosd is reported in the body, never as a status code"
    );
    assert_api_headers(&response, "/api/v1/health");
    assert_eq!(response.headers().get(RETRY_AFTER), None);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["apid"], json!("ok"));
    assert_eq!(body["mosd"], json!("unreachable"));
    assert!(
        body["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "the unreachable answer must say why: {body}"
    );
    assert!(body.get("checkedAt").is_none(), "{body}");
}

// The probe is a live bus call and not a flag: move the appliance's uptime and
// the next answer moves with it, in the same router and the same session.

// Authenticated like every other `/api/v1/` route, and its refusal is §2.4's
// envelope rather than the gate's HTML redirect (§3.1).
#[tokio::test]
async fn health_without_a_session_is_the_envelope_and_not_a_redirect() {
    let (router, _) = health_app(90_061);

    let response = get(&router, "/api/v1/health", None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers().get(LOCATION), None);
    assert_api_headers(&response, "/api/v1/health anonymous");
    assert_eq!(envelope(response).await["code"], "not_authenticated");
}

// `/healthz` is unchanged by any of this, and this test is the pin.
//
// The boot health gate probes exactly this path, unauthenticated, and treats
// any non-2xx as a failed boot (`rootfs/overlay/usr/lib/mos/mos-health`),
// so its path, its exemption, its status, its literal body and the fact that
// it is not JSON are all load-bearing. §2.4 case 3 is explicit that `/healthz`
// cannot be fixed and that the API adds a second endpoint instead — the two
// answer different questions.
#[tokio::test]
async fn healthz_is_untouched_by_the_health_route() {
    let (router, _) = health_app(90_061);

    let response = get(&router, "/healthz", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_value(&response, CONTENT_TYPE),
        "text/plain; charset=utf-8"
    );
    assert_eq!(body_string(response).await, "ok");

    // Still `ok` with mosd dead, which is the property §2.4 case 3 calls the
    // trap and answers with a second route rather than by changing this one.
    let (failing, _) = failing_app(None).await;
    let response = get(&failing, "/healthz", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_string(response).await, "ok");
}

// Every declared `/api/` route, on a method it does not serve: §2.4's
// envelope, 405, and an `Allow` header naming what the route does serve.
//
// The expectation names the `Allow` value per route rather than deriving it,
// so a route that quietly gained or lost a method fails here.
#[tokio::test]
async fn a_wrong_method_on_a_declared_api_route_answers_the_envelope() {
    let (router, fake) = health_app(90_061);
    fake.set_state_entry(
        "network",
        json!({ "wg0": { "kind": "wireguard", "publicKey": "k" } }),
    );
    let cookie = login(&router, "hunter2secret").await;

    for (method, path, allow) in [
        ("POST", "/api/versions", "GET,HEAD"),
        ("POST", "/api/v1/meta", "GET,HEAD"),
        ("DELETE", "/api/v1/health", "GET,HEAD"),
        ("POST", "/api/v1/settings/hostname", "GET,HEAD,PUT"),
        ("PUT", "/api/v1/state/uptime", "GET,HEAD"),
        ("GET", "/api/v1/actions/change-password", "POST"),
        ("GET", "/api/v1/actions/wireguard/wg0/rotate-key", "POST"),
        // M7's three verbs. `GET` on each of them is the assertion that no
        // `GET` handler is declared: the HTML router refuses the same thing
        // deliberately so a browser prefetch, a crawler or a mis-clicked link
        // cannot power the appliance off, and `actions` is named `actions` so
        // no reader expects a `GET` to work there. Asserted here rather than in
        // a test of their own so the `Allow` value is checked by the same
        // per-route expectation every other declared route is checked by.
        ("GET", "/api/v1/actions/reboot", "POST"),
        ("GET", "/api/v1/actions/poweroff", "POST"),
        ("GET", "/api/v1/actions/transient-root-password", "POST"),
        ("PUT", "/api/v1/tokens", "GET,HEAD,POST"),
        ("GET", "/api/v1/tokens/deadbeef", "DELETE"),
        // M8's one route. `GET` on it for the reason the three actions above
        // get one: it is the assertion that no `GET` handler is declared, made
        // by the same per-route expectation as every other declared route. On
        // a configured device this is a 405 and not the 409 a `POST` gets --
        // the router refuses the method before the handler sees the tree.
        ("GET", "/api/v1/setup", "POST"),
    ] {
        let response = request(&router, method, path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} {path}"
        );
        assert_eq!(header_value(&response, ALLOW), allow, "{method} {path}");
        assert_api_headers(&response, &format!("{method} {path}"));
        let error = envelope(response).await;
        assert_eq!(error["code"], "method_not_allowed", "{method} {path}");
        // apid, not mosd: the router refused this before any bus call.
        assert_eq!(error["source"], "apid", "{method} {path}");
        assert!(
            error["message"].is_string(),
            "{method} {path}: §2.4 requires a message"
        );
        // A wrong method names no settings dot-path, so the optional member is
        // absent rather than empty.
        assert!(error.get("path").is_none(), "{method} {path}: {error}");
    }
}

// The 405 is the router's answer and not an authenticated one, which is what
// the shipped tree already did: `is_declared_api_route` tests the path and not
// the method, so the gate hands a wrong-method request off exactly as it hands
// off a right one. Recorded because it is a property, not an accident.
#[tokio::test]
async fn the_405_envelope_does_not_depend_on_a_session() {
    let (router, _) = health_app(90_061);

    let response = request(&router, "POST", "/api/v1/meta", None, None).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(response.headers().get(LOCATION), None);
    assert_eq!(header_value(&response, ALLOW), "GET,HEAD");
    assert_eq!(envelope(response).await["code"], "method_not_allowed");
}

// The asset router's 405 is outside `/api/` and is not unified with the one
// above: `docs/design/api.md` §4.2 condition 2 gives it a bare body, its own
// `Allow: GET, HEAD` and no `Content-Type` at all, and §2.4's envelope is a
// promise about `/api/v1/` routes only.
//
// Asserted here as a contrast — the same method against both routers in one
// test — so that a later attempt to give the whole server one 405 fails with
// the distinction printed rather than silently widening a promise.

// The reserved subtree's own 404 is untouched by the 405: a path the API does
// not declare is still `not_found`, on every method, health-adjacent spellings
// included.
#[tokio::test]
async fn the_api_fallback_404_survives_the_405() {
    let (router, _) = health_app(90_061);
    let cookie = login(&router, "hunter2secret").await;

    for (method, path) in [
        ("GET", "/api/v1/health/extra"),
        ("POST", "/api/v1/health/extra"),
        ("GET", "/api/v1/healthz"),
        ("DELETE", "/api/v1/nope"),
        ("POST", "/api/nope"),
    ] {
        let response = request(&router, method, path, Some(&cookie), Some(BROWSER_ACCEPT)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(response.headers().get(ALLOW), None, "{method} {path}");
        assert_eq!(
            envelope(response).await["code"],
            "not_found",
            "{method} {path}"
        );
    }
}

// The document describes the route and the outcome this milestone adds: a
// client reading only `openapi.json` has to be able to learn both.

// §3.2's bearer token — the credential, the three `/api/v1/tokens` routes,
// and the bootstrap pane under §6.3's reserved prefix.

// A stored entry and the plaintext that opens it, both derived from `index`
// so two calls differ in every field identity is keyed on.
//
// Built here rather than minted, because a test that needs a full list needs
// 32 of them and the mint is one of the things under test.
fn seeded_token(index: usize) -> (serde_json::Value, String) {
    let id = format!("{index:08x}");
    let secret = format!("{index:064x}");
    (
        json!({
            "id": id,
            "name": format!("seeded-{index}"),
            "hash": crate::token::digest(&secret),
            "created": 1,
        }),
        format!("mos_{id}_{secret}"),
    )
}

// `tree` with one usable bearer token seeded into it, and that token's
// plaintext.
//
// The M9 shape of every fixture whose subject is an `/api/v1/`
// route. Seeded and **not** minted through §3.2's pane, for a reason the
// tests would otherwise have to excuse one at a time: a mint is itself a
// write to `access.apiTokens`, and twenty of the tests below assert
// `set_paths()` exactly -- several of them assert it is EMPTY, which is the
// whole content of a refusal test. Seeding puts the credential in the
// starting tree, so the only writes in a run are the ones under test.
fn with_token(mut tree: serde_json::Value) -> (serde_json::Value, String) {
    let (entry, wire) = seeded_token(0);
    // APPENDED and not assigned: some fixtures ship their own `apiTokens` and
    // assert on entry 0 by id -- `secret_tree`'s `ci-deploy` entry, whose
    // digest is the redaction canary. Overwriting the array would take that
    // fixture away and the test would fail describing the wrong thing. The
    // seeded credential goes on the end, so entry 0 is whatever the caller put
    // there.
    match tree["access"]["apiTokens"].as_array_mut() {
        Some(existing) => existing.push(entry),
        None => tree["access"]["apiTokens"] = json!([entry]),
    }
    (tree, wire)
}

// A configured tree holding `count` usable tokens, with their plaintexts.
fn token_tree(password: &str, count: usize) -> (serde_json::Value, Vec<String>) {
    let (entries, wires): (Vec<_>, Vec<_>) = (0..count).map(seeded_token).unzip();
    let mut tree = configured_tree(password);
    tree["access"]["apiTokens"] = json!(entries);
    (tree, wires)
}

// A request carrying a bearer token and **no cookie**, which is what makes
// every assertion below about the token rather than about the session.
async fn bearer(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
) -> Response<axum::body::Body> {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header(AUTHORIZATION, format!("Bearer {token}"));
    send(router, builder.body(Body::empty()).unwrap()).await
}

// A form body carrying a bearer token and no cookie.
//
// The action routes take a form encoding rather than JSON, so the bearer
// equivalent of `post_form` is its own helper rather than a flag on one.
async fn bearer_form(
    router: &Router,
    path: &str,
    token: &str,
    body: &str,
) -> Response<axum::body::Body> {
    let builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(AUTHORIZATION, format!("Bearer {token}"));
    send(router, builder.body(Body::from(body.to_string())).unwrap()).await
}

// A JSON body carrying a bearer token and no cookie.
async fn bearer_json(
    router: &Router,
    method: &str,
    path: &str,
    token: &str,
    body: &str,
) -> Response<axum::body::Body> {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header(CONTENT_TYPE, "application/json")
        .header(AUTHORIZATION, format!("Bearer {token}"));
    send(router, builder.body(Body::from(body.to_string())).unwrap()).await
}

// The pane's sentence, ratified and asserted
// byte for byte.
//
// Token revocation on a password change stays out — a password change would
// otherwise destroy N credentials the operator cannot see, with no
// confirmation and no undo — so the pane has to say so. A paraphrase would
// quietly drop the containment advice, which is the part of it that matters,
// so the assertion is verbatim rather than on keywords.

// The bootstrap end to end: a browser session mints, the plaintext appears
// once, the tree keeps only a digest, and the token then authenticates the
// API on its own.

// **The cutover, asserted (M9).** Amendment 1 opened a
// dual-credential window and scheduled its close for a named milestone; this
// is the assertion that it closed. Every `/api/v1/` route takes the bearer,
// and the session cookie that used to work on the shipped four is now a 401.
//
// This test is the amended form of `every_shipped_api_route_takes_a_bearer_or_the_cookie`,
// and the amendment is a TIGHTENING and not a weakening: the bearer arm is
// unchanged and still asserts 200 on all four, while the cookie arm flipped
// from asserting 200 to asserting 401 -- the behaviour §3.2 always specified.
//
// The cookie arm asserts the envelope and not merely the status, because
// §3.1's trap is a redirect and not a refusal: a 303 to `/login` would answer
// 200 with HTML on the next hop and a script would read the exchange as
// success. So the assertion is 401 **and** `not_authenticated` in §2.4's
// shape, which is a thing a script can parse.

// The boundary inside Amendment 1: the three token routes take a bearer and
// nothing else, and a session cookie presented to any of them is a 401.
//
// §3.2 rejects the cookie-accepting mint by name, because it would put a
// permanent-credential factory inside the surface §3.3 makes its strongest
// statement about. The amendment preserves the credentials of routes that
// already shipped, and these had not.

// The three routes as a lifecycle: mint, list, revoke, and the revoked token
// stops being accepted on the next request.
#[tokio::test]
async fn the_api_mints_lists_and_revokes() {
    let (tree, wires) = token_tree("hunter2secret", 1);
    let (router, _) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/tokens",
        &wires[0],
        r#"{"name":"ci-deploy"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_api_headers(&response, "POST /api/v1/tokens");
    let minted: serde_json::Value =
        serde_json::from_str(&body_string(response).await).expect("a JSON body");
    let wire = minted["token"].as_str().expect("the plaintext").to_string();
    let id = minted["id"].as_str().expect("the id").to_string();
    assert_eq!(minted["name"], json!("ci-deploy"));
    assert!(crate::token::parse(&wire).is_some(), "{wire}");

    // The listing carries identity and never a secret — neither the digest
    // that is stored nor the plaintext that is not.
    let response = bearer(&router, "GET", "/api/v1/tokens", &wire).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        !body.contains(&wire),
        "the plaintext reached a listing: {body}"
    );
    assert!(
        !body.contains("hash"),
        "the digest reached a listing: {body}"
    );
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let names: Vec<&str> = listed
        .as_array()
        .expect("an array")
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["seeded-0", "ci-deploy"]);

    // Revocation takes effect on the next request.
    let response = bearer(&router, "DELETE", &format!("/api/v1/tokens/{id}"), &wire).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        bearer(&router, "GET", "/api/v1/tokens", &wire)
            .await
            .status(),
        StatusCode::UNAUTHORIZED,
        "a revoked token must stop working"
    );
    assert_eq!(
        bearer(&router, "GET", "/api/v1/tokens", &wires[0])
            .await
            .status(),
        StatusCode::OK,
        "revoking one token must not revoke another"
    );
}

// The cap is answered at the route, in the caller's terms.
//
// The store makes a full list a hard refusal, so without a check here the
// caller meets it as a failed write — a 500 about mosd — instead of an answer
// about the request. 409 and not 422: the body is well formed and what refuses
// it is the collection's state.

// A name the store would refuse is refused at the route, as a 422 about the
// body rather than as a failed write.
#[tokio::test]
async fn a_name_the_store_refuses_is_a_422() {
    let (tree, wires) = token_tree("hunter2secret", 1);
    let (router, fake) = test_app(tree);

    for body in [r#"{"name":""}"#, r#"{"name":"ci\ndeploy"}"#] {
        let response = bearer_json(&router, "POST", "/api/v1/tokens", &wires[0], body).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        assert_eq!(
            envelope(response).await["code"],
            "validation_failed",
            "{body}"
        );
    }
    // And a body that is not this shape at all is a 400, not a 422.
    let response = bearer_json(&router, "POST", "/api/v1/tokens", &wires[0], "{}").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(envelope(response).await["code"], "request_invalid");

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The collection error contract: a well-formed
// identifier that names nothing is **404**, and 422 is reserved for an
// identifier that is not well formed at all.
//
// Paired with `the_builtin_revoke_pane_answers_422_where_the_api_answers_404`,
// which asserts the HTML surface's deliberately different answer to the same
// condition.
#[tokio::test]
async fn an_absent_token_id_is_404_and_a_malformed_one_is_422() {
    let (tree, wires) = token_tree("hunter2secret", 1);
    let (tree, _token) = with_token(tree);
    let (router, fake) = test_app(tree);

    // Well formed, and no entry carries it.
    let response = bearer(&router, "DELETE", "/api/v1/tokens/deadbeef", &wires[0]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!("access.apiTokens"));

    // Not an identifier at all: well formed and absent is a different answer
    // from not well formed, and they must not share a status.
    for path in ["/api/v1/tokens/NOTHEX", "/api/v1/tokens/ci-deploy"] {
        let response = bearer(&router, "DELETE", path, &wires[0]).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
        assert_eq!(
            envelope(response).await["code"],
            "validation_failed",
            "{path}"
        );
    }

    // The empty spelling is not this route -- measured, and not assumed from
    // the rotate action, whose empty `{iface}` segment is interior rather than
    // trailing and IS served. `/api/v1/tokens/` reaches the reserved subtree's
    // own not-found handler. That measurement carried a consequence until
    // `token_id` had to refuse this spelling, or the gate would
    // release an unauthenticated caller to a 404 where a redirect was owed --
    // and the consequence is gone now that the gate releases the whole subtree
    // either way. What is left is the measurement of which handler answers,
    // and this arm's answer is unchanged by M2 in status, code and body.
    let cookie = login(&router, "hunter2secret").await;
    let response = request(&router, "DELETE", "/api/v1/tokens/", Some(&cookie), None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(envelope(response).await["code"], "not_found");

    // And the bearer arm of the same path answers the same thing, because the
    // gate releases the whole reserved subtree and stops there: the credential
    // is never read, so it cannot decide the medium. This arm asserted the 303
    // to `/login` until that was closed
    // asymmetry; `an_undeclared_api_path_answers_the_404_envelope_whatever_the_credential`
    // is the general statement, and this is the one path that carried the old
    // answer.
    let response = bearer(&router, "DELETE", "/api/v1/tokens/", &wires[0]).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(envelope(response).await["code"], "not_found");

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// §4.1 rule 1's medium rule, stated the way the gate has to implement it: a
// path inside the reserved `/api` subtree that no route declares answers
// §2.4's 404 envelope, and **which credential the request carried is not
// what decides that**. A client that addressed the JSON surface is answered
// in JSON whether it sent nothing at all, a token that is not stored, a
// token that is, or a session cookie.
//
// The four answers are compared to each other and not only to a literal, so
// what is asserted is the symmetry itself. The cookie arm is the fixed point:
// it is the answer this repository already documented — `an_absent_token_id_
// is_404_and_a_malformed_one_is_422` asserts `not_found` on `/api/v1/tokens/`
// through a cookie — and the other three are required to be the same bytes,
// so a future change that moves the cookie arm fails here rather than
// silently taking the other three with it.
//
// `/apibogus` is in the list for the reason
// `each_guard_is_exercised_by_exactly_one_hostile_feature` states: the
// assertion has to distinguish the rule from its absence. The subtree is
// `/api` and what nests under it, not the four characters, so a path that
// merely begins with them is an HTML path and still redirects.

// The same rule in **setup mode**, which is where the gate's *other* redirect
// lives and so is a second exit that has to be closed rather than the same
// one twice.
//
// A device with no admin password yet still has a reserved `/api` subtree,
// and a client asking it for a path that does not exist is asking a question
// the setup form is not an answer to.

// **Bearer verification is not rate limited, and must not be.**
//
// 256 bits of `OsRng` is not guessable online, and the login backoff is a
// single global counter (`auth::GuardStore`), so a shared counter on the token
// path would let anyone holding a bad token lock out every script *and* every
// login on the appliance. The assertion is both halves: a good token still
// works after a long run of bad ones, and the password path's counter was
// never touched by them.

// The published document describes the routes this milestone adds, and
// describes them as the code serves them.
#[test]
fn the_openapi_document_covers_the_token_routes() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    let collection = &document["paths"]["/api/v1/tokens"];
    for (method, statuses) in [
        ("get", vec!["200", "401"]),
        ("post", vec!["201", "400", "401", "409", "422"]),
    ] {
        for status in statuses {
            assert!(
                collection[method]["responses"][status].is_object(),
                "{method} /api/v1/tokens must document {status}: {collection}"
            );
        }
    }

    let item = &document["paths"]["/api/v1/tokens/{id}"]["delete"]["responses"];
    for status in ["204", "401", "404", "422"] {
        assert!(
            item[status].is_object(),
            "DELETE must document {status}: {item}"
        );
    }

    // The listing's row carries identity and never a secret, in the document
    // as well as on the wire.
    let summary = &document["components"]["schemas"]["ApiTokenSummary"]["properties"];
    let members: Vec<&str> = summary
        .as_object()
        .expect("properties")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(members, vec!["created", "id", "name"], "{summary}");

    // The plaintext is a member of the mint's response and of nothing else.
    let minted = &document["components"]["schemas"]["MintedToken"]["properties"];
    assert!(minted["token"].is_object(), "{minted}");
}

// A tree with all four writable paths already present, so a write is a change
// of value and never a creation -- the creation case is what the refusal list
// exists to prevent, and it must not be smuggled into the happy path.
fn writable_tree(password: &str) -> serde_json::Value {
    let mut tree = configured_tree(password);
    tree["access"]["ssh"] = json!({ "enabled": false });
    tree["container"] = json!({ "enabled": false });
    tree["mqtt"] = json!({ "enabled": false });
    tree["time"] = json!({ "ntp": { "servers": [] }, "timezone": "UTC" });
    tree
}

// The six dot-paths admitted, each written and
// each read back through the route that answers for it.
//
// 202 and a task id: persistence has completed, while reconciliation is a
// separately observable lifecycle.
#[tokio::test]
async fn the_write_route_writes_the_six_scalar_settings() {
    let (tree, token) = with_token(writable_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    for (path, body) in [
        ("hostname", r#""router7""#),
        ("access.ssh.enabled", "true"),
        ("container.enabled", "true"),
        ("mqtt.enabled", "true"),
        ("time.ntp.servers", r#"["0.pool.ntp.org","192.0.2.7"]"#),
        ("time.timezone", r#""Europe/Berlin""#),
    ] {
        let url = format!("/api/v1/settings/{path}");
        let response = bearer_json(&router, "PUT", &url, &token, body).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{path}");
        assert_eq!(header_value(&response, CACHE_CONTROL), "no-store", "{path}");
        let accepted = body_json(response).await;
        let task_id = accepted["taskId"].as_str().expect("a task id");

        let task = bearer(&router, "GET", &format!("/api/v1/tasks/{task_id}"), &token).await;
        assert_eq!(task.status(), StatusCode::OK, "{path}");
        let task = body_json(task).await;
        assert_eq!(task["dotPath"], path, "{task}");
        assert_eq!(task["status"], "finished", "{task}");
        assert_eq!(task["outcome"], "succeeded", "{task}");

        let read = bearer(&router, "GET", &url, &token).await;
        assert_eq!(read.status(), StatusCode::OK, "{path}");
        assert_eq!(
            body_string(read).await,
            body,
            "{path} reads back as written"
        );
    }

    assert_eq!(
        fake.set_paths(),
        vec![
            "hostname",
            "access.ssh.enabled",
            "container.enabled",
            "mqtt.enabled",
            "time.ntp.servers",
            "time.timezone"
        ],
        "one bus write per request, at the dot-path the URL named"
    );

    let tasks = bearer(&router, "GET", "/api/v1/tasks", &token).await;
    assert_eq!(tasks.status(), StatusCode::OK);
    assert_eq!(body_json(tasks).await.as_array().unwrap().len(), 6);

    let missing = bearer(&router, "GET", "/api/v1/tasks/not-retained", &token).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert_eq!(envelope(missing).await["code"], "task_not_found");
}

// §2.2's round trip, driven exactly as the client that motivates the rule
// would drive it: read a subtree, hand it back, and find the credential
// intact rather than replaced by the sentinel.
//
// *"A redacted field is **read-only through the API**: a `PUT` whose body
// contains `"<redacted>"` is rejected at 422 rather than written, because
// writing the sentinel would silently destroy the credential."*
// (`docs/design/api.md` §2.2) Without the refusal this test's `PUT`
// succeeds and `access.webAdmin.password_hash` becomes the literal string
// `<redacted>`, which no password verifies against and no operator can undo.
#[tokio::test]
async fn a_write_carrying_the_redaction_sentinel_is_refused_and_writes_nothing() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    // The exact bytes a client would have read, sentinels and all.
    let read = bearer(&router, "GET", "/api/v1/settings/access", &token).await;
    assert_eq!(read.status(), StatusCode::OK);
    let redacted = body_string(read).await;
    assert!(
        redacted.contains(REDACTED),
        "the fixture must carry a redacted field: {redacted}"
    );

    let response = bearer_json(&router, "PUT", "/api/v1/settings/access", &token, &redacted).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_api_headers(&response, "the sentinel refusal");
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!("access"));
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|text| text.contains(REDACTED)),
        "the message has to name what it refused: {error}"
    );

    // Nothing reached the bus, and the credential the sentinel stood for still
    // verifies -- which is the whole of what this rule protects.
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert!(
        !login(&router, "hunter2secret").await.is_empty(),
        "the admin hash must still be the hash"
    );

    // The same rule on an allowlisted path, where the sentinel is the whole
    // body rather than a field inside one: a client that read
    // `access.webAdmin.password_hash` got a bare `"<redacted>"` string back.
    let response = bearer_json(
        &router,
        "PUT",
        "/api/v1/settings/hostname",
        &token,
        &format!("\"{REDACTED}\""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["code"], "validation_failed");
    assert!(fake.set_paths().is_empty());
}

// The refusal list: a dot-path the schema has and this route does not write
// is **409 `settings_read_only`**, answered before any bus call.
//
// 409 and not 422 for the reason §2.4 already spends it on and the mint route
// already uses: the body is well formed and nothing about it is wrong, and
// what refuses it is the state of the surface.
#[tokio::test]
async fn every_dot_path_outside_the_allowlist_is_refused_with_409() {
    let (tree, token) = with_token(writable_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    for path in [
        "schema_version",
        "network",
        "network.eth0",
        "network.eth0.dhcp",
        "access",
        "access.ssh",
        "access.ssh.authorizedKeys",
        "access.webAdmin.password_hash",
        "provisioning",
        "wifi",
        "wifi.client.networks",
        "container",
        "mqtt",
        "mqtt.listen.port",
        "time",
        "time.ntp",
        // `.` is the whole tree, not a malformed path: `Settings::set`
        // documents `""` and `"."` as replacing the root, so it is a real path
        // this route refuses rather than one it cannot parse.
        ".",
    ] {
        let response = bearer_json(
            &router,
            "PUT",
            &format!("/api/v1/settings/{path}"),
            &token,
            "true",
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
        assert_api_headers(&response, path);
        let error = envelope(response).await;
        assert_eq!(error["code"], "settings_read_only", "{path}");
        assert_eq!(error["source"], "apid", "{path}");
        assert_eq!(error["path"], json!(path), "{path}");
    }

    assert!(
        fake.set_paths().is_empty(),
        "a refused write must reach no bus call, got {:?}",
        fake.set_paths()
    );
}

// Two refusals carry a message the general one cannot, and both are asserted
// because both are the reason the path is refused rather than decoration.
#[tokio::test]
async fn the_two_named_refusals_say_why_rather_than_only_that() {
    let (tree, token) = with_token(writable_tree("hunter2secret"));
    let (router, _) = test_app(tree);

    // `schema_version` is read-only in the tree itself, not merely here: no
    // later milestone widens this route to cover it.
    let response = bearer_json(
        &router,
        "PUT",
        "/api/v1/settings/schema_version",
        &token,
        "9",
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let message = envelope(response).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("read-only in the settings tree itself"),
        "{message}"
    );

    // `network` names the typed route that owns it, because a raw write here
    // creates an entry of the default kind rather than refusing an interface
    // the device does not have.
    let response = bearer_json(
        &router,
        "PUT",
        "/api/v1/settings/network.wg9",
        &token,
        r#"{"dhcp": true}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let message = envelope(response).await["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.contains("PUT /api/v1/network/{iface}"),
        "the refusal must name the route that does own it: {message}"
    );
}

// §2.4's rule, on the write route: **well-formed but absent is 404, not
// well-formed is 422**, and they must not share a status.
//
// "Absent" is decided on the first segment, and that is a statement about
// writes. A write may legitimately create the leaf it names -- `Settings::set`
// creates missing intermediates -- so a missing leaf is not an absent
// resource; a top-level key the typed schema has no field for is, because no
// write can ever make the tree deserialize with one.
#[tokio::test]
async fn an_absent_root_is_404_and_a_malformed_path_is_422() {
    let (tree, token) = with_token(writable_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    for path in ["hostnam", "netwrok.eth0", "acess.ssh.enabled", "sshd"] {
        let response = bearer_json(
            &router,
            "PUT",
            &format!("/api/v1/settings/{path}"),
            &token,
            "true",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let error = envelope(response).await;
        assert_eq!(error["code"], "settings_not_found", "{path}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|text| text.contains(path)),
            "{path}: {error}"
        );
    }

    for path in [
        "access..ssh",
        "access.\"ssh",
        "access.\"ssh\"x",
        "hostname.",
    ] {
        let response = bearer_json(
            &router,
            "PUT",
            &format!("/api/v1/settings/{path}"),
            &token,
            "true",
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
        let error = envelope(response).await;
        assert_eq!(error["code"], "validation_failed", "{path}");
    }

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The nine top-level keys the write route's not-found rule is derived from.
//
// `is_settings_root` reads them out of `Settings::default()` rather than
// carrying a list, so this asserts the derivation rather than a copy of it: a
// field added to `Settings` changes this expectation and the route together,
// and a `#[serde(skip_serializing_if)]` on a top-level field -- which would
// drop a real root out of the default tree and turn its 409 into a 404 --
// fails here.
#[test]
fn the_settings_schema_has_the_nine_roots_the_write_route_knows() {
    let tree = serde_json::to_value(mosd_settings::Settings::default()).unwrap();
    let mut keys: Vec<&str> = tree
        .as_object()
        .expect("the settings tree is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "access",
            "container",
            "hostname",
            "mqtt",
            "network",
            "provisioning",
            "schema_version",
            "time",
            "wifi",
        ]
    );
}

// Each writable path's value has one shape, and a body of the wrong shape is
// a 422 that names the shape rather than a write of whatever arrived.
//
// The hostname sentence is `HOSTNAME_RULES`, the same string the form pane
// puts in its error box: one rule, one wording, two surfaces.
#[tokio::test]
async fn a_body_of_the_wrong_shape_is_refused_and_not_written() {
    let (tree, token) = with_token(writable_tree("hunter2secret"));
    let (router, fake) = test_app(tree);

    for (path, body, expected) in [
        ("hostname", "true", "text"),
        ("hostname", "7", "text"),
        ("hostname", r#"["a"]"#, "text"),
        ("hostname", r#""-nope-""#, "hyphen"),
        ("hostname", r#""""#, "1-63"),
        ("hostname", r#""has space""#, "1-63"),
        ("access.ssh.enabled", r#""yes""#, "switch"),
        ("container.enabled", "1", "switch"),
        ("mqtt.enabled", "null", "switch"),
        ("time.timezone", "true", "text"),
        ("time.timezone", r#""Not A Zone!""#, "IANA"),
        ("time.timezone", r#""Etc//UTC""#, "IANA"),
        ("time.ntp.servers", r#""0.pool.ntp.org""#, "list"),
        ("time.ntp.servers", "[7]", "list"),
        ("time.ntp.servers", r#"["bad server"]"#, "host name"),
        ("time.ntp.servers", r#"["a.example","a.example"]"#, "twice"),
    ] {
        let response = bearer_json(
            &router,
            "PUT",
            &format!("/api/v1/settings/{path}"),
            &token,
            body,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path} <- {body}"
        );
        let error = envelope(response).await;
        assert_eq!(error["code"], "validation_failed", "{path} <- {body}");
        assert_eq!(error["path"], json!(path), "{path} <- {body}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|text| text.contains(expected)),
            "{path} <- {body}: {error}"
        );
    }

    // Not JSON at all is 400 and not 422: the request never became a value to
    // validate. Same classification the mint route gives the same condition.
    let response = bearer_json(
        &router,
        "PUT",
        "/api/v1/settings/hostname",
        &token,
        "router7",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(envelope(response).await["code"], "request_invalid");

    // A body with no `Content-Type: application/json` is the same refusal.
    let response = send(
        &router,
        Request::builder()
            .method("PUT")
            .uri("/api/v1/settings/hostname")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(r#""router7""#))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(envelope(response).await["code"], "request_invalid");

    assert!(
        fake.set_paths().is_empty(),
        "a refused write must write nothing, got {:?}",
        fake.set_paths()
    );
}

// The credential: bearer **or** cookie, which is the
// ruling applied to a new route. The bearer-only rule is about the token
// routes specifically, so this route matches the shipped reads instead.

// A write mosd refuses is classified by §2.4's table exactly as a read is:
// the route adds no second opinion, and mosd's own message comes through.
#[tokio::test]
async fn a_write_mosd_refuses_carries_mosds_classification() {
    for (fdo_name, code, status) in [
        (
            "org.freedesktop.DBus.Error.InvalidArgs",
            "settings_rejected",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "com.mos.mosd1.Error.ReadOnly",
            "settings_read_only",
            StatusCode::CONFLICT,
        ),
        (
            "org.freedesktop.DBus.Error.IOError",
            "settings_io",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let (router, token) = failing_app(Some(fdo_name)).await;
        let response = bearer_json(
            &router,
            "PUT",
            "/api/v1/settings/hostname",
            &token,
            r#""router7""#,
        )
        .await;
        assert_eq!(response.status(), status, "{fdo_name}");
        let error = envelope(response).await;
        assert_eq!(error["code"], code, "{fdo_name}");
        assert_eq!(error["source"], "mosd", "{fdo_name}");
        assert_eq!(error["message"], MOSD_MESSAGE, "{fdo_name}");
        assert_eq!(error["path"], json!("hostname"), "{fdo_name}");
    }
}

// The document describes the served surface: a client reading only
// `openapi.json` has to learn the write route, every outcome it has, and that
// its body is a bare JSON value.
#[test]
fn the_openapi_document_covers_the_settings_write() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    let write = &document["paths"]["/api/v1/settings/{path}"]["put"];
    for status in [
        "202", "400", "401", "404", "405", "409", "422", "500", "503", "504",
    ] {
        assert!(
            write["responses"][status].is_object(),
            "the settings write must document {status}: {write}"
        );
    }
    assert_eq!(
        write["requestBody"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/SettingsWrite",
        "{write}"
    );

    // The read is unchanged by the write sharing its path.
    assert!(
        document["paths"]["/api/v1/settings/{path}"]["get"]["responses"]["200"].is_object(),
        "{document}"
    );

    let tasks = &document["paths"]["/api/v1/tasks"]["get"];
    assert!(tasks["responses"]["200"].is_object(), "{tasks}");
    let task = &document["paths"]["/api/v1/tasks/{id}"]["get"];
    for status in ["200", "401", "404", "405", "500", "503", "504"] {
        assert!(task["responses"][status].is_object(), "{status}: {task}");
    }
}

// The two array collections that already
// exist in the settings tree -- the SSH authorized keys, identified by
// fingerprint, and the WiFi station's known networks, identified by SSID.

// The whole body as JSON, for the collection routes that answer a document
// rather than §2.4's envelope.
async fn body_json(response: Response<axum::body::Body>) -> serde_json::Value {
    let body = body_string(response).await;
    serde_json::from_str(&body).unwrap_or_else(|_| panic!("a JSON body, got: {body}"))
}

// The sentence required on the listing and
// on the add, spelled out here rather than read from the constant: a test that
// compares the code against itself cannot notice the sentence being reworded.
const ROOT_KEY_NOTICE_TEXT: &str = "Every authorized key is a root key.";

// The SSH collection's dot-path, as every envelope it raises names it.
const SSH_KEYS_DOT_PATH: &str = "access.ssh.authorizedKeys";

// The WiFi collection's dot-path.
const WIFI_NETWORKS_DOT_PATH: &str = "wifi.client.networks";

// The item route of one stored key.
fn ssh_key_url(fingerprint: &str) -> String {
    format!(
        "/api/v1/ssh/authorized-keys/{}",
        fingerprint.replace('/', "%2F")
    )
}

// A configured tree carrying a `wifi.client` subtree holding `networks`.
//
// The subtree is present even when the list is empty, which is what a real
// tree looks like: `WifiClientSettings::networks` carries no
// `skip_serializing_if`, so mosd's serialization of the typed tree always has
// it.
fn wifi_tree(networks: serde_json::Value) -> serde_json::Value {
    let mut tree = configured_tree("hunter2secret");
    tree["wifi"] = json!({
        "client": { "enabled": false, "interface": "wlan0", "networks": networks },
        "ap": { "mode": "off", "channel": 6 },
    });
    tree
}

// The stored network list, as JSON.
async fn stored_network_list(fake: &FakeSettings) -> serde_json::Value {
    fake.get_settings(WIFI_NETWORKS_DOT_PATH).await.unwrap()
}

// The collection end to end: an empty listing, an add, a listing that shows
// it, and a removal addressed by the fingerprint the add returned.
//
// The fingerprint is the whole handle: it is what `GET` publishes, what
// `DELETE` takes, and it is checked against a value that came out of
// `ssh-keygen` rather than against apid's own arithmetic.
#[tokio::test]
async fn the_ssh_key_collection_lists_adds_and_removes() {
    let (tree, token) = with_token(ssh_tree(json!([])));
    let (router, fake) = test_app(tree);

    let empty = bearer(&router, "GET", "/api/v1/ssh/authorized-keys", &token).await;
    assert_eq!(empty.status(), StatusCode::OK);
    let empty = body_json(empty).await;
    assert_eq!(empty["keys"], json!([]));

    let added = bearer_json(
        &router,
        "POST",
        "/api/v1/ssh/authorized-keys",
        &token,
        &json!({ "key": REAL_ED25519_LINE }).to_string(),
    )
    .await;
    assert_eq!(added.status(), StatusCode::CREATED);
    let added = body_json(added).await;
    // Canonicalised by the parser: the comment is lifted out of `key` so the
    // same key pasted under two labels is one key.
    assert_eq!(added["key"]["key"], json!(canonical(REAL_ED25519_LINE)));
    assert_eq!(
        added["key"]["comment"],
        json!(comment_of(REAL_ED25519_LINE))
    );
    assert_eq!(added["key"]["fingerprint"], json!(REAL_ED25519_FINGERPRINT));
    assert_eq!(
        stored_key_list(&fake).await,
        json!([stored_key(REAL_ED25519_LINE)])
    );
    assert_eq!(fake.set_paths(), vec![SSH_KEYS_DOT_PATH]);

    let listed =
        body_json(bearer(&router, "GET", "/api/v1/ssh/authorized-keys", &token).await).await;
    assert_eq!(listed["keys"].as_array().unwrap().len(), 1);
    assert_eq!(
        listed["keys"][0]["fingerprint"],
        json!(REAL_ED25519_FINGERPRINT)
    );

    let removed = bearer(
        &router,
        "DELETE",
        &ssh_key_url(REAL_ED25519_FINGERPRINT),
        &token,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(stored_key_list(&fake).await, json!([]));
}

// The notice is on **both** answers, which is what section 2.5 asks for: a
// client that only ever adds keys is still told that a key added here logs in
// as root.
#[tokio::test]
async fn the_root_key_notice_is_on_the_listing_and_on_the_add() {
    let (tree, token) = with_token(ssh_tree(json!([])));
    let (router, _) = test_app(tree);

    let listed =
        body_json(bearer(&router, "GET", "/api/v1/ssh/authorized-keys", &token).await).await;
    assert_eq!(listed["notice"], json!(ROOT_KEY_NOTICE_TEXT));

    let added = bearer_json(
        &router,
        "POST",
        "/api/v1/ssh/authorized-keys",
        &token,
        &json!({ "key": REAL_ED25519_LINE }).to_string(),
    )
    .await;
    assert_eq!(
        body_json(added).await["notice"],
        json!(ROOT_KEY_NOTICE_TEXT)
    );
}

// The add runs the parser the pane runs, and refuses the same lines: both
// surfaces reach `parse_authorized_key`, so a line one accepts is a line the
// other accepts and a line mosd would reject reaches neither.
//
// The two answers differ in shape and not in verdict -- the API's envelope
// carries the parser's own message, the pane re-renders itself around it.

// A key already stored is refused at **409 `key_exists`**.
//
// **This assertion was 422 when M5 shipped it, and the change is a correction
// rather than a weakening.** M5 answered 422 because the duplicate check lives
// inside `validate_authorized_keys` and the only ways out were exporting a
// private constant or matching the validator's words; it declined both and
// took the validator's own message. the error-contract ruling gives
// the contract a third clause -- absent is 404, malformed is 422, **duplicate
// is 409 with a per-collection code** -- and picks the export. This route now
// decides the duplicate itself, before the validator runs, so the status is
// stronger than it was and not looser: 422 was one answer for a malformed key
// and a duplicate alike, and these are now two.
//
// `validate_authorized_keys` still runs on the rewritten list and still
// refuses a duplicate. It has to: the settings file is writable without apid,
// and the reconciler is the boundary. What changed is which of the two answers
// first, not whether the rule exists in one place.
//
// The duplicate is submitted under a different comment, which is the case the
// canonical `key` field exists for: two operators pasting one key under two
// labels must not end up with two entries granting the same access. That is
// also why the route compares the parsed `key` and not the submitted line --
// the identity `validate_authorized_keys` itself uses.
#[tokio::test]
async fn a_duplicate_key_is_409_and_the_stored_list_is_unchanged() {
    let (tree, token) = with_token(ssh_tree(json!([stored_key(REAL_ED25519_LINE)])));
    let (router, fake) = test_app(tree);

    let relabelled = format!("{} someone-else", canonical(REAL_ED25519_LINE));
    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/ssh/authorized-keys",
        &token,
        &json!({ "key": relabelled }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error = envelope(response).await;
    assert_eq!(error["code"], "key_exists");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!(SSH_KEYS_DOT_PATH));
    // The message must not be the validator's -- this route decided the answer
    // and did not recover it from a sentence.
    assert!(
        !error["message"].as_str().unwrap().contains("entry 0"),
        "the refusal echoed the validator's wording: {error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(
        stored_key_list(&fake).await,
        json!([stored_key(REAL_ED25519_LINE)])
    );

    // And a malformed key is still 422, which is the distinction the third
    // clause buys: one status no longer covers two conditions.
    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/ssh/authorized-keys",
        &token,
        &json!({ "key": "ssh-ed25519 not-base64" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["code"], "validation_failed");
}

// The 32-key cap is **409 `key_limit_reached`**, answered from the exported
// bound exactly as the token mint answers its own from `MAX_TOKENS`.
//
// The export is the one the ruling picked, and this is the other thing it
// buys: without a readable bound, a full list reaches the caller either as the
// shared validator's 422 -- indistinguishable from a malformed key -- or as a
// failed write, a 500 about mosd, for a request that was never going to be
// accepted.
#[tokio::test]
async fn a_full_key_list_is_409_and_names_the_bound() {
    let full: Vec<serde_json::Value> = (0..mosd_settings::MAX_KEYS)
        .map(|index| json!({ "key": generated_key_line(index as u8) }))
        .collect();
    let (tree, token) = with_token(ssh_tree(json!(full)));
    let (router, fake) = test_app(tree);

    // A key no stored entry carries, so the duplicate rule above cannot be what
    // answers: the two 409s must be told apart by their code.
    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/ssh/authorized-keys",
        &token,
        &json!({ "key": generated_key_line(mosd_settings::MAX_KEYS as u8) }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error = envelope(response).await;
    assert_eq!(error["code"], "key_limit_reached");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains(&mosd_settings::MAX_KEYS.to_string()),
        "the refusal must name the bound: {error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// Section 2.4's rule on the SSH item route: a well-formed fingerprint that
// matches no key is **404**, and a string that is not a fingerprint at all is
// **422**.
//
// Paired with `the_ssh_pane_answers_422_where_the_api_answers_404`, which
// asserts the HTML surface's deliberately different answer to the first of
// those two conditions.
#[tokio::test]
async fn an_absent_key_fingerprint_is_404_where_the_pane_is_422() {
    let (tree, token) = with_token(ssh_tree(json!([stored_key(REAL_ED25519_LINE)])));
    let (router, fake) = test_app(tree);

    // Well formed -- it is a real fingerprint of a real key -- and no stored
    // entry carries it.
    let response = bearer(
        &router,
        "DELETE",
        &ssh_key_url(REAL_ED25519_SECOND_FINGERPRINT),
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!(SSH_KEYS_DOT_PATH));

    // Not an identifier at all. The last of these is the canonical key text,
    // which the pane accepts as an identifier and this route does not: on a
    // path segment there is one interpretation, and it is the fingerprint.
    for identifier in [
        "SHA256:tooshort",
        "HrgN3GLi6Mop2uSRjgOoxImM8zRkFmgqCKoeGD9QOaM",
        "SHA1:HrgN3GLi6Mop2uSRjgOoxImM8zRkFmgqCKoeGD9QOa",
        &canonical(REAL_ED25519_LINE).replace(' ', "%20"),
    ] {
        let response = bearer(&router, "DELETE", &ssh_key_url(identifier), &token).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{identifier}"
        );
        assert_eq!(
            envelope(response).await["code"],
            "validation_failed",
            "{identifier}"
        );
    }

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(
        stored_key_list(&fake).await,
        json!([stored_key(REAL_ED25519_LINE)])
    );
}

// The HTML half of the split recorded here:
// the pane answers **422** where `DELETE /api/v1/ssh/authorized-keys/
// {fingerprint}` answers **404**, on the same condition.
//
// It is not drift. The pane's identifier is a submitted string that may be a
// fingerprint *or* the exact key text, so a value matching nothing is as
// likely mistyped as absent -- the re-submit-the-form condition 422 means
// there -- and its body is a re-rendered page no consumer reads a status
// from. On the API the identifier is a path segment with one interpretation.
// Paired with `an_absent_key_fingerprint_is_404_where_the_pane_is_422`.

// A fingerprint's base64 alphabet contains `/`, so the identifier of a real
// RSA key is two path segments unless it is percent-encoded. Measured rather
// than assumed: `%2F` is three characters at match time, so the route matches
// one segment, and axum decodes it back to a `/` before the handler sees it.
//
// The gate's predicate has to agree, which is the other half of this: it reads
// the raw path, sees no separator, and hands the request off.
#[tokio::test]
async fn a_fingerprint_carrying_a_slash_is_addressable_percent_encoded() {
    assert!(
        REAL_RSA_FINGERPRINT.contains('/'),
        "this test is about the `/`, and the fixture no longer has one"
    );
    let (tree, token) = with_token(ssh_tree(json!([
        stored_key(REAL_RSA_LINE),
        stored_key(REAL_ED25519_LINE),
    ])));
    let (router, fake) = test_app(tree);

    let response = bearer(
        &router,
        "DELETE",
        &ssh_key_url(REAL_RSA_FINGERPRINT),
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        stored_key_list(&fake).await,
        json!([stored_key(REAL_ED25519_LINE)]),
        "the RSA key and only the RSA key was removed"
    );

    // Unencoded, the same fingerprint is two segments and names no route at
    // all -- which is the reserved subtree's own not-found answer and not this
    // collection's 404.
    let raw = format!("/api/v1/ssh/authorized-keys/{REAL_RSA_FINGERPRINT}");
    // The COOKIE and not the bearer, deliberately: this path is UNDECLARED, so
    // it reaches the gate rather than a route's own extractor, and the gate
    // takes the session and only the session.
    let cookie = login(&router, "hunter2secret").await;
    let response = request(&router, "DELETE", &raw, Some(&cookie), None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(envelope(response).await["code"], "not_found");
}

// The WiFi collection end to end. **It has no pane**, so this is the first
// management surface the list has ever had: it exists in the settings model
// and was reachable only by editing the settings file on STATE.
#[tokio::test]
async fn the_wifi_network_collection_lists_adds_and_removes() {
    let (tree, token) = with_token(wifi_tree(json!([])));
    let (router, fake) = test_app(tree);

    let empty = bearer(&router, "GET", "/api/v1/wifi/client/networks", &token).await;
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(body_json(empty).await, json!([]));

    let added = bearer_json(
        &router,
        "POST",
        "/api/v1/wifi/client/networks",
        &token,
        &json!({ "ssid": "roastery", "psk": "hunter2hunter2", "hidden": true, "priority": 7 })
            .to_string(),
    )
    .await;
    assert_eq!(added.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(added).await,
        json!({ "ssid": "roastery", "psk": REDACTED, "hidden": true, "priority": 7 })
    );
    assert_eq!(fake.set_paths(), vec![WIFI_NETWORKS_DOT_PATH]);
    // The stored value is the real key; only what leaves the device is
    // substituted.
    assert_eq!(
        stored_network_list(&fake).await[0]["psk"],
        json!("hunter2hunter2")
    );

    // An open network: `psk` is absent rather than null, which is the model's
    // own shape.
    let open = bearer_json(
        &router,
        "POST",
        "/api/v1/wifi/client/networks",
        &token,
        &json!({ "ssid": "cafe-guest" }).to_string(),
    )
    .await;
    assert_eq!(open.status(), StatusCode::CREATED);
    assert_eq!(
        body_json(open).await,
        json!({ "ssid": "cafe-guest", "hidden": false, "priority": 0 })
    );

    let removed = bearer(
        &router,
        "DELETE",
        "/api/v1/wifi/client/networks/roastery",
        &token,
    )
    .await;
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let left =
        body_json(bearer(&router, "GET", "/api/v1/wifi/client/networks", &token).await).await;
    assert_eq!(
        left,
        json!([{ "ssid": "cafe-guest", "hidden": false, "priority": 0 }])
    );
}

// A key written through `POST` is redacted on the next `GET`, and the
// redaction is section 2.2's structural one rather than a rule this route
// keeps for itself.
#[tokio::test]
async fn a_posted_psk_is_redacted_on_the_next_read() {
    let (tree, token) = with_token(wifi_tree(json!([])));
    let (router, fake) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/wifi/client/networks",
        &token,
        &json!({ "ssid": "roastery", "psk": "hunter2hunter2" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let listed = bearer(&router, "GET", "/api/v1/wifi/client/networks", &token).await;
    let body = body_string(listed).await;
    assert!(
        !body.contains("hunter2hunter2"),
        "the stored key left the device: {body}"
    );
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed[0]["psk"], json!(REDACTED));

    // The same list read through the settings root is redacted too, which is
    // what makes this one list with one rule and not two surfaces with two.
    let through_settings = bearer(
        &router,
        "GET",
        "/api/v1/settings/wifi.client.networks",
        &token,
    )
    .await;
    assert_eq!(body_json(through_settings).await[0]["psk"], json!(REDACTED));
    assert_eq!(
        stored_network_list(&fake).await[0]["psk"],
        json!("hunter2hunter2")
    );
}

// The round trip that would destroy a working key: read the list, change one
// field, post the entry back. What comes back carries `"<redacted>"` in
// `psk`, and storing it would replace the key with ten literal characters.
//
// The sentinel is checked before the body is even read as a network, which is
// the ordering the scalar write route landed and the reason it landed it: the
// caller is answered about the thing it actually got wrong.
#[tokio::test]
async fn posting_a_redacted_psk_back_is_refused_and_the_stored_key_survives() {
    let (tree, token) = with_token(wifi_tree(json!([
        { "ssid": "roastery", "psk": "hunter2hunter2", "hidden": false, "priority": 0 },
    ])));
    let (router, fake) = test_app(tree);

    // Exactly what a client that read the collection holds.
    let listed =
        body_json(bearer(&router, "GET", "/api/v1/wifi/client/networks", &token).await).await;
    let mut edited = listed[0].clone();
    edited["ssid"] = json!("roastery-5g");
    edited["hidden"] = json!(true);
    assert_eq!(edited["psk"], json!(REDACTED));

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/wifi/client/networks",
        &token,
        &edited.to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!(WIFI_NETWORKS_DOT_PATH));

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(
        stored_network_list(&fake).await,
        json!([{ "ssid": "roastery", "psk": "hunter2hunter2", "hidden": false, "priority": 0 }]),
        "the real key must survive the refusal"
    );
}

// The SSID is this collection's identity, so a second entry under one SSID is
// refused rather than appended: with two, a `DELETE` would have no answer to
// which of them it names.
//
// 409 and not 422, for the reason the token mint's `token_limit_reached` is a
// 409: the body is well formed and nothing about it is wrong, and what refuses
// it is the collection's current state.
#[tokio::test]
async fn a_second_network_under_one_ssid_is_refused() {
    let (tree, token) = with_token(wifi_tree(json!([
        { "ssid": "roastery", "psk": "hunter2hunter2", "hidden": false, "priority": 0 },
    ])));
    let (router, fake) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/wifi/client/networks",
        &token,
        &json!({ "ssid": "roastery", "psk": "adifferentkey" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error = envelope(response).await;
    assert_eq!(error["code"], "ssid_exists");
    assert_eq!(error["path"], json!(WIFI_NETWORKS_DOT_PATH));
    assert!(
        !body_string(bearer(&router, "GET", "/api/v1/wifi/client/networks", &token).await)
            .await
            .contains("adifferentkey")
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// A body that is not a network is 422, and the validator is the settings
// model's own deserializer -- which is what mosd's `Settings::set` validates
// with, so a body this route accepts is one the store accepts.
#[tokio::test]
async fn a_body_that_is_not_a_network_is_422() {
    let (tree, token) = with_token(wifi_tree(json!([])));
    let (router, fake) = test_app(tree);

    for body in [
        // No `ssid`: the one field with no default.
        r#"{"psk":"hunter2hunter2"}"#,
        // A field the model does not carry; `deny_unknown_fields` is what
        // catches a typo before it becomes a silently ignored setting.
        r#"{"ssid":"roastery","hiden":true}"#,
        // Wrong types.
        r#"{"ssid":7}"#,
        r#"{"ssid":"roastery","priority":"high"}"#,
        // An array where an object belongs.
        r#"[{"ssid":"roastery"}]"#,
    ] {
        let response = bearer_json(
            &router,
            "POST",
            "/api/v1/wifi/client/networks",
            &token,
            body,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        assert_eq!(
            envelope(response).await["code"],
            "validation_failed",
            "{body}"
        );
    }

    // Not JSON at all is 400 and not 422: the request could not be read, which
    // is a different failure from one that was read and refused.
    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/wifi/client/networks",
        &token,
        "{not json",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(envelope(response).await["code"], "request_invalid");

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// Section 2.4's rule on the WiFi item route, with its 422 half **vacant**.
//
// An SSID has no grammar -- every non-empty single path segment spells a
// possible one -- so there is no malformed identifier to answer 422 about and
// everything absent is 404. That is the rule applied, not an exception to it.
//
// **There is no paired pane test here, and the absence is the point.** The
// SSH keys have one (`the_ssh_pane_answers_422_where_the_api_answers_404`)
// because both surfaces exist and answer differently on purpose. This
// collection has no HTML pane at all, so there is no form-path behaviour for
// it to agree or disagree with.
#[tokio::test]
async fn an_absent_ssid_is_404_and_this_collection_has_no_pane_to_disagree_with() {
    let (tree, token) = with_token(wifi_tree(json!([
        { "ssid": "roastery", "psk": "hunter2hunter2", "hidden": false, "priority": 0 },
    ])));
    let (router, fake) = test_app(tree);

    for ssid in ["cafe-guest", "roastery-5g", "%20", "SHA256:not-an-ssid"] {
        let response = bearer(
            &router,
            "DELETE",
            &format!("/api/v1/wifi/client/networks/{ssid}"),
            &token,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{ssid}");
        let error = envelope(response).await;
        assert_eq!(error["code"], "settings_not_found", "{ssid}");
        assert_eq!(error["source"], "apid", "{ssid}");
        assert_eq!(error["path"], json!(WIFI_NETWORKS_DOT_PATH), "{ssid}");
    }

    // No pane serves this list -- the assertion behind the paragraph above, so
    // it cannot quietly stop being true. Read out of the router's own source,
    // for the reason `every_mutating_route_is_covered_by_the_authentication_tests`
    // reads it: a hand-listed set of paths to probe would describe the panes
    // somebody remembered.
    let html_wifi_routes: Vec<&str> = include_str!("routes.rs")
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(".route(\"/wifi"))
        .collect();
    assert!(
        html_wifi_routes.is_empty(),
        "this collection now has a pane, so the paragraph above is stale and a paired 422/404 test is owed: {html_wifi_routes:?}"
    );

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// Both collections take a bearer token **and** a session cookie, and neither
// takes nothing.
//
// the bearer-only ruling is about the token routes
// specifically -- the credential factory -- and not about new routes in
// general, so these are dual-credential exactly as the shipped reads are.

// The document describes the served surface: a client reading only
// `openapi.json` has to learn both collections, every outcome each route has,
// and that the SSH listing carries a `notice`.
#[test]
fn the_openapi_document_covers_the_two_collections() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    for (path, method, statuses) in [
        (
            "/api/v1/ssh/authorized-keys",
            "get",
            vec!["200", "401", "405", "500", "503"],
        ),
        (
            "/api/v1/ssh/authorized-keys",
            "post",
            vec!["201", "400", "401", "405", "409", "422", "500", "503"],
        ),
        (
            "/api/v1/ssh/authorized-keys/{fingerprint}",
            "delete",
            vec!["204", "401", "404", "405", "422", "500", "503"],
        ),
        (
            "/api/v1/wifi/client/networks",
            "get",
            vec!["200", "401", "405", "500", "503"],
        ),
        (
            "/api/v1/wifi/client/networks",
            "post",
            vec!["201", "400", "401", "405", "409", "422", "500", "503"],
        ),
        (
            "/api/v1/wifi/client/networks/{ssid}",
            "delete",
            vec!["204", "401", "404", "405", "500", "503"],
        ),
    ] {
        let operation = &document["paths"][path][method];
        assert!(operation.is_object(), "{method} {path} is undocumented");
        for status in statuses {
            assert!(
                operation["responses"][status].is_object(),
                "{method} {path} must document {status}: {operation}"
            );
        }
    }

    // The notice is a documented member and not an undeclared extra, on both
    // answers that carry it.
    let schemas = &document["components"]["schemas"];
    assert!(schemas["AuthorizedKeyList"]["properties"]["notice"].is_object());
    assert!(schemas["AddedAuthorizedKey"]["properties"]["notice"].is_object());
    // The WiFi item route documents no 422: its identifier has no grammar, so
    // there is no malformed spelling to answer one for.
    assert!(
        document["paths"]["/api/v1/wifi/client/networks/{ssid}"]["delete"]["responses"]["422"]
            .is_null()
    );
}

// The documented WiFi entry is the settings model's own shape.
//
// The route deserializes into `mosd_settings::WifiNetwork` and answers a
// redacted serialization of it, so `WifiNetworkEntry` is a description of
// that type rather than a second definition of it. Without this, a field
// added to the model would be served and undocumented.
#[test]
fn the_wifi_schema_matches_the_settings_model() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");
    let mut documented: Vec<String> =
        document["components"]["schemas"]["WifiNetworkEntry"]["properties"]
            .as_object()
            .expect("WifiNetworkEntry is an object schema")
            .keys()
            .cloned()
            .collect();

    // Every field present: `psk` is the one the model omits when it is absent.
    let model = serde_json::to_value(mosd_settings::WifiNetwork {
        ssid: "roastery".to_string(),
        psk: Some("hunter2hunter2".to_string()),
        hidden: true,
        priority: 7,
    })
    .expect("a network serializes");
    let mut fields: Vec<String> = model
        .as_object()
        .expect("a network is an object")
        .keys()
        .cloned()
        .collect();
    fields.sort();
    documented.sort();
    assert_eq!(
        documented, fields,
        "the documented WiFi entry has drifted from `mosd_settings::WifiNetwork`"
    );
}

// The network cluster typed, the
// WireGuard peer collection, and the rotate-key 404.

// The API path of one interface.
const NETWORK_MAP_PATH: &str = "/api/v1/network";

// The `network` dot-path every envelope about the whole map names.
const NETWORK_DOT_PATH: &str = "network";

fn iface_url(iface: &str) -> String {
    format!("{NETWORK_MAP_PATH}/{}", urlencode(iface))
}

fn peers_url(iface: &str) -> String {
    format!("{}/peers", iface_url(iface))
}

fn peer_url(iface: &str, public_key: &str) -> String {
    format!("{}/{}", peers_url(iface), urlencode(public_key))
}

// The stored map, read back through the fake.
async fn stored_network_map(fake: &FakeSettings) -> serde_json::Value {
    fake.get_settings(NETWORK_DOT_PATH).await.unwrap()
}

// A syntactically valid X25519 public key whose base64 spelling carries a
// `/`, which the standard alphabet really does contain.
//
// Its private half was never generated -- it is 32 copies of one byte -- so
// it authorises nothing anywhere.
const SLASHED_PEER_KEY: &str = "Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8/Pz8=";

// The four relational rules, each with its own route-level test, because the
// whole reason this cluster is typed rather than a dot-path passthrough is
// that these rules exist and a passthrough runs none of them.
//
// Each asserts the same three things: **422**, the rule's own sentence in the
// message, and the stored tree unchanged. The last one is what separates this
// from the shipped passthrough, which answers 204 and leaves the device's
// networking broken with the only evidence in a later state read.
#[tokio::test]
async fn a_vlan_parent_that_is_not_declared_is_422_and_writes_nothing() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("vlan9"),
        &token,
        &json!({ "kind": "vlan", "dhcp": true, "vlan": { "parent": "eth9", "id": 9 } }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_api_headers(&response, "vlan parent");
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!("network.vlan9"));
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("has VLAN parent \"eth9\", which is not a declared network entry"),
        "{error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);
}

// Rule two. This is the exact submission the contract
// names as the concrete failure a bare passthrough produces: a bridge naming
// a port that does not exist, which a `PUT` to
// `/api/v1/settings/network.br9` would have answered 204 to.
#[tokio::test]
async fn a_bridge_port_that_is_not_declared_is_422_and_writes_nothing() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("br9"),
        &token,
        &json!({ "kind": "bridge", "dhcp": true, "bridge": { "ports": ["eth9"] } }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("has bridge port \"eth9\", which is not a declared network entry"),
        "{error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);
}

// Rule three, and it is the one no check confined to the entry being written
// could ever see: what is refused here is an edit to `eth1`, and what refuses
// it is `br0`, a different entry that claims `eth1` as a port.
#[tokio::test]
async fn a_bridge_port_that_carries_addressing_is_422_and_writes_nothing() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("eth1"),
        &token,
        &json!({ "dhcp": true }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert!(
        error["message"].as_str().unwrap().contains(
            "network.eth1 is a port of bridge br0 and must not carry addressing of its own"
        ),
        "{error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);
}

// Rule four. `br0` already claims `eth1`; a second bridge claiming it is a
// race between two `Bridge=` lines for one file, and it is refused.
#[tokio::test]
async fn a_port_claimed_by_two_bridges_is_422_and_writes_nothing() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("br1"),
        &token,
        &json!({ "kind": "bridge", "dhcp": true, "bridge": { "ports": ["eth1"] } }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("is claimed as a port by both bridge"),
        "{error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);
}

// The happy path: declare an interface that did not exist, replace one that
// did, and remove one.
#[tokio::test]
async fn the_interface_route_declares_replaces_and_removes() {
    let (router, fake, _cookie, token) = kinds_app().await;

    // Declared: `eth2` is not in the stored map, and a `PUT` creates it.
    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("eth2"),
        &token,
        &json!({ "dhcp": false, "static": { "address": "10.0.0.9/24", "dns": ["1.1.1.1"] } })
            .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(header_value(&response, CACHE_CONTROL), "no-store");
    assert!(body_string(response).await.is_empty());
    assert_eq!(fake.set_paths(), vec!["network.eth2".to_string()]);
    assert_eq!(
        fake.get_settings("network.eth2").await.unwrap(),
        json!({ "dhcp": false, "static": { "address": "10.0.0.9/24", "dns": ["1.1.1.1"] } })
    );

    // Replaced whole: the second body has no `static`, and the stored entry
    // has none afterwards. A `PUT` is the entry, not a patch of it.
    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("eth2"),
        &token,
        &json!({ "dhcp": true }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        fake.get_settings("network.eth2").await.unwrap(),
        json!({ "dhcp": true })
    );

    // Removed: the whole map is rewritten without it, because the dot-path
    // syntax has no delete.
    let response = bearer(&router, "DELETE", &iface_url("eth2"), &token).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        fake.set_paths().last().map(String::as_str),
        Some(NETWORK_DOT_PATH)
    );
    let map = stored_network_map(&fake).await;
    assert!(map.get("eth2").is_none(), "{map}");
    // And nothing else went with it.
    for kept in ["eth0", "eth1", "eth0.100", "br0", "wg0"] {
        assert!(map.get(kept).is_some(), "{kept} was dropped: {map}");
    }
}

// A removal is re-validated against the map it leaves behind, which is the
// half a delete-by-dot-path could not do at all.
#[tokio::test]
async fn removing_a_port_a_bridge_still_lists_is_refused() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    let response = bearer(&router, "DELETE", &iface_url("eth1"), &token).await;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("has bridge port \"eth1\", which is not a declared network entry"),
        "{error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);

    // Removing the bridge first makes the port removable, which is the order
    // the message asks for.
    assert_eq!(
        bearer(&router, "DELETE", &iface_url("br0"), &token)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        bearer(&router, "DELETE", &iface_url("eth1"), &token)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

// Section 2.4's rule on the interface item route: absent is 404, malformed is
// 422, and they are not the same answer.
//
// There is no 404 on the `PUT`, deliberately: that route's job is to create
// the entry it names, so an absent one is not an absent resource.
#[tokio::test]
async fn an_absent_interface_is_404_and_a_malformed_name_is_422() {
    let (router, fake, _cookie, token) = kinds_app().await;

    let response = bearer(&router, "DELETE", &iface_url("eth9"), &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_api_headers(&response, "absent interface");
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!(NETWORK_DOT_PATH));

    // Not a name any interface could have: sixteen characters is one past
    // `IFNAMSIZ` minus the terminator, and `/` is not in the charset (it
    // reaches the route percent-encoded, so it is one segment).
    for bad in ["waytoolongiface016", "bad%2Fname"] {
        for method in ["PUT", "DELETE"] {
            let path = format!("{NETWORK_MAP_PATH}/{bad}");
            let response = if method == "PUT" {
                bearer_json(&router, "PUT", &path, &token, "{\"dhcp\":true}").await
            } else {
                bearer(&router, method, &path, &token).await
            };
            assert_eq!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{method} {bad}"
            );
            assert_eq!(
                envelope(response).await["code"],
                "validation_failed",
                "{method} {bad}"
            );
        }
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The whole map, replaced in one request and validated as one tree.
//
// This is what section 2.3 says the typed route gives back in exchange for
// refusing the passthrough: atomic whole-list replacement, which the
// passthrough had, **and** the relational validation, which it did not. The
// second half of this test is the case the item route cannot express at all —
// a bridge and its port declared together, where sending the bridge first
// would be refused.
#[tokio::test]
async fn the_whole_map_put_replaces_atomically_and_validates_relationally() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    // Refused as one tree: `br9` names a port that this very body does not
    // declare either.
    let response = bearer_json(
        &router,
        "PUT",
        NETWORK_MAP_PATH,
        &token,
        &json!({
            "eth0": { "dhcp": true },
            "br9": { "kind": "bridge", "dhcp": true, "bridge": { "ports": ["eth7"] } },
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["code"], "validation_failed");
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);

    // Accepted as one tree: the same bridge, with its port declared in the
    // same body. Neither entry is legal without the other.
    let response = bearer_json(
        &router,
        "PUT",
        NETWORK_MAP_PATH,
        &token,
        &json!({
            "eth7": { "dhcp": false },
            "br9": { "kind": "bridge", "dhcp": true, "bridge": { "ports": ["eth7"] } },
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(fake.set_paths(), vec![NETWORK_DOT_PATH.to_string()]);
    // Replaced and not merged: every entry the old map had is gone.
    let map = stored_network_map(&fake).await;
    assert_eq!(
        map.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["br9", "eth7"],
        "{map}"
    );

    // A key that is not an interface name is 422, and it names the key.
    let response = bearer_json(
        &router,
        "PUT",
        NETWORK_MAP_PATH,
        &token,
        &json!({ "waytoolongiface016": { "dhcp": true } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["code"], "validation_failed");

    // A body that is not a map of interfaces at all is 422; a body that is not
    // JSON is 400.
    for (body, status) in [
        ("[]", StatusCode::UNPROCESSABLE_ENTITY),
        (
            "{\"eth0\":{\"nosuchfield\":1}}",
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("{", StatusCode::BAD_REQUEST),
    ] {
        let response = bearer_json(&router, "PUT", NETWORK_MAP_PATH, &token, body).await;
        assert_eq!(response.status(), status, "{body}");
    }
}

// Both typed write paths run the
// wizard's CIDR rule, on the entries the *request* carries.
//
// RED-first for the pinned gap: both
// refused bodies below were answered `204` and written before this milestone.
//
// The condition is the wizard's and is not widened here, so the two accepted
// arms at the end are as much of the rule as the two refusals: an address is
// examined only when `dhcp` is off and the field is non-empty.
#[tokio::test]
async fn the_typed_network_writes_refuse_an_address_that_is_not_a_cidr() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    // The item route: an address the kernel cannot parse, on one entry.
    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("eth0"),
        &token,
        &json!({ "dhcp": false, "static": { "address": "192.168.1.10" } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_api_headers(
        &response,
        "an item write carrying an address that is not a CIDR",
    );
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert_eq!(error["source"], "apid");
    assert_eq!(error["path"], json!("network.eth0"));
    assert_eq!(
        error["message"],
        json!("Static address must be IPv4 CIDR notation, e.g. 192.168.1.10/24."),
        "the message is the wizard's own, so the two surfaces do not disagree"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);

    // The map route: one bad entry refuses the whole body, and the envelope
    // names that entry rather than the map, because that is what failed.
    let response = bearer_json(
        &router,
        "PUT",
        NETWORK_MAP_PATH,
        &token,
        &json!({
            "eth0": { "dhcp": true },
            "eth1": { "dhcp": false, "static": { "address": "10.0.0.5/33" } },
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error = envelope(response).await;
    assert_eq!(error["code"], "validation_failed");
    assert_eq!(error["path"], json!("network.eth1"));
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("IPv4 CIDR notation"),
        "{error}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);

    // The two arms the condition does not reach: DHCP on with a junk address
    // left in the block, and DHCP off with no `static` at all -- which is a
    // bridge port, an interface with no addressing rather than an error.
    for (iface, body) in [
        (
            "eth0",
            json!({ "dhcp": true, "static": { "address": "nonsense" } }),
        ),
        ("eth1", json!({ "dhcp": false })),
    ] {
        let response =
            bearer_json(&router, "PUT", &iface_url(iface), &token, &body.to_string()).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{iface} {body}");
    }
}

// A dotted interface name round-trips through the quoted path segment, so the
// daemon sees one key and not two (M6 acceptance).
#[tokio::test]
async fn a_dotted_interface_name_round_trips_through_the_quoted_path_segment() {
    let (router, fake, _cookie, token) = kinds_app().await;

    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("eth0.100"),
        &token,
        &json!({ "kind": "vlan", "dhcp": true, "vlan": { "parent": "eth0", "id": 100 } })
            .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(fake.set_paths(), vec![r#"network."eth0.100""#.to_string()]);

    // And the envelope quotes it too, because that is the dot-path an operator
    // would type at the settings route.
    let (router, _, _cookie, token) = kinds_app().await;
    let response = bearer_json(
        &router,
        "PUT",
        &iface_url("wg.9"),
        &token,
        &json!({ "kind": "vlan", "dhcp": true, "vlan": { "parent": "nope", "id": 1 } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(envelope(response).await["path"], json!(r#"network."wg.9""#));
}

// M4's refusal, verified rather than duplicated: a raw settings write under
// `network` is 409 and names the typed routes this milestone added.
#[tokio::test]
async fn the_settings_passthrough_under_network_is_409_and_names_the_typed_route() {
    let (router, fake, _cookie, token) = kinds_app().await;

    for path in [
        "/api/v1/settings/network",
        "/api/v1/settings/network.br0",
        "/api/v1/settings/network.br0.bridge.ports",
    ] {
        let response = bearer_json(&router, "PUT", path, &token, "{\"dhcp\":true}").await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
        let error = envelope(response).await;
        assert_eq!(error["code"], "settings_read_only", "{path}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("/api/v1/network"),
            "{path} did not name the typed route: {error}"
        );
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The peer collection end to end: list, add, remove.
#[tokio::test]
async fn the_peer_collection_lists_adds_and_removes() {
    let (router, fake, _cookie, token) = kinds_app().await;

    let response = bearer(&router, "GET", &peers_url("wg0"), &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "peer listing");
    let listed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["publicKey"], json!(PEER_KEY));
    assert_eq!(listed[0]["allowedIps"], json!(["10.8.0.0/24"]));

    let response = bearer_json(
        &router,
        "POST",
        &peers_url("wg0"),
        &token,
        &json!({
            "publicKey": OTHER_PEER_KEY,
            "allowedIps": ["10.8.1.0/24"],
            "endpoint": "vpn2.example.net:51820",
            "persistentKeepalive": 25,
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let echoed: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(echoed["publicKey"], json!(OTHER_PEER_KEY));
    assert_eq!(echoed["persistentKeepalive"], json!(25));
    // Only the peer list was written, not the whole entry.
    assert_eq!(
        fake.set_paths(),
        vec!["network.wg0.wireguard.peers".to_string()]
    );

    let response = bearer(&router, "DELETE", &peer_url("wg0", OTHER_PEER_KEY), &token).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let peers = fake
        .get_settings("network.wg0.wireguard.peers")
        .await
        .unwrap();
    assert_eq!(peers.as_array().unwrap().len(), 1, "{peers}");
    assert_eq!(peers[0]["publicKey"], json!(PEER_KEY));
}

// the sweep, discharged: the typed route
// answers **404 before anything is written** for the interface the pane
// silently creates a broken entry for.
//
// Paired with `the_pane_peer_add_writes_a_broken_entry_for_an_undeclared_interface`,
// which runs the pane's behaviour and confirms the finding was right. The
// split between them is the whole reason this milestone typed the route
// instead of adding a guard to the old one.
#[tokio::test]
async fn the_api_peer_add_refuses_an_undeclared_interface_where_the_pane_writes_one() {
    let (router, fake, _cookie, token) = kinds_app().await;

    let response = bearer_json(
        &router,
        "POST",
        &peers_url("wg9"),
        &token,
        &json!({ "publicKey": PEER_KEY }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_api_headers(&response, "peer add on an undeclared interface");
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["path"], json!(NETWORK_DOT_PATH));
    // Nothing was written, which is the half the pane gets wrong: no write at
    // all, and therefore no `network.wg9` of the default kind.
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert!(
        stored_network_map(&fake).await.get("wg9").is_none(),
        "an undeclared interface was created"
    );

    // The same 404 on the other two operations of the collection.
    assert_eq!(
        bearer(&router, "GET", &peers_url("wg9"), &token)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bearer(&router, "DELETE", &peer_url("wg9", PEER_KEY), &token)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// A declared entry of the wrong kind is **422** and not 404, which is the
// same split mosd's rotate-key now makes: the URL names a real entry, and
// what is wrong is the argument.
#[tokio::test]
async fn peers_on_an_interface_that_is_not_a_tunnel_are_422() {
    let (router, fake, _cookie, token) = kinds_app().await;

    for (method, path) in [
        ("GET", peers_url("eth0")),
        ("DELETE", peer_url("eth0", PEER_KEY)),
    ] {
        let response = bearer(&router, method, &path, &token).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
        let error = envelope(response).await;
        assert_eq!(error["code"], "validation_failed", "{path}");
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("is not a WireGuard interface"),
            "{error}"
        );
    }

    let response = bearer_json(
        &router,
        "POST",
        &peers_url("eth0"),
        &token,
        &json!({ "publicKey": PEER_KEY }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// A duplicate public key is **409 `peer_exists`**, following the WiFi
// collection's `ssid_exists` and not the SSH collection's 422.
//
// The reason is the identity: the public key is this collection's `DELETE`
// path segment, so two entries under one key would leave no answer to which
// one a `DELETE` names -- the argument the WiFi route's 409 makes about an
// SSID. The SSH 422 is the *shared validator's* own message, inherited rather
// than decided, and no validator on either side of the bus refuses a
// duplicate peer. This is flagged as an open contract
// question: the ratified rule covers absent and malformed and says nothing
// about duplicate.
#[tokio::test]
async fn a_duplicate_peer_is_409_and_writes_nothing() {
    let (router, fake, _cookie, token) = kinds_app().await;
    let before = stored_network_map(&fake).await;

    let response = bearer_json(
        &router,
        "POST",
        &peers_url("wg0"),
        &token,
        &json!({ "publicKey": PEER_KEY, "allowedIps": ["10.9.0.0/24"] }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error = envelope(response).await;
    assert_eq!(error["code"], "peer_exists");
    assert_eq!(error["path"], json!("network.wg0.wireguard.peers"));
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(stored_network_map(&fake).await, before);
}

// Section 2.4's rule on the peer item route, with both halves live.
#[tokio::test]
async fn an_absent_peer_key_is_404_and_a_malformed_one_is_422() {
    let (router, fake, _cookie, token) = kinds_app().await;

    // Well formed -- it is 32 bytes of base64 -- and no stored peer has it.
    let response = bearer(&router, "DELETE", &peer_url("wg0", OTHER_PEER_KEY), &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error = envelope(response).await;
    assert_eq!(error["code"], "settings_not_found");
    assert_eq!(error["path"], json!("network.wg0.wireguard.peers"));

    // Not a public key at all, and could never be one.
    for identifier in ["nope", "AAAA", &"A".repeat(44), &"!".repeat(44)] {
        let response = bearer(&router, "DELETE", &peer_url("wg0", identifier), &token).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{identifier}"
        );
        assert_eq!(
            envelope(response).await["code"],
            "validation_failed",
            "{identifier}"
        );
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The API answers **404** where the pane answers 422, on the same condition.
//
// Paired with `an_absent_peer_key_is_404_and_a_malformed_one_is_422` above
// and kept for the reason given about the
// SSH pane: a form's body is a re-rendered page no consumer reads a status
// from, and its message asks for a re-submit.

// A public key carrying a `/` is addressable, percent-encoded.
//
// The base64 alphabet a WireGuard key uses is the standard one, not the
// URL-safe variant, so a real key can contain `/` and `+`. Sent as `%2F` it
// is three characters at match time, so the router still matches one segment
// and axum decodes it back before the handler sees it. Sent unencoded it is
// two segments and reaches the reserved subtree's own not-found, which is a
// different answer from this collection's 404.
#[tokio::test]
async fn a_peer_key_carrying_a_slash_is_addressable_percent_encoded() {
    let mut tree = kinds_tree("hunter2secret");
    tree["network"]["wg0"]["wireguard"]["peers"] = json!([{ "publicKey": SLASHED_PEER_KEY }]);
    let (tree, token) = with_token(tree);
    let (router, fake) = test_app(tree);
    let cookie = login(&router, "hunter2secret").await;

    assert!(SLASHED_PEER_KEY.contains('/'), "the fixture must carry one");
    let response = bearer(
        &router,
        "DELETE",
        &peer_url("wg0", SLASHED_PEER_KEY),
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        fake.get_settings("network.wg0.wireguard.peers")
            .await
            .unwrap(),
        json!([])
    );

    // Unencoded, the same key is two segments and is not this route. The
    // COOKIE and not the bearer: an undeclared path reaches the gate, and the
    // gate takes the session and only the session.
    let response = request(
        &router,
        "DELETE",
        &format!("{}/{SLASHED_PEER_KEY}", peers_url("wg0")),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(envelope(response).await["code"], "not_found");
}

// A peer the reconciler would refuse is refused here first, and the refusal
// never echoes the key -- the property the reconciler's index-only rule
// exists for, now that the message reaches an HTTP client.
#[tokio::test]
async fn the_peer_add_runs_the_same_validator_the_reconciler_runs() {
    const PASTED_SECRET: &str = "OOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOO";
    let (router, fake, _cookie, token) = kinds_app().await;

    for (body, fragment) in [
        (
            json!({ "publicKey": PASTED_SECRET }),
            "is not a WireGuard key",
        ),
        (
            json!({ "publicKey": OTHER_PEER_KEY, "allowedIps": ["not-a-cidr"] }),
            "is not an IP address or CIDR",
        ),
        (
            json!({ "publicKey": OTHER_PEER_KEY, "endpoint": "no-port" }),
            "is not host:port",
        ),
    ] {
        let response = bearer_json(
            &router,
            "POST",
            &peers_url("wg0"),
            &token,
            &body.to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}"
        );
        let error = envelope(response).await;
        assert!(
            error["message"].as_str().unwrap().contains(fragment),
            "{body} did not explain itself: {error}"
        );
        assert!(
            !error["message"].as_str().unwrap().contains(PASTED_SECRET),
            "the refusal echoed the value: {error}"
        );
    }

    // A body that is not a peer at all is 422; one that is not JSON is 400.
    for (body, status) in [
        ("{\"nosuchfield\":1}", StatusCode::UNPROCESSABLE_ENTITY),
        ("{", StatusCode::BAD_REQUEST),
    ] {
        let response = bearer_json(&router, "POST", &peers_url("wg0"), &token, body).await;
        assert_eq!(response.status(), status, "{body}");
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// An entry this build cannot read stops every route in the cluster, rather
// than being silently dropped.
//
// The pane can afford to skip one and name it in the page; these routes
// cannot. Two of them rewrite the whole map, so a dropped entry is a deleted
// interface, and all of them validate relationally, so an invisible entry
// turns a legal bridge port into a 422.
#[tokio::test]
async fn an_unreadable_network_entry_stops_every_route_in_the_cluster() {
    let mut tree = kinds_tree("hunter2secret");
    tree["network"]["mangled"] = json!("not an interface");
    let (tree, token) = with_token(tree);
    let (router, fake) = test_app(tree);

    for (method, path) in [
        ("PUT", iface_url("eth0")),
        ("DELETE", iface_url("eth0")),
        ("GET", peers_url("wg0")),
    ] {
        let response = if method == "PUT" {
            bearer_json(&router, "PUT", &path, &token, "{\"dhcp\":true}").await
        } else {
            bearer(&router, method, &path, &token).await
        };
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{method} {path}"
        );
        let error = envelope(response).await;
        assert_eq!(error["code"], "settings_invalid", "{method} {path}");
        assert!(
            error["message"].as_str().unwrap().contains("mangled"),
            "the envelope must name the entry: {error}"
        );
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());

    // The whole-map `PUT` is the exception, and deliberately: it does not read
    // the stored map at all, because the map it sends is the map that ends up
    // stored. It is also the only way out of this state through the API.
    let response = bearer_json(
        &router,
        "PUT",
        NETWORK_MAP_PATH,
        &token,
        &json!({ "eth0": { "dhcp": true } }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

// Amendment 1's reading, on M6's four routes: a bearer **or** a cookie, and
// section 2.4's envelope at 401 with neither -- never the gate's redirect.

// The gate hands off exactly what the router serves under this prefix, and
// nothing else.
//
// The two precedents this cluster's predicate applies, asserted rather than
// asserted-about: a **trailing** empty identifier is the collection path with
// a slash and reaches the reservation, and a `{iface}` in the **middle** may
// be empty because axum really matches zero characters there.
#[tokio::test]
async fn the_network_paths_the_router_does_not_serve_reach_the_reservation() {
    let (router, _, cookie, _token) = kinds_app().await;

    for (method, path) in [
        ("GET", "/api/v1/network/"),
        ("DELETE", "/api/v1/network/"),
        ("DELETE", "/api/v1/network/wg0/peers/"),
        ("GET", "/api/v1/network/wg0/peers/extra/deep"),
        ("GET", "/api/v1/network/wg0/notpeers"),
    ] {
        let response = request(&router, method, path, Some(&cookie), None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(
            envelope(response).await["code"],
            "not_found",
            "{method} {path}"
        );
    }

    // The empty interface in the middle IS a route, so an unauthenticated call
    // gets section 2.4's envelope and not the gate's redirect -- the same
    // property the rotate action already has.
    let (fresh, _) = test_app(kinds_tree("hunter2secret"));
    let response = request(&fresh, "GET", "/api/v1/network//peers", None, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response.headers().get(LOCATION), None);
    assert_eq!(envelope(response).await["code"], "not_authenticated");
}

// The document describes every operation this milestone adds, with every
// outcome each has: a client reading only `openapi.json` has to learn them.

// The documented interface schema against `mosd_settings::IfaceSettings`
// itself, field for field, so a field added to the model cannot go
// undocumented here.
//
// Four schemas and not one, because the model is four structs; each is
// compared against a fully-populated instance, since every optional field is
// `skip_serializing_if` and an absent one would make the comparison vacuous.
#[test]
fn the_network_schema_matches_the_settings_model() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    let peer = mosd_settings::WireguardPeer {
        public_key: PEER_KEY.to_string(),
        allowed_ips: vec!["10.8.0.0/24".to_string()],
        endpoint: Some("vpn.example.net:51820".to_string()),
        persistent_keepalive: Some(25),
    };
    let iface = mosd_settings::IfaceSettings {
        kind: mosd_settings::IfaceKind::Wireguard,
        dhcp: false,
        static_: Some(mosd_settings::StaticConfig {
            address: "10.8.0.2/24".to_string(),
            gateway: Some("10.8.0.1".to_string()),
            dns: vec!["1.1.1.1".to_string()],
        }),
        vlan: Some(mosd_settings::VlanConfig {
            parent: "eth0".to_string(),
            id: 100,
        }),
        bridge: Some(mosd_settings::BridgeConfig {
            ports: vec!["eth1".to_string()],
        }),
        wireguard: Some(mosd_settings::WireguardConfig {
            listen_port: Some(51820),
            peers: vec![peer.clone()],
        }),
    };

    for (schema, model) in [
        ("NetworkInterface", serde_json::to_value(&iface).unwrap()),
        (
            "StaticAddressing",
            serde_json::to_value(iface.static_.clone().unwrap()).unwrap(),
        ),
        (
            "VlanParameters",
            serde_json::to_value(iface.vlan.clone().unwrap()).unwrap(),
        ),
        (
            "BridgeParameters",
            serde_json::to_value(iface.bridge.clone().unwrap()).unwrap(),
        ),
        (
            "WireguardParameters",
            serde_json::to_value(iface.wireguard.clone().unwrap()).unwrap(),
        ),
        ("WireguardPeerEntry", serde_json::to_value(&peer).unwrap()),
    ] {
        let mut documented: Vec<String> = document["components"]["schemas"][schema]["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{schema} is an object schema"))
            .keys()
            .cloned()
            .collect();
        let mut fields: Vec<String> = model
            .as_object()
            .unwrap_or_else(|| panic!("{schema}'s model is an object"))
            .keys()
            .cloned()
            .collect();
        documented.sort();
        fields.sort();
        assert_eq!(
            documented, fields,
            "the documented {schema} has drifted from the settings model"
        );
    }
}

// The lifted pre-shared key bound, run by the WiFi route for the first time.
//
// M5 could not check it: the bound lived
// inside a private function of the `mosd` binary crate's station reconciler,
// so a key outside IEEE 802.11i's range was accepted, stored, and refused
// later by the renderer with the error visible only in live state. M6 lifted
// it into `mosd-settings` and the reconciler calls the lifted copy, so this is
// the same rule and not a second one.
#[tokio::test]
async fn a_psk_outside_the_lifted_bounds_is_refused_by_the_wifi_route() {
    let (tree, token) = with_token(wifi_tree(json!([])));
    let (router, fake) = test_app(tree);

    for psk in ["short07", &"x".repeat(64)] {
        let response = bearer_json(
            &router,
            "POST",
            "/api/v1/wifi/client/networks",
            &token,
            &json!({ "ssid": "roastery", "psk": psk }).to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{} characters",
            psk.len()
        );
        let error = envelope(response).await;
        assert_eq!(error["code"], "validation_failed");
        assert!(
            error["message"].as_str().unwrap().contains("8 to 63"),
            "{error}"
        );
        // The message never names the length observed: a length is a fact
        // about a secret, and this string reaches an HTTP client.
        assert!(
            !error["message"].as_str().unwrap().contains(psk),
            "the refusal echoed the key: {error}"
        );
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());

    // And the two admissible shapes still store: a passphrase in range, and a
    // 64-digit hex PMK, which the bound does not apply to.
    for (ssid, psk) in [("roastery", "hunter2hunter2"), ("lab", &"a".repeat(64))] {
        let response = bearer_json(
            &router,
            "POST",
            "/api/v1/wifi/client/networks",
            &token,
            &json!({ "ssid": ssid, "psk": psk }).to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "{ssid}");
    }
}

// The quotable half of the lifted bound, run by the WiFi route.
//
// The measured gap this closes: a key
// carrying a quote or a backslash passed `validate_wifi_psk`, was stored, and
// was then refused at render time by the station renderer's own `is_quotable`
// — accepted at the route and dead on the reconciler, with the error visible
// only in live state. The predicate now lives beside the model and the
// renderer calls it, so this is the same rule and not a second one.
#[tokio::test]
async fn a_psk_the_renderer_cannot_quote_is_refused_by_the_wifi_route() {
    let (tree, token) = with_token(wifi_tree(json!([])));
    let (router, fake) = test_app(tree);

    for psk in [
        "has\"quote1",
        "has\\backslash",
        "two\nlines1",
        "tab\there1",
        "caf\u{e9}-latte",
    ] {
        let response = bearer_json(
            &router,
            "POST",
            "/api/v1/wifi/client/networks",
            &token,
            &json!({ "ssid": "roastery", "psk": psk }).to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{psk:?}"
        );
        let error = envelope(response).await;
        assert_eq!(error["code"], "validation_failed", "{psk:?}");
        assert_eq!(error["source"], "apid", "{psk:?}");
        assert_eq!(error["path"], json!(WIFI_NETWORKS_DOT_PATH), "{psk:?}");
        // The refusal never echoes the key.
        assert!(
            !error["message"].as_str().unwrap().contains(psk),
            "the refusal echoed the key: {error}"
        );
    }
    // Nothing reached mosd: a key refused here is never stored, which is the
    // whole point of moving the refusal to the write surface.
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The collection identifier contract's third clause, held across **every**
// collection at once: a duplicate is **409**, with a per-collection code.
//
// One test over all three rather than three that happen to agree. The clause
// exists because the two shipped answers had diverged — M5's SSH route
// answered 422 and its WiFi route answered 409 for the same class of condition
// — and what stops a fourth collection from picking a fourth answer is a test
// that fails when one of them drifts, not three tests that would each keep
// passing on their own. `docs/design/api.md` section 2.4 carries the clause.
//
// The codes are asserted individually and are deliberately **not** one shared
// constant: the clause says *a per-collection code*, so a client can tell
// which collection refused it without parsing a path.
#[tokio::test]
async fn every_collection_answers_409_for_a_duplicate() {
    let mut tree = kinds_tree("hunter2secret");
    tree["access"]["ssh"] =
        ssh_tree(json!([stored_key(REAL_ED25519_LINE)]))["access"]["ssh"].clone();
    tree["wifi"] = wifi_tree(json!([
        { "ssid": "roastery", "psk": "hunter2hunter2", "hidden": false, "priority": 0 },
    ]))["wifi"]
        .clone();
    let (tree, token) = with_token(tree);
    let (router, fake) = test_app(tree);

    for (path, body, code) in [
        (
            "/api/v1/ssh/authorized-keys",
            json!({ "key": format!("{} relabelled", canonical(REAL_ED25519_LINE)) }),
            "key_exists",
        ),
        (
            "/api/v1/wifi/client/networks",
            json!({ "ssid": "roastery", "psk": "adifferentkey" }),
            "ssid_exists",
        ),
        (
            "/api/v1/network/wg0/peers",
            json!({ "publicKey": PEER_KEY }),
            "peer_exists",
        ),
    ] {
        let response = bearer_json(&router, "POST", path, &token, &body.to_string()).await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
        assert_api_headers(&response, path);
        let error = envelope(response).await;
        assert_eq!(error["code"], code, "{path}");
        assert_eq!(error["source"], "apid", "{path}");
    }
    // Not one of them wrote: a refused duplicate leaves the collection alone.
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// The three actions. No state to `GET` and no idempotency to
// promise, so every assertion below is about the status code, the call that
// did or did not reach mosd, and what the response body does not contain.

const REBOOT_PATH: &str = "/api/v1/actions/reboot";
const POWEROFF_PATH: &str = "/api/v1/actions/poweroff";
const TRANSIENT_PATH: &str = "/api/v1/actions/transient-root-password";

// **202 and not 204**, on both verbs, with the bus call reaching mosd after
// the response was built.
//
// The status is the milestone's first acceptance criterion: the call is
// spawned on a detached task, so the response goes out before the machine goes
// down and whether the action completed is not knowable over the connection
// that asked. `await_power_calls` is what proves the dispatch is detached
// rather than awaited — a handler that awaited the call would already have the
// entry when the response arrived, and would have no reason to answer 202.
//
// Asserted against the form path in the same test rather than trusted from the
// design: both surfaces answer the same code because both go through one
// dispatch, and a change to one of them fails here.

// **The confirmation token is not carried over, and the form still demands
// it.**
//
// `PowerAction::confirm_token` and `TRANSIENT_CONFIRM_TOKEN` are compile-time
// constants, not secrets and not per-session; they stop a mis-click on a
// rendered page, and there is no mis-click on a `POST` a script constructed.
// So the API takes none — an empty body is enough — while the form path is
// unchanged. The asymmetry is deliberate, and this test is what stops a later
// reading from "harmonising" either half into the other.

// The transient password reaches mosd, is written into no setting, and is
// nowhere in the tree afterwards — the API half of the property the form path
// already holds.
#[tokio::test]
async fn the_transient_password_route_sets_it_and_writes_no_setting() {
    const PASSWORD: &str = "correct horse battery";

    let (tree, token) = with_token(ssh_tree(json!([])));
    let (router, fake) = test_app(tree);

    let response = bearer_json(
        &router,
        "POST",
        TRANSIENT_PATH,
        &token,
        &json!({ "password": PASSWORD }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(header_value(&response, CACHE_CONTROL), "no-store");
    let accepted = body_json(response).await;
    assert!(accepted["taskId"].as_str().is_some(), "{accepted}");
    assert_eq!(fake.transient_password_calls(), 1);

    assert!(
        fake.set_paths().is_empty(),
        "a transient password must write no setting, got {:?}",
        fake.set_paths()
    );
    let tree = fake.get_settings("").await.unwrap().to_string();
    assert!(
        !tree.contains(PASSWORD),
        "the password must not appear in the settings tree"
    );
}

// **The same byte bounds as the form path, because it is the same function.**
//
// `validate_transient_password` is called by both surfaces, so this asserts
// the boundaries in both directions and then asserts the form path agrees on
// the very same inputs. Two copies of the rule could disagree; one cannot, and
// this is the test that would fail if a second copy ever appeared.
//
// 72 is bcrypt's limit, which is why the upper bound exists at all: a longer
// password would be silently shortened to its first 72 bytes.

// **A rejected password never appears in the response.**
//
// The validator's three messages state the bound, the reason for the bound, or
// the forbidden bytes, and none of them interpolates the password — so the
// message can be passed through verbatim. This asserts the property the
// milestone requires rather than the mechanism that provides it: the whole
// response, headers and body, is searched for the value that was sent.
//
// The passwords below are distinctive strings rather than runs of one
// character, so a substring match cannot pass by accident.
#[tokio::test]
async fn a_rejected_transient_password_is_never_echoed() {
    let (tree, token) = with_token(ssh_tree(json!([])));
    let (router, _) = test_app(tree);

    for password in [
        "shortpw",
        "quagga-vestibule-marzipan-cornice-thimble-quixotic-basalt-lantern-ferrule",
        "quagga\nvestibule",
    ] {
        let response = bearer_json(
            &router,
            "POST",
            TRANSIENT_PATH,
            &token,
            &json!({ "password": password }).to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{password}"
        );
        let headers = format!("{:?}", response.headers());
        let body = body_string(response).await;
        assert!(!body.contains(password), "the body echoed it: {body}");
        assert!(!headers.contains(password), "a header echoed it: {headers}");
        // Nor a distinctive fragment of it: a truncated echo is still an echo.
        for fragment in ["quagga", "shortpw"] {
            if password.contains(fragment) {
                assert!(
                    !body.contains(fragment),
                    "the body echoed `{fragment}`: {body}"
                );
            }
        }
        // It is still a usable §2.4 envelope: the caller has to learn what the
        // rule was without being told what it sent. Every one of the
        // validator's messages names the subject and the rule and nothing else,
        // which is exactly why the message can be passed through verbatim.
        let error: serde_json::Value = serde_json::from_str(&body).expect("§2.4 envelope");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("Password must"),
            "{password}: {message}"
        );
    }
}

// A body that is not this shape is §2.4's `request_invalid` at 400, and the
// rejection text describes the shape rather than the value — so a malformed
// body carrying a password does not put it in the response either.
#[tokio::test]
async fn a_malformed_transient_password_body_is_refused_at_400() {
    let (tree, token) = with_token(ssh_tree(json!([])));
    let (router, fake) = test_app(tree);

    for body in [
        "not json at all",
        r#"{"password": 7}"#,
        r#"{"passphrase": "hunter2secret"}"#,
        "{}",
    ] {
        let response = bearer_json(&router, "POST", TRANSIENT_PATH, &token, body).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
        assert_api_headers(&response, body);
        let error = envelope(response).await;
        assert_eq!(error["code"], "request_invalid", "{body}");
        assert_eq!(error["source"], "apid", "{body}");
    }
    assert_eq!(fake.transient_password_calls(), 0);
    assert!(fake.set_paths().is_empty());
}

// All three take a bearer token, per Amendment 1's dual-credential reading:
// `ApiSession`, so a cookie works too and the bearer is what a script uses.

// No credential, no action. The 401 is §2.4's envelope and the machine stays
// up: this is the one route family where a missing check is unrecoverable.
#[tokio::test]
async fn an_unauthenticated_action_post_is_refused_and_does_not_act() {
    for path in [REBOOT_PATH, POWEROFF_PATH, TRANSIENT_PATH] {
        let (router, fake) = test_app(ssh_tree(json!([])));
        let response = post_json(&router, path, r#"{"password":"hunter2secret"}"#, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_api_headers(&response, path);
        assert_eq!(
            envelope(response).await["code"],
            "not_authenticated",
            "{path}"
        );
        // Two calls' worth of deadline, then assert nothing arrived.
        assert!(
            fake.await_power_calls(1).await.is_empty(),
            "{path} acted without a credential"
        );
        assert_eq!(fake.transient_password_calls(), 0, "{path}");
    }
}

// A failed transient-password call is §2.4's envelope with **no `path`
// member**: the route writes no setting, so there is no dot-path at fault.
//
// This is the clause that made `bus_api_error` take an `Option`. Asserted
// because the alternative — passing a plausible-looking dot-path such as
// `access.ssh` — would name something that was not at fault, and an empty
// string would put `"path": ""` on the wire.
#[tokio::test]
async fn a_failed_transient_password_names_no_dot_path() {
    let (router, token) = failing_app(Some("org.freedesktop.DBus.Error.Failed")).await;

    let response = bearer_json(
        &router,
        "POST",
        TRANSIENT_PATH,
        &token,
        &json!({ "password": "hunter2secret" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_api_headers(&response, TRANSIENT_PATH);
    let error = envelope(response).await;
    assert_eq!(error["code"], "mosd_failed");
    assert_eq!(error["source"], "mosd");
    assert!(error.get("path").is_none(), "{error}");

    // A route that does name one still names it: the member is optional, not
    // removed. Same fixture, same failure, same classifier — the only
    // difference is that this one has a dot-path at fault.
    let response = bearer_json(
        &router,
        "PUT",
        "/api/v1/settings/hostname",
        &token,
        r#""mos""#,
    )
    .await;
    assert_eq!(envelope(response).await["path"], "hostname");
}

// The document describes all three, each `POST`-only: a documented `GET`
// would be a contract for a route that does not exist, and on these three
// paths it would be a contract to power the appliance off by following a link.
#[test]
fn the_openapi_document_covers_the_three_actions() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");

    for (path, statuses) in [
        (REBOOT_PATH, ["202", "401", "405"].as_slice()),
        (POWEROFF_PATH, ["202", "401", "405"].as_slice()),
        (
            TRANSIENT_PATH,
            ["202", "400", "401", "405", "422", "500", "503", "504"].as_slice(),
        ),
    ] {
        let route = &document["paths"][path];
        assert!(route["post"].is_object(), "{path} is missing its POST");
        for status in statuses {
            assert!(
                route["post"]["responses"][status].is_object(),
                "{path} is missing its {status}: {document}"
            );
        }
        for method in ["get", "head", "put", "delete", "patch"] {
            assert!(
                route[method].is_null(),
                "{path} must declare no {method}: {document}"
            );
        }
    }

    // The one request body among the three carries exactly one member.
    let properties =
        &document["components"]["schemas"]["TransientRootPasswordRequest"]["properties"];
    assert_eq!(
        properties.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["password"],
        "{document}"
    );
}

// `POST /api/v1/setup`, the one unauthenticated write, and the one behaviour
// this campaign fixed rather than documented.

const SETUP_PATH: &str = "/api/v1/setup";
// A body that configures everything the route accepts.
fn full_setup_body() -> String {
    json!({
        "password": "first-boot-pw",
        "hostname": "appliance",
        "network": { "eth0": { "kind": "physical", "dhcp": true } },
    })
    .to_string()
}

// The happy path: one unauthenticated call configures the device and hands
// back a credential that works.
//
// The token is asserted by *using* it and not by its shape alone. §3.2's
// reason for minting here at all is that a caller who drove setup over the API
// wants API access, and a token that authenticates nothing would satisfy the
// letter of that and none of it.

// The write order, asserted as an order and not as a set.
//
// `access.webAdmin` is third. It is the write that takes the device out of
// setup mode, so everything that can fail before it fails with the wizard
// still reachable — which is what section 2.3 item (ii) says the form path
// does not do.

// **The milestone's own assertion.** One partial failure, driven through both
// surfaces, with opposite outcomes.
//
// The hostname write fails on the bus. On the wizard the password is already
// written when it does, so the device leaves setup mode with no hostname —
// section 2.3 item (ii)'s measured failure, reproduced here rather than
// relayed. On the API route nothing has been written yet, so the device is
// still in setup mode and the wizard that was meant to configure it still
// answers.
//
// The wizard is not fixed, deliberately: what is left there is a bus failure
// between two writes, and closing it needs a transactional multi-path write
// on the bus, which is a mosd change outside this crate.

// The acceptance criterion for M8, on the rule the
// route cannot reach any other way.
//
// A bridge naming a port that is not a declared entry is one of the four
// relational rules, and it is checkable only against the whole candidate tree.
// The answer is 422 with the rule's own sentence, `access.webAdmin` is
// unwritten, and the device is still in setup mode.

// A relational rule that is legal only because of an entry the device already
// has: the candidate tree is the stored map plus the submission, not the
// submission alone.
//
// `br0` names `eth1` as a port. `eth1` is not in the body; it is already in
// the tree. Validating the submitted entries on their own would refuse this,
// and refusing it would make a bridge unbuildable through setup.
#[tokio::test]
async fn the_setup_network_tree_is_merged_with_the_stored_one_before_it_is_judged() {
    let (router, fake) = test_app(json!({
        "hostname": "mos",
        "network": { "eth1": { "kind": "physical", "dhcp": false } },
        "access": {},
    }));

    let body = json!({
        "password": "first-boot-pw",
        "network": { "br0": { "kind": "bridge", "dhcp": false, "bridge": { "ports": ["eth1"] } } },
    })
    .to_string();
    let response = post_json(&router, SETUP_PATH, &body, None).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    // Both entries are there: the submitted one was added and the stored one
    // was not dropped by the whole-map write.
    let network = fake.get_settings("network").await.unwrap();
    let mut names: Vec<&str> = network
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["br0", "eth1"], "{network}");
}

// 409 on the form path's own condition, and both surfaces are asserted on one
// tree so neither can drift into answering about a different one.
//
// The condition is `access.webAdmin` carrying a hash, which is what
// `password_hash` tests; it is not "a session exists" and not "the tree is
// non-empty".

// Every validation failure, each with nothing written.
//
// The password floor answers **422** where the wizard answers 400, and that is
// the contract `docs/design/api.md` section 2.3's row states for this route:
// *"422 on any validation failure"*. The wizard's 400 is left alone; the two
// surfaces are asserted side by side so the difference is recorded rather than
// discovered.

// A rejected password is never echoed, in the body or the headers.
//
// The same property M7's transient-password route has, and for the same
// reason: a refusal that repeats the secret puts it in every proxy log
// between the caller and the device.
#[tokio::test]
async fn a_rejected_setup_password_is_never_echoed() {
    let (router, _) = test_app(unconfigured_tree());
    let response = post_json(
        &router,
        SETUP_PATH,
        &json!({ "password": "sh0rt!" }).to_string(),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let headers = format!("{:?}", response.headers());
    let body = body_string(response).await;
    for haystack in [&headers, &body] {
        assert!(!haystack.contains("sh0rt!"), "{haystack}");
        // Not a fragment of it either.
        assert!(!haystack.contains("sh0rt"), "{haystack}");
    }
}

// A body that is not JSON at all is 400, and it names no dot-path.
//
// §2.4's `path` is the settings dot-path at fault. This route writes three
// subtrees, and a body that never parsed is not about any of them, so the
// member is absent rather than naming one arbitrarily.

// The route takes no credential, and it is the only one under the prefix that
// does not.
//
// Both halves matter. A route that demanded one could never be used on a
// factory-fresh device, and a second route that did not would be an
// unauthenticated write nobody decided to add. Asserted in setup mode, which
// is the only mode where the question is live.
#[tokio::test]
async fn the_setup_route_is_the_one_api_route_that_takes_no_credential() {
    let (router, _) = test_app(unconfigured_tree());

    // Every other write route under the prefix, unauthenticated, in setup
    // mode: §2.4's 401 envelope and never a redirect.
    for (method, path, body) in [
        ("PUT", "/api/v1/settings/hostname", "\"appliance\""),
        ("POST", "/api/v1/actions/reboot", ""),
        ("POST", "/api/v1/tokens", "{\"name\":\"x\"}"),
        ("PUT", "/api/v1/network", "{}"),
        ("POST", "/api/v1/ssh/authorized-keys", "{}"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = send(&router, request).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        assert_eq!(
            response.headers().get(LOCATION),
            None,
            "{method} {path} answered a redirect, which a script reads as success"
        );
        assert_eq!(
            envelope(response).await["code"],
            "not_authenticated",
            "{method} {path}"
        );
    }

    // And the setup route, with nothing at all: no cookie, no bearer.
    let response = post_json(&router, SETUP_PATH, &full_setup_body(), None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
}

// The route records the CLAIM, and the password reaches no line of the trail.
//
// The event names the transition and not the route, because §6's trail says
// what happened to the device: this route and a provisioning document produce
// the same claim, so a name like `setup` would have described the door rather
// than what went through it. The token is checked out of the log for the
// reason the password is — it is a credential, and the response body is the
// only place it may appear.
#[tokio::test]
async fn the_api_setup_route_records_the_claim_and_no_credential() {
    let dir = TempDir::new().unwrap();
    let fake = Arc::new(FakeSettings::new(unconfigured_tree()));
    let router = app(AppState::new(fake, SIGNING_KEY).with_persistence(dir.path()));

    let response = post_json(&router, SETUP_PATH, &full_setup_body(), None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let token =
        serde_json::from_str::<serde_json::Value>(&body_string(response).await).unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();

    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [("claim".to_string(), "completed".to_string())]
    );
    let raw = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
    for secret in ["first-boot-pw", token.as_str()] {
        assert!(!raw.contains(secret), "the trail must not carry {secret}");
    }
}
