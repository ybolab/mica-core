//! Route tests for the provisioning-document status (`provisioning_api.rs`):
//! transport, authentication, the read-only surface and what it must never
//! serve. What a document MEANS — validation, idempotence, the claim gate — is
//! mosd's contract, tested in `mosd/src/provisioning_doc.rs`.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::*;

const STATUS_PATH: &str = "/api/v1/provisioning/status";

/// The baked manifest, in the shape `mosd_settings::configuration` accepts.
///
/// Every field of that struct is required — it carries no serde defaults — so
/// a shortened fixture would not parse, the reader would fall back to the code
/// defaults, and the test would pass without anything having been read. This
/// is `meta.example/updates/manifest.json`.
const BAKED_MANIFEST: &str = r#"{
  "schema": "mos/meta/v1",
  "product": { "vendor": "example", "model": "mos-appliance" },
  "update": {
    "source": "https://baked.example/v1/manifest.json",
    "channel": "stable",
    "policy": "check",
    "checkIntervalMinutes": 1440
  },
  "http": { "credentialHosts": [] },
  "fleet": { "enabled": false, "url": null }
}
"#;

/// A device with the baked tree every device has, which is what this route
/// reads on every request. A build host has no `/usr/share/mos/meta`, so
/// without this the reading tests below never reach the handler at all.
///
/// The tree is the production shape: the manifest and nothing beside it.
/// `meta/GENERATED` is conditional on a device — staged only for
/// development-grade material — so one file is what a shipped image carries.
///
/// The `TempDir` comes back with the router because dropping it deletes the
/// tree the next request would read.
fn provisioning_app(tree: serde_json::Value) -> (Router, TempDir) {
    provisioning_app_with_updates(tree, None)
}

/// A route fixture carrying an optional isolated operator update document.
fn provisioning_app_with_updates(
    tree: serde_json::Value,
    updates_document: Option<&str>,
) -> (Router, TempDir) {
    provisioning_app_with_documents(tree, updates_document, None)
}

/// A route fixture carrying isolated operator update and fleet documents.
fn provisioning_app_with_documents(
    tree: serde_json::Value,
    updates_document: Option<&str>,
    fleet_document: Option<&str>,
) -> (Router, TempDir) {
    provisioning_app_with_manifest_and_documents(
        tree,
        BAKED_MANIFEST,
        updates_document,
        fleet_document,
    )
}

fn provisioning_app_with_manifest_and_documents(
    tree: serde_json::Value,
    manifest: &str,
    updates_document: Option<&str>,
    fleet_document: Option<&str>,
) -> (Router, TempDir) {
    let dir = TempDir::new().expect("temp baked metadata");
    let updates = dir.path().join("meta/updates");
    std::fs::create_dir_all(&updates).expect("meta/updates");
    std::fs::write(updates.join("manifest.json"), manifest).expect("baked manifest");
    let updates_path = dir.path().join("config/updates.json");
    if let Some(document) = updates_document {
        std::fs::create_dir(updates_path.parent().expect("operator config parent"))
            .expect("operator config directory");
        std::fs::write(&updates_path, document).expect("operator updates document");
    }
    let fleet_path = dir.path().join("config/fleet.json");
    if let Some(document) = fleet_document {
        std::fs::create_dir_all(fleet_path.parent().expect("fleet config parent"))
            .expect("fleet config directory");
        std::fs::write(&fleet_path, document).expect("operator fleet document");
    }
    let fake = Arc::new(FakeSettings::new(tree));
    let router = app(AppState::new(fake, SIGNING_KEY)
        .with_meta_manifest(updates.join("manifest.json"))
        .with_updates_path(updates_path)
        .with_fleet_path(fleet_path));
    (router, dir)
}

