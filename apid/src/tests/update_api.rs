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
const ROLLBACK_PATH: &str = "/api/v1/update/rollback";
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

/// A seeded state document whose `rollback` object is what mosd's
/// `rollback_eligibility` would have written for a permitted rollback.
fn permitted_rollback() -> serde_json::Value {
    json!({
        "booted_slot": "rootfs.0",
        "primary": "rootfs.0",
        "pending_not_confirmed": false,
        "rollback": { "target": "rootfs.1", "permitted": true, "reason": null },
    })
}

#[tokio::test]
async fn the_state_read_carries_the_rollback_verdict_and_no_second_route_serves_it() {
    let (router, _fake, token) = update_app(permitted_rollback());
    let response = bearer(&router, "GET", UPDATE_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["rollback"]["target"], "rootfs.1");
    assert_eq!(body["rollback"]["permitted"], true);
    assert!(body["rollback"]["reason"].is_null());

    // The action's path is POST-only: there is no read route for slot state
    // beside `GET /api/v1/update`.
    let (router, _fake, token) = update_app(permitted_rollback());
    let read = bearer(&router, "GET", ROLLBACK_PATH, &token).await;
    assert_eq!(read.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_permitted_rollback_marks_the_booted_slot_bad_and_names_the_next_step() {
    let (router, fake, token) = update_app(permitted_rollback());

    let anonymous = json_request(&router, "POST", ROLLBACK_PATH, json!({}), None, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(
        fake.update_calls().is_empty(),
        "an unauthenticated rollback must not reach mosd"
    );

    let response = bearer(&router, "POST", ROLLBACK_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["target"], "rootfs.1");
    assert_eq!(body["slotName"], "rootfs.0");
    assert_eq!(body["nextStep"], "POST /api/v1/actions/reboot");
    // The guard is read first, then exactly one mark is emitted: `bad` on the
    // BOOTED slot. Nothing here marks the target good, and nothing reboots.
    assert_eq!(
        fake.update_calls(),
        vec![
            "get_update_state".to_string(),
            "mark bad booted".to_string()
        ],
    );
}

#[tokio::test]
async fn every_guard_refusal_is_a_409_carrying_its_own_reason() {
    // Every reason `rollback_eligibility` produces, each with the state
    // document mosd writes for it.
    let cases = [
        ("no_alternate_slot", json!(null)),
        ("alternate_is_booted_slot", json!(null)),
        ("alternate_never_installed", json!("rootfs.1")),
        ("alternate_marked_bad", json!("rootfs.1")),
        ("alternate_is_newer", json!("rootfs.1")),
        ("install_order_unknown", json!("rootfs.1")),
        ("booted_slot_not_confirmed", json!("rootfs.1")),
    ];
    for (reason, target) in cases {
        let (router, fake, token) = update_app(json!({
            "rollback": { "target": target, "permitted": false, "reason": reason },
        }));
        let response = bearer(&router, "POST", ROLLBACK_PATH, &token).await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{reason}");
        let body = body_json(response).await;
        assert_eq!(body["error"]["code"], reason, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(reason)),
            "the reason must reach the caller: {body}"
        );
        // A refused rollback reads the state and stops there.
        assert_eq!(
            fake.update_calls(),
            vec!["get_update_state".to_string()],
            "{reason}"
        );
    }
}

#[tokio::test]
async fn a_verdict_this_surface_does_not_know_is_refused_rather_than_renamed() {
    // A reason outside the vocabulary, and a document with no verdict at all:
    // both fail closed, and neither is reported as one of the five.
    for update in [
        json!({ "rollback": { "target": "rootfs.1", "permitted": false, "reason": "gremlins" } }),
        json!({ "lifecycle": { "state": "idle" } }),
        // Permitted with no resolved target is a shape mosd cannot write; if
        // it ever appears, it must not be acted on.
        json!({ "rollback": { "target": null, "permitted": true, "reason": null } }),
    ] {
        let (router, fake, token) = update_app(update);
        let response = bearer(&router, "POST", ROLLBACK_PATH, &token).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_json(response).await["error"]["code"],
            "rollback_refused"
        );
        assert_eq!(fake.update_calls(), vec!["get_update_state".to_string()]);
    }
}

#[tokio::test]
async fn a_rauc_failure_on_the_rollback_mark_is_500_like_the_mark_route() {
    let (router, fake, token) = update_app(permitted_rollback());
    // The guard read succeeds (the fake answers it from the seeded tree);
    // the mark that follows is what RAUC refuses.
    fake.refuse_updates("org.freedesktop.DBus.Error.Failed", "rauc mark: no primary");
    let response = bearer(&router, "POST", ROLLBACK_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(response).await["error"]["code"], "mosd_failed");
}

#[tokio::test]
async fn a_rollback_from_a_browser_session_needs_its_csrf_token() {
    let (router, fake, _token) = update_app(permitted_rollback());
    let cookie = login(&router, "hunter2secret").await;
    let response = json_request(
        &router,
        "POST",
        ROLLBACK_PATH,
        json!({}),
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        fake.update_calls().is_empty(),
        "a CSRF-refused rollback must not reach mosd"
    );
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
