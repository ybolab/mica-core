//! The PLAN-052 routes: the system-information surface, the telemetry and
//! observed-network reads, and the diagnostic snapshot lifecycle, driven
//! through the router against the fake backend and a temporary store.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::header::{ALLOW, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tempfile::TempDir;

use super::{
    SIGNING_KEY, assert_api_headers, bearer, bearer_json, body_json, body_string, envelope,
    failing_app, header_value, secret_tree, send, with_token,
};
use crate::diagnostics;
use crate::routes::{AppState, app};
use crate::settings_api::FakeSettings;

/// A router whose snapshot store lives in a temporary directory, so no test
/// can touch `/mos/diagnostics`.
fn diagnostics_app(tree: serde_json::Value) -> (Router, Arc<FakeSettings>, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = Arc::new(FakeSettings::new(tree));
    let state = AppState::new(fake.clone(), SIGNING_KEY)
        .with_diagnostics_root(dir.path().join("diagnostics"));
    (app(state), fake, dir)
}

async fn unauthenticated(router: &Router, method: &str, path: &str) -> StatusCode {
    let response = send(
        router,
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let status = response.status();
    assert_eq!(
        envelope(response).await["code"],
        "not_authenticated",
        "{method} {path}"
    );
    status
}

// The system-information surface: authenticated, read-only, micad's
// assembly passed through rather than re-derived here, and the live
// denylist applied on the way out like every other read.
#[tokio::test]
async fn the_system_info_route_answers_mosds_surface_read_only() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake, _dir) = diagnostics_app(tree);
    fake.set_system_info(json!({
        "machineId": { "available": true, "id": "0123456789abcdef0123456789abcdef" },
        "board": { "available": false, "detail": "no device-tree model" },
        "system": { "available": true, "version": "0.1.0+git00b674ec0ffe-1", "package": "micad",
                    "gitStamp": { "available": true, "commit": "00b674ec0ffe", "dirty": false, "consistent": true } },
        "packages": { "available": true, "count": 1, "entries": [{ "name": "micad", "version": "0.1.0+git00b674ec0ffe-1", "architecture": "arm64", "mos": true }] },
        "slot": { "available": true, "booted": "rootfs.0" },
        "uptime": { "available": true, "seconds": 99 },
        // A field the live denylist covers must not pass through this
        // route either.
        "hash": "must-not-ship",
    }));

    let response = bearer(&router, "GET", "/api/v1/system/info", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/system/info");
    let info = body_json(response).await;
    assert_eq!(info["machineId"]["id"], "0123456789abcdef0123456789abcdef");
    assert_eq!(info["board"]["available"], false);
    assert_eq!(info["system"]["gitStamp"]["commit"], "00b674ec0ffe");
    assert_eq!(info["packages"]["entries"][0]["name"], "micad");
    assert_eq!(info["slot"]["booted"], "rootfs.0");
    assert_eq!(info["uptime"]["seconds"], 99);
    assert_eq!(info["hash"], crate::redact::REDACTED);

    assert_eq!(
        unauthenticated(&router, "GET", "/api/v1/system/info").await,
        StatusCode::UNAUTHORIZED
    );
    for method in ["PUT", "POST", "DELETE", "PATCH"] {
        let response = bearer_json(&router, method, "/api/v1/system/info", &token, "{}").await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
        assert_eq!(envelope(response).await["code"], "method_not_allowed");
    }
}