#[tokio::test]
async fn the_status_resolves_an_isolated_fleet_document_without_activity_state() {
    let (tree, token) = with_token(applied_tree());
    let (router, _meta) = provisioning_app_with_documents(
        tree,
        None,
        Some(
            r#"{
              "schema": "mos/fleet-config/v1",
              "enabled": true,
              "reporting": false,
              "url": "https://fleet.example.invalid"
            }"#,
        ),
    );

    let response = bearer(&router, "GET", STATUS_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let status = body_json(response).await;
    assert_eq!(
        status["operator"],
        json!({
            "fleet": {
                "enabled": true,
                "reporting": false,
                "url": "https://fleet.example.invalid",
            },
        })
    );
    assert_eq!(
        status["effective"]["fleet"],
        json!({
            "enabled": true,
            "reporting": false,
            "url": "https://fleet.example.invalid",
        })
    );
    for activity in ["registered", "connected", "lastReport", "credential"] {
        assert!(status["operator"]["fleet"].get(activity).is_none());
        assert!(status["effective"]["fleet"].get(activity).is_none());
    }
}

#[tokio::test]
async fn fleet_url_absence_and_null_stay_distinct_while_both_use_baked() {
    let baked = BAKED_MANIFEST.replace(
        r#""fleet": { "enabled": false, "url": null }"#,
        r#""fleet": { "enabled": true, "url": "https://baked.example/fleet" }"#,
    );
    let cases = [
        (
            None,
            json!({}),
            json!({
                "enabled": true,
                "reporting": true,
                "url": "https://baked.example/fleet",
            }),
        ),
        (
            Some(r#"{ "schema": "mos/fleet-config/v1", "reporting": false }"#),
            json!({ "fleet": { "reporting": false } }),
            json!({
                "enabled": true,
                "reporting": false,
                "url": "https://baked.example/fleet",
            }),
        ),
        (
            Some(r#"{ "schema": "mos/fleet-config/v1", "reporting": false, "url": null }"#),
            json!({ "fleet": { "reporting": false, "url": null } }),
            json!({
                "enabled": true,
                "reporting": false,
                "url": "https://baked.example/fleet",
            }),
        ),
    ];

    for (document, expected_operator, expected_fleet) in cases {
        let (tree, token) = with_token(applied_tree());
        let (router, _meta) =
            provisioning_app_with_manifest_and_documents(tree, &baked, None, document);
        let status = body_json(bearer(&router, "GET", STATUS_PATH, &token).await).await;
        assert_eq!(status["operator"], expected_operator);
        assert_eq!(status["effective"]["fleet"], expected_fleet);
    }
}

#[tokio::test]
async fn fleet_enabled_and_reporting_overrides_resolve_without_activity() {
    let cases = [
        (
            r#"{ "schema": "mos/fleet-config/v1", "enabled": true }"#,
            json!({ "fleet": { "enabled": true } }),
            json!({ "enabled": true, "reporting": true, "url": null }),
        ),
        (
            r#"{
              "schema": "mos/fleet-config/v1",
              "enabled": false,
              "reporting": true,
              "url": "https://fleet.example.invalid"
            }"#,
            json!({
                "fleet": {
                    "enabled": false,
                    "reporting": true,
                    "url": "https://fleet.example.invalid",
                },
            }),
            json!({
                "enabled": false,
                "reporting": false,
                "url": "https://fleet.example.invalid",
            }),
        ),
    ];

    for (document, expected_operator, expected_fleet) in cases {
        let (tree, token) = with_token(applied_tree());
        let (router, _meta) = provisioning_app_with_documents(tree, None, Some(document));
        let status = body_json(bearer(&router, "GET", STATUS_PATH, &token).await).await;
        assert_eq!(status["operator"], expected_operator);
        assert_eq!(status["effective"]["fleet"], expected_fleet);
        for activity in ["registered", "connected", "lastReport", "credential"] {
            assert!(status["operator"]["fleet"].get(activity).is_none());
            assert!(status["effective"]["fleet"].get(activity).is_none());
        }
    }
}

#[tokio::test]
async fn invalid_fleet_documents_fail_closed_without_disclosing_rejected_values() {
    const REJECTED: &str = "REJECTED-FLEET-SENTINEL";
    let documents = [
        (r#"[]"#, "non-object root"),
        (r#"{ "enabled": true }"#, "missing schema"),
        (
            r#"{ "schema": "REJECTED-FLEET-SENTINEL" }"#,
            "unsupported schema",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "enabled": "REJECTED-FLEET-SENTINEL" }"#,
            "incorrect enabled type",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "reporting": 7 }"#,
            "incorrect reporting type",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "url": true }"#,
            "incorrect URL type",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "url": "http://REJECTED-FLEET-SENTINEL" }"#,
            "non-HTTPS URL",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "unknown": "REJECTED-FLEET-SENTINEL" }"#,
            "unknown field",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "signingKeys": ["REJECTED-FLEET-SENTINEL"] }"#,
            "anchor field",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "enabled": false, "enabled": true }"#,
            "duplicate field",
        ),
        (
            r#"{ "schema": "mos/fleet-config/v1", "url": "REJECTED-FLEET-SENTINEL" "#,
            "malformed JSON",
        ),
    ];

    for (document, case) in documents {
        let (tree, token) = with_token(applied_tree());
        let (router, _meta) = provisioning_app_with_documents(tree, None, Some(document));
        let response = bearer(&router, "GET", STATUS_PATH, &token).await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{case}"
        );
        let body = body_string(response).await;
        let error: serde_json::Value = serde_json::from_str(&body).expect("API error envelope");
        assert_eq!(
            error["error"]["code"], "configuration_unavailable",
            "{case}"
        );
        assert!(!body.contains(REJECTED), "{case} disclosed input: {body}");
    }
}

