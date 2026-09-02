//! Route tests for the update cluster (`update_api.rs`): transport,
//! authentication, the 409/422 mappings and the staged-bundle resolution.
//! What the states and policies MEAN is mosd's contract, tested in mosd.

use axum::http::StatusCode;
use serde_json::json;

use super::*;

const UPDATE_PATH: &str = "/api/v1/update";
const CHECK_PATH: &str = "/api/v1/update/check";
const FETCH_PATH: &str = "/api/v1/update/fetch";
const INSTALL_PATH: &str = "/api/v1/update/install";
const MARK_PATH: &str = "/api/v1/update/mark";
const OVERRIDE_PATH: &str = "/api/v1/update/reboot-override";

const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

/// A router with a bearer token and a seeded `update` live-state entry.
fn update_app(update: serde_json::Value) -> (axum::Router, Arc<FakeSettings>, String) {
    let (tree, token) = with_token(configured_tree("hunter2secret"));
    let (router, fake) = test_app(tree);
    fake.set_state_entry("update", update);
    (router, fake, token)
}

#[tokio::test]
async fn the_state_read_answers_mosd_verbatim_and_requires_a_credential() {
    let seeded = json!({
        "lifecycle": { "state": "ready", "bundle": "/data/u.raucb" },
        "booted_slot": "rootfs.0",
        "pending_not_confirmed": false,
    });
    let (router, fake, token) = update_app(seeded.clone());

    let anonymous = get(&router, UPDATE_PATH, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(
        fake.update_calls().is_empty(),
        "an unauthenticated read must not reach mosd"
    );

    let response = bearer(&router, "GET", UPDATE_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_value(&response, CACHE_CONTROL), "no-store");
    assert_eq!(body_json(response).await, seeded);
    assert_eq!(fake.update_calls(), vec!["get_update_state"]);
}

#[tokio::test]
async fn check_and_fetch_are_post_only_202_admissions() {
    let (router, fake, token) = update_app(json!({}));

    for (path, call) in [(CHECK_PATH, "check"), (FETCH_PATH, "fetch")] {
        let anonymous = json_request(&router, "POST", path, json!({}), None, None).await;
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED, "{path}");

        let response = bearer(&router, "POST", path, &token).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{path}");
        assert!(
            fake.update_calls().contains(&call.to_string()),
            "{path} must reach mosd as `{call}`, got {:?}",
            fake.update_calls()
        );

        let wrong_method = bearer(&router, "GET", path, &token).await;
        assert_eq!(
            wrong_method.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{path} must be POST-only"
        );
    }
}

#[tokio::test]
async fn a_policy_refusal_is_answered_409_with_mosds_reason() {
    let (router, fake, token) = update_app(json!({}));
    fake.refuse_updates(
        ACCESS_DENIED,
        "network mode is offline: updates arrive by import only",
    );

    let response = bearer(&router, "POST", FETCH_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "policy_refused");
    assert_eq!(body["error"]["source"], "mosd");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("offline")),
        "the refusing rule must reach the caller: {body}"
    );
}

#[tokio::test]
async fn an_install_uses_the_staged_bundle_and_refuses_when_none_is() {
    // Nothing staged, no explicit path: 409 before any bus call.
    let (router, fake, token) = update_app(json!({ "lifecycle": { "state": "idle" } }));
    let response = bearer_json(&router, "POST", INSTALL_PATH, &token, "{}").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "no_staged_bundle");
    assert!(
        fake.update_calls().is_empty(),
        "a refused install must not reach mosd: {:?}",
        fake.update_calls()
    );

    // A staged bundle is what an empty body installs — the verified path,
    // not a guess.
    let (router, fake, token) = update_app(json!({
        "lifecycle": { "state": "ready", "bundle": "/data/staged.raucb" }
    }));
    let response = bearer_json(&router, "POST", INSTALL_PATH, &token, "{}").await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(fake.update_calls(), vec!["install /data/staged.raucb"]);

    // An explicit operator path outranks the staged one (the manual/offline
    // escape hatch) and an unparseable body is 400, not a guessed install.
    let (router, fake, token) = update_app(json!({
        "lifecycle": { "state": "ready", "bundle": "/data/staged.raucb" }
    }));
    let response = bearer_json(
        &router,
        "POST",
        INSTALL_PATH,
        &token,
        &json!({ "bundlePath": "/media/usb/manual.raucb" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(fake.update_calls(), vec!["install /media/usb/manual.raucb"]);

    let response = bearer_json(&router, "POST", INSTALL_PATH, &token, "{not json").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_mark_answers_raucs_slot_and_message_and_bad_vocabulary_is_422() {
    let (router, fake, token) = update_app(json!({}));
    let response = bearer_json(
        &router,
        "POST",
        MARK_PATH,
        &token,
        &json!({ "state": "good", "slot": "booted" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["slotName"], "rootfs.0");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("good"))
    );
    assert_eq!(fake.update_calls(), vec!["mark good booted"]);

    // mosd's InvalidArgs (a state or slot outside the offered vocabulary)
    // is 422 `validation_failed`, not the settings-flavoured code.
    let (router, fake, token) = update_app(json!({}));
    fake.refuse_updates(
        INVALID_ARGS,
        "mark state must be `good` or `bad`, got `active`",
    );
    let response = bearer_json(
        &router,
        "POST",
        MARK_PATH,
        &token,
        &json!({ "state": "active", "slot": "other" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
}

#[tokio::test]
async fn the_reboot_override_answers_the_armed_record() {
    let (router, fake, token) = update_app(json!({}));
    let response = bearer_json(
        &router,
        "POST",
        OVERRIDE_PATH,
        &token,
        &json!({ "seconds": 600 }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert!(body["until"].is_string(), "{body}");
    assert_eq!(fake.update_calls(), vec!["reboot-override 600"]);

    let (router, fake, token) = update_app(json!({}));
    fake.refuse_updates(INVALID_ARGS, "override TTL must be at most 3600 seconds");
    let response = bearer_json(
        &router,
        "POST",
        OVERRIDE_PATH,
        &token,
        &json!({ "seconds": 90000 }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "validation_failed"
    );
}

// A browser session must carry its CSRF token on these mutations — the
// extractor owns the check; this pins that the cluster is behind it.
#[tokio::test]
async fn a_session_mutation_without_csrf_is_403() {
    let (router, fake, _token) = update_app(json!({}));
    let cookie = login(&router, "hunter2secret").await;
    let response = json_request(&router, "POST", CHECK_PATH, json!({}), Some(&cookie), None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        fake.update_calls().is_empty(),
        "a CSRF-refused mutation must not reach mosd"
    );
}