// Telemetry and the observed network: the same read-only pass-through, and
// the observed route is its own surface with none of the desired map in it.
#[tokio::test]
async fn the_telemetry_and_network_status_routes_are_read_only_observations() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake, _dir) = diagnostics_app(tree);
    fake.set_telemetry(json!({
        "thermal": { "available": true, "zones": [{ "sensor": "thermal_zone0", "label": "soc", "milliCelsius": 51000 }], "hwmon": [] },
        "watchdog": { "available": false, "detail": "none" },
        "reset": { "available": true, "reason": "watchdog", "detail": "d", "evidence": {} },
    }));
    fake.set_observed_network(json!({
        "interfaces": { "available": true, "count": 1, "entries": [{ "name": "eth0", "link": { "carrier": false, "operationalState": "no-carrier" }, "addresses": [], "dhcp": { "available": false, "detail": "no client" }, "dns": [] }] },
        "defaultRoutes": { "available": true, "count": 0, "entries": [] },
        "dns": { "available": false, "detail": "resolved unreachable", "linkServers": [] },
        "wifi": { "available": false, "detail": "no wireless interface on this board" },
        "capabilities": { "wifi": { "supported": false, "interfaces": [] }, "bluetooth": { "supported": false, "adapters": [] }, "cellular": { "supported": false, "interfaces": [], "detail": "not selected" } },
    }));

    let response = bearer(&router, "GET", "/api/v1/system/telemetry", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/system/telemetry");
    let telemetry = body_json(response).await;
    assert_eq!(telemetry["thermal"]["zones"][0]["milliCelsius"], 51000);
    assert_eq!(telemetry["watchdog"]["available"], false);
    assert_eq!(telemetry["reset"]["reason"], "watchdog");

    let response = bearer(&router, "GET", "/api/v1/network/status", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/network/status");
    let observed = body_json(response).await;
    // A configured-but-dead port reads as exactly that, and the desired map
    // is not in this answer under any name.
    assert_eq!(
        observed["interfaces"]["entries"][0]["link"]["carrier"],
        false
    );
    assert_eq!(observed["defaultRoutes"]["count"], 0);
    assert_eq!(observed["capabilities"]["cellular"]["supported"], false);
    assert!(observed.get("configured").is_none());
    assert!(observed.get("configuredCount").is_none());
    // And the desired map's own route still carries its `configured`
    // member: two surfaces, two shapes.
    let overview = body_json(bearer(&router, "GET", "/api/v1/network", &token).await).await;
    assert!(overview.get("configured").is_some());

    // The static `status` segment wins over `{iface}`: a write there is a
    // method refusal from the status route, not an interface named
    // `status` being written.
    let response = bearer_json(&router, "PUT", "/api/v1/network/status", &token, "{}").await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(header_value(&response, ALLOW), "GET,HEAD");
    assert!(fake.set_paths().is_empty(), "the settings tree was written");

    for path in ["/api/v1/system/telemetry", "/api/v1/network/status"] {
        assert_eq!(
            unauthenticated(&router, "GET", path).await,
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
}

// micad failures on the three reads travel under §2.4's classification like
// every other bus read.
#[tokio::test]
async fn the_three_reads_classify_a_mosd_failure() {
    let (router, token) = failing_app(None).await;
    for path in [
        "/api/v1/system/info",
        "/api/v1/system/telemetry",
        "/api/v1/network/status",
    ] {
        let response = bearer(&router, "GET", path, &token).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(
            envelope(response).await["code"],
            "micad_unreachable",
            "{path}"
        );
    }
}

// The snapshot lifecycle end to end: an empty store, a collection, the
// listing, the export with its attachment name, the redaction visible in
// the exported bytes, the explicit delete, and every not-found.
#[tokio::test]
async fn a_snapshot_is_collected_listed_exported_and_deleted() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake, dir) = diagnostics_app(tree);
    fake.set_observed_network(json!({
        "interfaces": { "available": true, "count": 1, "entries": [{
            "name": "wlan0", "hardwareAddress": "aa:bb:cc:dd:ee:01",
            "link": { "carrier": true }, "addresses": [{ "family": "ipv4", "address": "192.0.2.10", "prefixLength": 24 }],
            "dhcp": { "available": false, "detail": "static" }, "dns": ["192.0.2.1"],
            "wifi": { "interface": "wlan0", "available": true, "state": "COMPLETED", "associated": true, "ssid": "HomeNet-marker", "bssid": "aa:bb:cc:dd:ee:ff", "psk": "psk-marker" },
        }] },
        "defaultRoutes": { "available": true, "count": 1, "entries": [{ "family": "ipv4", "gateway": "192.0.2.1", "interface": "wlan0" }] },
        "dns": { "available": true, "linkServers": ["192.0.2.1"], "probe": { "available": true, "name": "n", "reachable": true, "result": "resolved" } },
        "wifi": { "available": true, "associations": [] },
        "capabilities": { "wifi": { "supported": true, "interfaces": ["wlan0"] }, "bluetooth": { "supported": false, "adapters": [] }, "cellular": { "supported": false, "interfaces": [] } },
    }));
    fake.set_failure_evidence(json!({
        "journal": { "available": true, "scope": "current boot", "priority": "warning", "lineCount": 2, "sourceLines": 2, "sourceBytes": 80, "truncated": false,
                     "bounds": { "maxLines": 400, "maxBytes": 131072, "maxLineBytes": 1024 },
                     "lines": ["systemd[1]: Failed to start x.service.", "wpa_supplicant[4]: psk=journal-psk-marker"] },
        "units": { "available": true, "count": 0, "truncated": false, "entries": [] },
    }));

    // Empty store: an empty list and the bounds, not an error.
    let response = bearer(&router, "GET", "/api/v1/diagnostics/snapshots", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, "/api/v1/diagnostics/snapshots");
    let listing = body_json(response).await;
    assert_eq!(listing["snapshots"], json!([]));
    assert_eq!(
        listing["retention"]["maxSnapshots"],
        diagnostics::MAX_SNAPSHOTS
    );
    assert_eq!(
        listing["retention"]["maxTotalBytes"],
        diagnostics::MAX_TOTAL_BYTES
    );
    assert_eq!(
        listing["retention"]["maxSnapshotBytes"],
        diagnostics::MAX_SNAPSHOT_BYTES
    );
    assert_eq!(
        listing["retention"]["schemaVersion"],
        diagnostics::SCHEMA_VERSION
    );
    assert!(
        !dir.path().join("diagnostics").exists(),
        "listing created the root"
    );

    // Collect one.
    let response = bearer_json(&router, "POST", "/api/v1/diagnostics/snapshots", &token, "").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_api_headers(&response, "POST /api/v1/diagnostics/snapshots");
    let collected = body_json(response).await;
    assert_eq!(collected["snapshot"]["id"], 1);
    assert_eq!(
        collected["snapshot"]["schemaVersion"],
        diagnostics::SCHEMA_VERSION
    );
    assert_eq!(
        collected["snapshot"]["machineId"],
        "0123456789abcdef0123456789abcdef"
    );
    assert!(
        collected["snapshot"]["bytes"]
            .as_u64()
            .is_some_and(|b| b > 0)
    );
    for section in [
        "system",
        "telemetry",
        "failures",
        "storage",
        "time",
        "network",
        "state",
    ] {
        assert_eq!(collected["sections"][section], "ok", "{section}");
    }
    assert!(collected["redactedFields"].as_u64().is_some_and(|n| n >= 3));

    // Listed.
    let listing =
        body_json(bearer(&router, "GET", "/api/v1/diagnostics/snapshots", &token).await).await;
    assert_eq!(listing["snapshots"].as_array().unwrap().len(), 1);
    assert_eq!(listing["snapshots"][0]["id"], 1);
    assert!(
        listing["snapshots"][0]["collectedAt"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z'))
    );

    // Exported: the stored bytes as an attachment, never cached, redacted.
    let response = bearer(&router, "GET", "/api/v1/diagnostics/snapshots/1", &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, CONTENT_TYPE), "application/json");
    assert_eq!(header_value(&response, CACHE_CONTROL), "no-store");
    assert_eq!(
        header_value(&response, CONTENT_DISPOSITION),
        "attachment; filename=\"mos-diagnostics-01234567-1.json\""
    );
    let text = body_string(response).await;
    for marker in [
        "HomeNet-marker",
        "aa:bb:cc:dd:ee:ff",
        "aa:bb:cc:dd:ee:01",
        "psk-marker",
        "journal-psk-marker",
    ] {
        assert!(
            !text.contains(marker),
            "{marker} shipped in the export:\n{text}"
        );
    }
    let snapshot: serde_json::Value = serde_json::from_str(&text).expect("the export is JSON");
    assert_eq!(snapshot["schemaVersion"], diagnostics::SCHEMA_VERSION);
    assert_eq!(
        snapshot["network"]["interfaces"]["entries"][0]["hardwareAddress"],
        crate::redact::REDACTED
    );
    assert_eq!(
        snapshot["network"]["interfaces"]["entries"][0]["wifi"]["ssid"],
        crate::redact::REDACTED
    );
    assert_eq!(
        snapshot["network"]["interfaces"]["entries"][0]["addresses"][0]["address"],
        "192.0.2.10"
    );
    assert_eq!(
        snapshot["network"]["defaultRoutes"]["entries"][0]["gateway"],
        "192.0.2.1"
    );
    assert_eq!(
        snapshot["journal"]["lines"][0],
        "systemd[1]: Failed to start x.service."
    );
    assert_eq!(snapshot["journal"]["lines"][1], diagnostics::REDACTED_LINE);
    assert_eq!(
        snapshot["system"]["machineId"]["id"],
        "0123456789abcdef0123456789abcdef"
    );
    assert_eq!(snapshot["time"]["status"], "synchronized");
    assert_eq!(snapshot["collection"]["sections"]["network"], "ok");
    assert_eq!(
        snapshot["collection"]["redaction"]["schemaVersion"],
        diagnostics::REDACTION_SCHEMA_VERSION
    );

    // Not found, in both spellings of "no such snapshot".
    for id in ["2", "0", "abc", "1.json", "-1", "18446744073709551616"] {
        let response = bearer(
            &router,
            "GET",
            &format!("/api/v1/diagnostics/snapshots/{id}"),
            &token,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{id}");
        assert_eq!(
            envelope(response).await["code"],
            "snapshot_not_found",
            "{id}"
        );
    }

    // Deleted explicitly; a second delete is a 404, and so is the export.
    let response = bearer(&router, "DELETE", "/api/v1/diagnostics/snapshots/1", &token).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = bearer(&router, "DELETE", "/api/v1/diagnostics/snapshots/1", &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(envelope(response).await["code"], "snapshot_not_found");
    let response = bearer(&router, "GET", "/api/v1/diagnostics/snapshots/1", &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let listing =
        body_json(bearer(&router, "GET", "/api/v1/diagnostics/snapshots", &token).await).await;
    assert_eq!(listing["snapshots"], json!([]));

    // Every route is authenticated.
    for (method, path) in [
        ("GET", "/api/v1/diagnostics/snapshots"),
        ("POST", "/api/v1/diagnostics/snapshots"),
        ("GET", "/api/v1/diagnostics/snapshots/1"),
        ("DELETE", "/api/v1/diagnostics/snapshots/1"),
    ] {
        assert_eq!(
            unauthenticated(&router, method, path).await,
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
    // And the collection takes no other verb.
    for method in ["PUT", "PATCH", "DELETE"] {
        let response = bearer_json(
            &router,
            method,
            "/api/v1/diagnostics/snapshots",
            &token,
            "{}",
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
}

// Collections serialise: while one runs, a second request is refused with
// 409 rather than queued, and the first still publishes.
#[tokio::test]
async fn a_concurrent_collection_is_refused_not_queued() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake, _dir) = diagnostics_app(tree);
    fake.set_diagnostic_delay(Duration::from_millis(150));
    let first = bearer_json(&router, "POST", "/api/v1/diagnostics/snapshots", &token, "");
    let second = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        bearer_json(&router, "POST", "/api/v1/diagnostics/snapshots", &token, "").await
    };
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.status(), StatusCode::CREATED);
    assert_eq!(second.status(), StatusCode::CONFLICT);
    assert_eq!(envelope(second).await["code"], "diagnostics_busy");
    let listing =
        body_json(bearer(&router, "GET", "/api/v1/diagnostics/snapshots", &token).await).await;
    assert_eq!(listing["snapshots"].as_array().unwrap().len(), 1);
}

// Sources that do not answer do not stop the snapshot: every section is
// recorded absent with the reason, the snapshot is still published, and the
// request returns within the bound rather than hanging on micad.
#[tokio::test]
async fn a_snapshot_is_still_published_when_no_source_answers() {
    let (tree, token) = with_token(secret_tree("hunter2secret"));
    let (router, fake, _dir) = diagnostics_app(tree);
    fake.set_diagnostic_delay(Duration::from_secs(30));
    let started = std::time::Instant::now();
    let response = bearer_json(&router, "POST", "/api/v1/diagnostics/snapshots", &token, "").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert!(
        started.elapsed() <= diagnostics::COLLECTION_DEADLINE + Duration::from_secs(5),
        "the request outlived the deadline: {:?}",
        started.elapsed()
    );
    let collected = body_json(response).await;
    for (section, status) in collected["sections"].as_object().unwrap() {
        assert_eq!(status, "timeout", "{section}");
    }
    let text =
        body_string(bearer(&router, "GET", "/api/v1/diagnostics/snapshots/1", &token).await).await;
    let snapshot: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(snapshot["system"]["available"], false);
    assert_eq!(snapshot["storage"]["available"], false);
    assert!(snapshot["storage"]["detail"].is_string());
}

// The document describes the served surface: a client reading only
// `openapi.json` has to learn the three reads, the collection, the item
// routes and every outcome each has.
#[test]
fn the_openapi_document_covers_the_diagnostics_routes() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");
    for path in [
        "/api/v1/system/info",
        "/api/v1/system/telemetry",
        "/api/v1/network/status",
    ] {
        let get = &document["paths"][path]["get"]["responses"];
        for status in ["200", "401", "500", "503", "504", "405"] {
            assert!(get[status].is_object(), "{path} lacks {status}: {get}");
        }
        assert!(
            document["paths"][path].get("put").is_none(),
            "{path} declares a write"
        );
    }
    let collection = &document["paths"]["/api/v1/diagnostics/snapshots"];
    assert!(collection["get"]["responses"]["200"].is_object());
    for status in ["201", "401", "403", "409", "500"] {
        assert!(
            collection["post"]["responses"][status].is_object(),
            "{status}"
        );
    }
    let item = &document["paths"]["/api/v1/diagnostics/snapshots/{id}"];
    for status in ["200", "401", "404"] {
        assert!(item["get"]["responses"][status].is_object(), "GET {status}");
    }
    for status in ["204", "401", "403", "404"] {
        assert!(
            item["delete"]["responses"][status].is_object(),
            "DELETE {status}"
        );
    }
    let schemas = &document["components"]["schemas"];
    assert!(schemas["SnapshotSummary"]["properties"]["id"].is_object());
    assert!(schemas["SnapshotList"]["properties"]["retention"].is_object());
    assert!(schemas["SnapshotCollected"]["properties"]["sections"].is_object());
    // No upload, no shell, no capture: the surface carries none of them.
    let paths = document["paths"].as_object().unwrap();
    for word in ["upload", "shell", "capture", "pcap", "fleet"] {
        assert!(
            !paths
                .keys()
                .any(|path| path.contains("diagnostics") && path.contains(word)),
            "the diagnostics surface declares a `{word}` route"
        );
    }
}