#[tokio::test]
async fn unreadable_and_invalid_utf8_fleet_documents_fail_closed() {
    let (tree, token) = with_token(applied_tree());
    let (router, meta) = provisioning_app(tree);
    std::fs::create_dir_all(meta.path().join("config/fleet.json"))
        .expect("directory in place of fleet document");
    let response = bearer(&router, "GET", STATUS_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        envelope(response).await["code"],
        "configuration_unavailable"
    );

    let (tree, token) = with_token(applied_tree());
    let (router, meta) = provisioning_app(tree);
    std::fs::create_dir_all(meta.path().join("config")).expect("fleet config directory");
    std::fs::write(meta.path().join("config/fleet.json"), [0xff])
        .expect("invalid UTF-8 fleet document");
    let response = bearer(&router, "GET", STATUS_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        envelope(response).await["code"],
        "configuration_unavailable"
    );
}

/// A device that applied a document from the boot medium, and was claimed by
/// it. The digest is the one mosd would have recorded; the secrets the
/// document carried live where they belong and are not in this subtree.
fn applied_tree() -> serde_json::Value {
    let mut tree = configured_tree("hunter2secret");
    tree["provisioning"] = json!({
        "state": "complete",
        "deviceId": "0123456789abcdef0123456789abcdef",
        "seededGeneration": 1,
        "document": {
            "appliedVersion": 1,
            "appliedDigest": "9f2c1d0e5a7b4c3d2e1f00112233445566778899aabbccddeeff001122334455",
            "lastImport": {
                "source": "boot",
                "outcome": "applied",
                "at": 1_700_000_000,
            },
        },
    });
    tree
}

/// A device nothing has ever been offered to: provisioned by Layer 1, no
/// document record, and no administrator credential.
fn unclaimed_tree() -> serde_json::Value {
    let mut tree = unconfigured_tree();
    tree["provisioning"] = json!({
        "state": "complete",
        "deviceId": "0123456789abcdef0123456789abcdef",
        "seededGeneration": 1,
    });
    tree
}

