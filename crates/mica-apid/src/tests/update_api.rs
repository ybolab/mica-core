//! Route tests for the update cluster (`update_api.rs`): transport,
//! authentication, the 409/422 mappings and the verified-deployment resolution.
//! What the states and policies MEAN is micad's contract, tested in micad.

use axum::http::StatusCode;
use serde_json::json;

use super::*;

const UPDATE_PATH: &str = "/api/v1/update";
const CHECK_PATH: &str = "/api/v1/update/check";
const FETCH_PATH: &str = "/api/v1/update/fetch";
const INSTALL_PATH: &str = "/api/v1/update/install";
const CONFIRM_PATH: &str = "/api/v1/update/confirm";
const REJECT_PATH: &str = "/api/v1/update/reject";
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
        "lifecycle": { "state": "ready", "deploymentId": "a".repeat(64) },
        "boot": { "deploymentId": "b".repeat(64), "contentVerified": true },
        "state": { "current": "b".repeat(64), "candidate": null },
    });
    let (router, fake, token) = update_app(seeded.clone());

    let anonymous = get(&router, UPDATE_PATH, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(
        fake.update_calls().is_empty(),
        "an unauthenticated read must not reach micad"
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
            "{path} must reach micad as `{call}`, got {:?}",
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
    assert_eq!(body["error"]["source"], "micad");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("offline")),
        "the refusing rule must reach the caller: {body}"
    );
}

#[tokio::test]
async fn deployment_actions_refuse_missing_malformed_and_path_based_requests() {
    let (router, fake, token) =
        update_app(json!({"lifecycle":{"state":"ready","deploymentId":"a".repeat(64)}}));
    for path in [INSTALL_PATH, CONFIRM_PATH, REJECT_PATH] {
        for body in [
            json!({}),
            json!({"deploymentId":"a".repeat(63)}),
            json!({"deploymentId":"A".repeat(64)}),
            json!({"deploymentId":"../candidate.json"}),
            json!({"bundlePath":"/media/usb/update.raucb"}),
            json!({"deploymentId":"a".repeat(64),"extra":true}),
        ] {
            let response = bearer_json(&router, "POST", path, &token, &body.to_string()).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}: {body}");
        }
    }
    assert!(fake.update_calls().is_empty());
}

#[tokio::test]
async fn native_identity_refusals_keep_the_bus_validation_code() {
    let (router, fake, token) = update_app(json!({}));
    fake.refuse_updates(INVALID_ARGS, "action must name the running deployment");
    let response = bearer_json(
        &router,
        "POST",
        CONFIRM_PATH,
        &token,
        &json!({"deploymentId":"b".repeat(64)}).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "validation_failed"
    );
}

/// A seeded state document whose `rollback` object is what micad's
/// `rollback_eligibility` would have written for a permitted rollback.
fn permitted_rollback() -> serde_json::Value {
    json!({"boot":{"deploymentId":"a".repeat(64)},
        "state":{"current":"a".repeat(64),"fallback":"b".repeat(64),"candidate":null,"failed":[],"highestGeneration":2},
        "rollback":{"target":"b".repeat(64),"permitted":true,"reason":null}})
}

