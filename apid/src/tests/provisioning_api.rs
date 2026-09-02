//! Route tests for the provisioning-document status (`provisioning_api.rs`):
//! transport, authentication, the read-only surface and what it must never
//! serve. What a document MEANS — validation, idempotence, the claim gate — is
//! mosd's contract, tested in `mosd/src/provisioning_doc.rs`.

use axum::http::StatusCode;
use serde_json::json;

use super::*;

const STATUS_PATH: &str = "/api/v1/provisioning/status";

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
    let (router, _fake) = test_app(tree);

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
    let (router, _fake) = test_app(tree);

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
    let (router, _fake) = test_app(tree);

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
    let (router, _fake) = test_app(tree);

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
    ] {
        assert!(
            schema[member].is_object(),
            "{member} is missing: {schema:?}"
        );
    }
    assert_eq!(
        schema.as_object().expect("a property map").len(),
        4,
        "the status must carry exactly the four documented members: {schema:?}"
    );
}