#[tokio::test]
async fn the_status_reports_the_applied_document_and_requires_a_credential() {
    let (tree, token) = with_token(applied_tree());
    let (router, _meta) = provisioning_app(tree);

    let anonymous = get(&router, STATUS_PATH, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(envelope(anonymous).await["code"], "not_authenticated");

    let response = bearer(&router, "GET", STATUS_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, STATUS_PATH);
    let status = body_json(response).await;
    assert_eq!(status["documentVersion"], 1);
    assert_eq!(
        status["documentDigest"],
        "9f2c1d0e5a7b4c3d2e1f00112233445566778899aabbccddeeff001122334455"
    );
    assert_eq!(status["lastImport"]["source"], "boot");
    assert_eq!(status["lastImport"]["outcome"], "applied");
    assert_eq!(status["lastImport"]["at"], 1_700_000_000_u64);
    assert_eq!(
        status["unclaimed"], false,
        "a device with an admin credential is claimed"
    );
}

#[tokio::test]
async fn a_device_no_document_reached_reports_nulls_and_unclaimed() {
    let (tree, token) = with_token(unclaimed_tree());
    let (router, _meta) = provisioning_app(tree);

    let status = body_json(bearer(&router, "GET", STATUS_PATH, &token).await).await;
    assert_eq!(status["documentVersion"], serde_json::Value::Null);
    assert_eq!(status["documentDigest"], serde_json::Value::Null);
    assert_eq!(status["lastImport"], serde_json::Value::Null);
    assert_eq!(
        status["unclaimed"], true,
        "with no admin credential the device is still claimable"
    );
}

#[tokio::test]
async fn the_status_resolves_the_isolated_operator_document_over_its_baked_fixture() {
    let (tree, token) = with_token(applied_tree());
    let (router, _meta) = provisioning_app_with_updates(
        tree,
        Some(
            r#"{
          "checkIntervalMinutes": 60,
          "source": {
            "url": "https://operator.example/v1/manifest.json",
            "channel": "edge",
            "maxBytes": 123456789
          },
          "policy": "off",
          "network": { "mode": "offline" },
          "rebootGate": {
            "blockingStatuses": ["UNPROJECTED-SECRET-SENTINEL"]
          }
        }"#,
        ),
    );

    let response = bearer(&router, "GET", STATUS_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(!body.contains("UNPROJECTED-SECRET-SENTINEL"), "{body}");
    for unprojected_key in ["maxBytes", "network", "rebootGate", "blockingStatuses"] {
        assert!(!body.contains(unprojected_key), "{body}");
    }
    let status: serde_json::Value = serde_json::from_str(&body).expect("provisioning status");
    assert_eq!(
        status["baked"],
        serde_json::from_str::<serde_json::Value>(BAKED_MANIFEST).expect("baked fixture")
    );
    let baked_digest = format!("{:x}", Sha256::digest(BAKED_MANIFEST.as_bytes()));
    assert_eq!(
        status["bakedDigests"],
        json!({ "updates/manifest.json": baked_digest })
    );
    assert_eq!(
        status["operator"],
        json!({
            "update": {
                "source": "https://operator.example/v1/manifest.json",
                "channel": "edge",
                "policy": "off",
            },
        })
    );
    assert_eq!(
        status["effective"],
        json!({
            "update": {
                "source": "https://operator.example/v1/manifest.json",
                "channel": "edge",
                "policy": "off",
            },
            "fleet": { "url": null, "enabled": false, "reporting": false },
        })
    );
    assert_eq!(
        status["effective"]
            .as_object()
            .expect("effective projection")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["fleet", "update"])
    );
    assert_eq!(
        status["operator"]
            .as_object()
            .expect("operator projection")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["update"])
    );
    assert_eq!(
        status["operator"]["update"]
            .as_object()
            .expect("operator update projection")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["channel", "policy", "source"])
    );
    assert_eq!(
        status["effective"]["update"]
            .as_object()
            .expect("effective update projection")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["channel", "policy", "source"])
    );
    assert_eq!(
        status["effective"]["fleet"]
            .as_object()
            .expect("effective fleet projection")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["enabled", "reporting", "url"])
    );
}