#[tokio::test]
async fn the_state_read_carries_the_rollback_verdict_and_no_second_route_serves_it() {
    let (router, _fake, token) = update_app(permitted_rollback());
    let response = bearer(&router, "GET", UPDATE_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["rollback"]["target"], "b".repeat(64));
    assert_eq!(body["rollback"]["permitted"], true);
    assert!(body["rollback"]["reason"].is_null());

    // The action's path is POST-only: there is no read route for slot state
    // beside `GET /api/v1/update`.
    let (router, _fake, token) = update_app(permitted_rollback());
    let read = bearer(&router, "GET", ROLLBACK_PATH, &token).await;
    assert_eq!(read.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_permitted_rollback_uses_the_native_atomic_action_and_names_the_next_step() {
    let (router, fake, token) = update_app(permitted_rollback());

    let anonymous = json_request(&router, "POST", ROLLBACK_PATH, json!({}), None, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(
        fake.update_calls().is_empty(),
        "an unauthenticated rollback must not reach micad"
    );

    let response = bearer(&router, "POST", ROLLBACK_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["target"], "b".repeat(64));
    assert_eq!(body["deploymentId"], "a".repeat(64));
    assert_eq!(body["nextStep"], "POST /api/v1/actions/reboot");
    // The guard is read first, then exactly one mark is emitted: `bad` on the
    // BOOTED slot. Nothing here marks the target good, and nothing reboots.
    assert_eq!(
        fake.update_calls(),
        vec![
            "get_update_state".to_string(),
            format!("rollback {}", "a".repeat(64))
        ],
    );
}

#[tokio::test]
async fn every_guard_refusal_is_a_409_carrying_its_own_reason() {
    // Every reason `rollback_eligibility` produces, each with the state
    // document micad writes for it.
    let cases = [
        ("candidate_pending", json!(null)),
        ("running_not_confirmed", json!(null)),
        ("no_usable_fallback", json!(null)),
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
        // Permitted with no resolved target is a shape micad cannot write; if
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
async fn a_native_rollback_failure_after_precheck_is_reported_without_claiming_success() {
    let (router, fake, token) = update_app(permitted_rollback());
    // The guard read succeeds (the fake answers it from the seeded tree);
    // the native rollback action then refuses.
    fake.refuse_updates(
        "org.freedesktop.DBus.Error.Failed",
        "rollback refused: candidate is pending",
    );
    let response = bearer(&router, "POST", ROLLBACK_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body_json(response).await["error"]["code"], "micad_failed");
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
        "a CSRF-refused rollback must not reach micad"
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
        "a CSRF-refused mutation must not reach micad"
    );
}

#[tokio::test]
async fn every_native_deployment_mutation_requires_a_credential_and_browser_csrf() {
    let (router, fake, token) = update_app(json!({}));
    let cookie = login(&router, "hunter2secret").await;
    for path in [INSTALL_PATH, CONFIRM_PATH, REJECT_PATH] {
        let body = json!({"deploymentId":"a".repeat(64)});
        assert_eq!(
            json_request(&router, "POST", path, body.clone(), None, None)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            json_request(&router, "POST", path, body, Some(&cookie), None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            bearer(&router, "GET", path, &token).await.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    assert!(fake.update_calls().is_empty());
    for removed in ["/api/v1/update/mark", "/api/v1/update/clear-suppression"] {
        assert_eq!(
            bearer(&router, "POST", removed, &token).await.status(),
            StatusCode::NOT_FOUND
        );
    }
}

const CONFIG_PATH: &str = "/api/v1/update/config";

/// The write route: administrator authority, the patch forwarded to micad
/// verbatim, and the saved document answered.
#[tokio::test]
async fn the_config_write_takes_a_credential_and_hands_the_patch_to_mosd() {
    let (router, fake, token) = update_app(json!({}));
    let patch = json!({ "source": { "channel": "beta" } });

    let anonymous = json_request(&router, "POST", CONFIG_PATH, patch.clone(), None, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert!(
        fake.update_calls().is_empty(),
        "an unauthenticated write must not reach micad"
    );

    let response = bearer_json(&router, "POST", CONFIG_PATH, &token, &patch.to_string()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, patch);
    assert_eq!(
        fake.update_calls(),
        vec![format!("config {patch}")],
        "apid forwards the patch and writes no file of its own"
    );
}

/// The refusals micad produces, mapped: a rejected patch is the request's
/// fault (422) and an unreadable document on the device is not (409).
#[tokio::test]
async fn the_config_write_maps_the_refusals_and_names_the_offending_field() {
    let (router, fake, token) = update_app(json!({}));
    fake.refuse_updates(
        INVALID_ARGS,
        "/mica/config/updates.json: policy `auto` requires at least one maintenance window",
    );
    let response = bearer_json(
        &router,
        "POST",
        CONFIG_PATH,
        &token,
        &json!({ "policy": "auto" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = body_json(response).await;
    assert_eq!(body["error"]["code"], "validation_failed");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("maintenance window"),
        "the rule reaches the operator: {body}"
    );

    let (router, fake, token) = update_app(json!({}));
    fake.refuse_updates(
        ACCESS_DENIED,
        "parse /mica/config/updates.json: expected value",
    );
    let response = bearer_json(
        &router,
        "POST",
        CONFIG_PATH,
        &token,
        &json!({ "policy": "off" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(response).await["error"]["code"], "policy_refused");
}

/// A body that is not JSON is the envelope's 400 and never reaches micad.
#[tokio::test]
async fn the_config_write_refuses_a_body_that_is_not_json() {
    let (router, fake, token) = update_app(json!({}));
    let response = bearer_json(&router, "POST", CONFIG_PATH, &token, "{not json").await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"]["code"],
        "request_invalid"
    );
    assert!(fake.update_calls().is_empty());
}

#[tokio::test]
async fn explicit_native_deployment_actions_require_ids_and_reach_their_own_bus_commands() {
    let (router, fake, token) = update_app(json!({}));
    let id = "a".repeat(64);
    for action in ["confirm", "reject"] {
        let path = format!("/api/v1/update/{action}");
        let response = bearer_json(
            &router,
            "POST",
            &path,
            &token,
            &json!({"deploymentId":id}).to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{action}");
        assert!(fake.update_calls().contains(&format!("{action} {id}")));
    }
    let response = bearer_json(
        &router,
        "POST",
        INSTALL_PATH,
        &token,
        &json!({"deploymentId":id}).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(fake.update_calls().contains(&format!("install {id}")));
}