#[tokio::test]
async fn absent_and_null_operator_sources_remain_distinct_while_both_use_baked() {
    let (tree, token) = with_token(applied_tree());
    let (absent_router, _absent_tree) = provisioning_app(tree.clone());
    let absent = body_json(bearer(&absent_router, "GET", STATUS_PATH, &token).await).await;

    let (null_router, _null_tree) =
        provisioning_app_with_updates(tree, Some(r#"{ "source": { "url": null } }"#));
    let explicit_null = body_json(bearer(&null_router, "GET", STATUS_PATH, &token).await).await;

    assert_eq!(absent["operator"], json!({}));
    assert_eq!(
        explicit_null["operator"],
        json!({ "update": { "source": null } })
    );
    for status in [&absent, &explicit_null] {
        assert_eq!(
            status["effective"]["update"]["source"], "https://baked.example/v1/manifest.json",
            "absent and explicit null both clear an override back to baked"
        );
    }
}

#[tokio::test]
async fn invalid_operator_documents_fail_closed_without_disclosing_rejected_values() {
    const REJECTED_SECRET: &str = "REJECTED-SECRET-SENTINEL";
    let documents = [
        (
            r#"{ "source": { "url": "REJECTED-SECRET-SENTINEL" } } trailing"#,
            "malformed JSON",
        ),
        (
            r#"{ "source": { "signingKeys": ["REJECTED-SECRET-SENTINEL"] } }"#,
            "operator trust anchor",
        ),
        (
            r#"{
              "policy": "auto",
              "rebootGate": {
                "blockingStatuses": ["REJECTED-SECRET-SENTINEL"]
              }
            }"#,
            "validation-invalid policy",
        ),
    ];

    for (document, case) in documents {
        let (tree, token) = with_token(applied_tree());
        let (router, _meta) = provisioning_app_with_updates(tree, Some(document));
        let response = bearer(&router, "GET", STATUS_PATH, &token).await;
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{case}"
        );
        let body = body_string(response).await;
        let error: serde_json::Value = serde_json::from_str(&body).expect("API error envelope");
        assert_eq!(
            error["error"]["code"], "configuration_unavailable",
            "{case}"
        );
        assert!(
            !body.contains(REJECTED_SECRET),
            "{case} disclosed the rejected value: {body}"
        );
    }
}

#[tokio::test]
async fn an_unreadable_operator_path_fails_closed() {
    let (tree, token) = with_token(applied_tree());
    let (router, meta) = provisioning_app(tree);
    std::fs::create_dir_all(meta.path().join("config/updates.json"))
        .expect("directory in place of operator document");

    let response = bearer(&router, "GET", STATUS_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        envelope(response).await["code"],
        "configuration_unavailable"
    );
}

#[tokio::test]
async fn a_refused_document_is_reported_with_its_key_path_and_no_value() {
    let mut tree = unclaimed_tree();
    tree["provisioning"]["document"] = json!({
        "lastImport": {
            "source": "media",
            "outcome": "rejected",
            "reason": "`wifi.networks[0].psk`: a WPA2 passphrase is 8 to 63 characters \
                       (or a 64-digit hex PMK)",
            "at": 0,
        },
    });
    let (tree, token) = with_token(tree);
    let (router, _meta) = provisioning_app(tree);

    let status = body_json(bearer(&router, "GET", STATUS_PATH, &token).await).await;
    // A refusal applied nothing, so there is no applied version or digest to
    // report — the two halves of the record are independent on purpose.
    assert_eq!(status["documentVersion"], serde_json::Value::Null);
    assert_eq!(status["documentDigest"], serde_json::Value::Null);
    assert_eq!(status["lastImport"]["outcome"], "rejected");
    let reason = status["lastImport"]["reason"].as_str().expect("a reason");
    assert!(
        reason.contains("wifi.networks[0].psk"),
        "the reason must name the key path: {reason}"
    );
    assert_eq!(
        status["unclaimed"], true,
        "a refused document must leave a claimable appliance"
    );
}

// Read-only, and the whole surface: no verb here applies, re-applies or clears
// a document, so a write is §2.4's method_not_allowed envelope and not a 404.
#[tokio::test]
async fn the_provisioning_surface_is_read_only() {
    let (tree, token) = with_token(applied_tree());
    let (router, _fake) = test_app(tree);

    for method in ["PUT", "POST", "DELETE", "PATCH"] {
        let response = bearer_json(&router, method, STATUS_PATH, &token, "{}").await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} on the provisioning status"
        );
        assert_eq!(envelope(response).await["code"], "method_not_allowed");
    }

    // And there is no route beside it that would apply one.
    for path in [
        "/api/v1/provisioning",
        "/api/v1/provisioning/apply",
        "/api/v1/provisioning/document",
        "/api/v1/actions/provision",
    ] {
        let response = bearer(&router, "GET", path, &token).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        let response = bearer_json(&router, "POST", path, &token, "{}").await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

// The secret-safety half apid owns. Two sentinels are planted where a careless
// handler would pick them up — the admin hash the claim check reads, and a
// secret-named field under `provisioning` — and neither may reach the body.
#[tokio::test]
async fn the_status_serves_no_secret_from_either_subtree_it_reads() {
    let mut tree = applied_tree();
    // A secret-named field inside the subtree this route reads. mosd's schema
    // has none, and this is the fail-closed half of that: the day one appears,
    // the redactor already covers it rather than it being served in the clear
    // until somebody remembers this route.
    tree["provisioning"]["document"]["psk"] = json!("PSK-SENTINEL-FROM-THE-TREE");
    let admin_hash = tree["access"]["webAdmin"]["password_hash"]
        .as_str()
        .expect("a hash")
        .to_string();
    let (tree, token) = with_token(tree);
    let (router, _meta) = provisioning_app(tree);

    let body = body_string(bearer(&router, "GET", STATUS_PATH, &token).await).await;
    assert!(
        !body.contains("PSK-SENTINEL-FROM-THE-TREE"),
        "a secret-named field under `provisioning` reached the wire: {body}"
    );
    assert!(
        !body.contains(&admin_hash),
        "the administrator hash reached the wire: {body}"
    );
    // The positive control: the route did answer, and answered about this
    // device, so the assertions above are not passing on an empty body.
    assert!(body.contains("documentDigest"), "{body}");
    assert!(body.contains("\"unclaimed\":false"), "{body}");
}

// A client reading only `openapi.json` has to be able to learn this route and
// what it answers.
#[test]
fn the_openapi_document_covers_the_provisioning_status_route() {
    let document: serde_json::Value =
        serde_json::from_str(&crate::openapi::document_json()).expect("the document is JSON");
    let paths = document["paths"].as_object().expect("paths is an object");
    let entry = paths
        .get(STATUS_PATH)
        .unwrap_or_else(|| panic!("{STATUS_PATH} is not in the document: {paths:?}"));
    let get = &entry["get"];
    for status in ["200", "401", "405", "500", "503", "504"] {
        assert!(
            get["responses"][status].is_object(),
            "{status} is undocumented: {get:?}"
        );
    }
    // GET and nothing else: the document is the route table, so this is the
    // assertion that no write verb was added beside it.
    assert_eq!(
        entry.as_object().expect("an operation map").len(),
        1,
        "the provisioning surface must serve GET alone: {entry:?}"
    );

    // The response schema names what it answers and nothing more.
    let schema = &document["components"]["schemas"]["ProvisioningStatus"]["properties"];
    for member in [
        "documentVersion",
        "documentDigest",
        "lastImport",
        "unclaimed",
        "baked",
        "bakedDigests",
        "operator",
        "effective",
    ] {
        assert!(
            schema[member].is_object(),
            "{member} is missing: {schema:?}"
        );
    }
    assert_eq!(
        schema.as_object().expect("a property map").len(),
        8,
        "the status must carry exactly the eight documented members: {schema:?}"
    );
}
