//! Authenticated signed deployment actions through micad.
//! Every mutation is POST-only and uses the shared credential, CSRF and audit gates.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use micad_settings::{
    REQUESTED, UPDATE_CHECK_EVENT, UPDATE_CONFIG_EVENT, UPDATE_FETCH_EVENT, UPDATE_INSTALL_EVENT,
};

use crate::audit::Source;
use crate::routes::{API, ApiCredential, ApiError, AppState, api_response, bus_api_error};

/// The cluster's paths, one spelling each (single-segment, so axum and
/// OpenAPI agree without a doc variant).
pub(crate) const V1_UPDATE_PATH: &str = "/v1/update";
pub(crate) const V1_UPDATE_CHECK_PATH: &str = "/v1/update/check";
pub(crate) const V1_UPDATE_FETCH_PATH: &str = "/v1/update/fetch";
pub(crate) const V1_UPDATE_INSTALL_PATH: &str = "/v1/update/install";
pub(crate) const V1_UPDATE_CONFIRM_PATH: &str = "/v1/update/confirm";
pub(crate) const V1_UPDATE_REJECT_PATH: &str = "/v1/update/reject";
pub(crate) const V1_UPDATE_ROLLBACK_PATH: &str = "/v1/update/rollback";
pub(crate) const V1_UPDATE_REBOOT_OVERRIDE_PATH: &str = "/v1/update/reboot-override";
pub(crate) const V1_UPDATE_CONFIG_PATH: &str = "/v1/update/config";

/// The D-Bus error name micad's update surface refuses policy-forbidden
/// actions with. Not in `routes.rs`'s table: only this cluster produces it.
const FDO_ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";

/// The update state document, verbatim from micad: `lifecycle` (state machine,
/// policy, reboot gate), per-slot status, `booted_slot`, `primary`,
/// `pending_not_confirmed`, `rollback`, `install` and `last_mark`.
///
/// `rollback` is the one place slot state is offered as a decision: micad's
/// `rollback_eligibility` resolves the alternate slot, says whether a manual
/// rollback is permitted, and names the refusal otherwise. There is no second
/// read route for it — this document is the single writer of that fact.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct UpdateState(Value);

/// Map an update-surface bus failure, reading the one name this cluster adds
/// before handing the rest to the shared mapping.
///
/// - `AccessDenied` → **409** `policy_refused`: the request was well-formed
///   and authenticated, and the device's update policy (offline mode,
///   maintenance window, unreadable policy file) or safe-to-reboot gate
///   refused it. The message names the rule.
/// - `InvalidArgs` → **422** `validation_failed`: a mark state, slot or
///   override TTL outside the offered vocabulary. The shared mapping calls
///   this `settings_rejected`, which on a route that writes no setting would
///   misname the fault.
fn update_bus_error(err: &anyhow::Error) -> Response {
    if let Some(zbus::Error::MethodError(name, message, _)) = err.downcast_ref::<zbus::Error>() {
        let message = message.clone().unwrap_or_else(|| name.to_string());
        if name.as_str() == FDO_ACCESS_DENIED {
            return api_response(
                StatusCode::CONFLICT,
                ApiError::micad("policy_refused", message),
            );
        }
        if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs" {
            return api_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiError::micad("validation_failed", message),
            );
        }
    }
    bus_api_error(err, None)
}

/// §2.4's envelope for a body that is not JSON or not the declared shape.
fn body_rejection(rejection: axum::extract::rejection::JsonRejection) -> Response {
    api_response(
        StatusCode::BAD_REQUEST,
        ApiError::apid("request_invalid", rejection.body_text()),
    )
}

/// Read the complete update state.
///
/// Reads native deployment records and the current acquisition lifecycle from
/// micad. The same response binds the rollback verdict to its retained target
/// and includes check, download and installation progress.
#[utoipa::path(
    get,
    path = V1_UPDATE_PATH,
    context_path = API,
    tag = "update",
    responses(
        (status = 200, description = "Authenticated native deployment state: `boot` (running deployment and kernel/root IDs, content verification and Secure Boot), `state` (current, fallback, candidate, failed deployment IDs and highestGeneration), `deployments` (version, generation, components and remaining trials), `rollback` (permitted, target and reason), `install`, `last_action`, and `lifecycle` (acquisition progress, workspace, policy and reboot gate). A failure carries a closed code beside its detail: `unknown`, `client-spawn-failed`, `client-exit-failure`, `client-output-unparseable`, `unverified-deployment-path`, `no-source-configured`, `policy-not-loaded`, `probe-failed`, `policy-invalid`, `network-offline`, `network-metered`, `client-unavailable`, `deployment-discarded`, `check-refused`, `no-newer-release`, `fetch-refused`, `clock-untrusted`, `outside-window`, `deployment-status-unknown`, `reboot-pending`, `workspace-unready`, `recheck-failed`, `recheck-refused`, `superseded`, `install-refused`, `reboot-gate-closed`, `install-in-flight`, `health-blocking`. Rollback reasons: `candidate_pending`, `running_not_confirmed`, `no_usable_fallback`. Failed deployment IDs and the generation floor are enforced by the native backend for every install.", body = UpdateState),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "micad could not read native deployment state (`micad_failed`)", body = ApiError),
        (status = 503, description = "The call to micad could not be made (`micad_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_state(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match state.api.get_update_state().await {
        Ok(value) => api_response(StatusCode::OK, UpdateState(value)),
        Err(err) => update_bus_error(&err),
    }
}

/// Start an update metadata check.
///
/// Answers **202**: micad admits the check and runs it on a background task;
/// the outcome lands in the state document's `lifecycle` entry.
#[utoipa::path(
    post,
    path = V1_UPDATE_CHECK_PATH,
    context_path = API,
    tag = "update",
    responses(
        (status = 202, description = "The check was admitted and is running; poll `GET /api/v1/update`"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "The update policy refused it: offline mode, no configured source, or an unreadable policy file (`policy_refused`)", body = ApiError),
        (status = 500, description = "The update client is absent or another operation is running (`micad_failed`)", body = ApiError),
        (status = 503, description = "The call to micad could not be made (`micad_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_check(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    match state.api.check_update().await {
        Ok(()) => {
            state.audit.record(UPDATE_CHECK_EVENT, REQUESTED, &source);
            accepted()
        }
        Err(err) => update_bus_error(&err),
    }
}

/// Start a bundle download into the reserve directory.
///
/// Answers **202** like the check; on success the lifecycle records the
/// verified bundle path and enters `ready`.
#[utoipa::path(
    post,
    path = V1_UPDATE_FETCH_PATH,
    context_path = API,
    tag = "update",
    responses(
        (status = 202, description = "The fetch was admitted and is running; poll `GET /api/v1/update`"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "The update policy refused it: offline mode, a metered link without `meteredAllowsFetch`, no configured source, or an unreadable policy file (`policy_refused`)", body = ApiError),
        (status = 500, description = "The update client is absent or another operation is running (`micad_failed`)", body = ApiError),
        (status = 503, description = "The call to micad could not be made (`micad_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_fetch(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    match state.api.fetch_update().await {
        Ok(()) => {
            state.audit.record(UPDATE_FETCH_EVENT, REQUESTED, &source);
            accepted()
        }
        Err(err) => update_bus_error(&err),
    }
}

/// An exact signed deployment identity already verified in the acquisition workspace.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeploymentRequest {
    #[serde(deserialize_with = "deployment_id")]
    #[schema(min_length = 64, max_length = 64, pattern = "^[0-9a-f]{64}$")]
    deployment_id: String,
}

fn valid_deployment_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn deployment_id<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let id = <String as serde::Deserialize>::deserialize(deserializer)?;
    if !valid_deployment_id(&id) {
        return Err(serde::de::Error::custom(
            "deploymentId must be 64 lowercase hexadecimal characters",
        ));
    }
    Ok(id)
}

/// Install an acquired signed deployment; poll update state for completion.
#[utoipa::path(post, path = V1_UPDATE_INSTALL_PATH, context_path = API, tag = "update",
    request_body = DeploymentRequest,
    responses(
        (status = 202, description = "The deployment action was accepted"),
        (status = 400, description = "Invalid JSON or deployment identity (`request_invalid`)", body = ApiError),
        (status = 401, description = "Authentication required (`not_authenticated`)", body = ApiError),
        (status = 403, description = "Browser CSRF token required (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "Operator policy refused the action (`policy_refused`)", body = ApiError),
        (status = 422, description = "Deployment identity or acquisition path is invalid (`validation_failed`)", body = ApiError),
        (status = 500, description = "The native backend refused or failed the action (`micad_failed`)", body = ApiError),
        (status = 503, description = "micad is unreachable (`micad_unreachable`)", body = ApiError),
        (status = 504, description = "micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "POST required (`method_not_allowed`)", body = ApiError),
    ))]
pub(crate) async fn api_v1_update_install(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<DeploymentRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    match state.api.install_update(&request.deployment_id).await {
        Ok(()) => {
            state.audit.record(
                UPDATE_INSTALL_EVENT,
                &format!("{} {}", REQUESTED, request.deployment_id),
                &source,
            );
            api_response(
                StatusCode::ACCEPTED,
                serde_json::json!({"deploymentId":request.deployment_id}),
            )
        }
        Err(error) => update_bus_error(&error),
    }
}

/// Confirm the authenticated running deployment explicitly.
#[utoipa::path(post, path = V1_UPDATE_CONFIRM_PATH, context_path = API, tag = "update",
    request_body = DeploymentRequest,
    responses(
        (status = 200, description = "The deployment action was accepted"),
        (status = 400, description = "Invalid JSON or deployment identity (`request_invalid`)", body = ApiError),
        (status = 401, description = "Authentication required (`not_authenticated`)", body = ApiError),
        (status = 403, description = "Browser CSRF token required (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "Operator policy refused the action (`policy_refused`)", body = ApiError),
        (status = 422, description = "Deployment identity or acquisition path is invalid (`validation_failed`)", body = ApiError),
        (status = 500, description = "The native backend refused or failed the action (`micad_failed`)", body = ApiError),
        (status = 503, description = "micad is unreachable (`micad_unreachable`)", body = ApiError),
        (status = 504, description = "micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "POST required (`method_not_allowed`)", body = ApiError),
    ))]
pub(crate) async fn api_v1_update_confirm(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<DeploymentRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    match state.api.confirm_deployment(&request.deployment_id).await {
        Ok(()) => {
            state.audit.record(
                "update-confirm",
                &format!("{} {}", REQUESTED, request.deployment_id),
                &source,
            );
            api_response(
                StatusCode::OK,
                serde_json::json!({"deploymentId":request.deployment_id}),
            )
        }
        Err(error) => update_bus_error(&error),
    }
}

/// Reject a deployment while retaining a usable fallback.
#[utoipa::path(post, path = V1_UPDATE_REJECT_PATH, context_path = API, tag = "update",
    request_body = DeploymentRequest,
    responses(
        (status = 200, description = "The deployment action was accepted"),
        (status = 400, description = "Invalid JSON or deployment identity (`request_invalid`)", body = ApiError),
        (status = 401, description = "Authentication required (`not_authenticated`)", body = ApiError),
        (status = 403, description = "Browser CSRF token required (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "Operator policy refused the action (`policy_refused`)", body = ApiError),
        (status = 422, description = "Deployment identity or acquisition path is invalid (`validation_failed`)", body = ApiError),
        (status = 500, description = "The native backend refused or failed the action (`micad_failed`)", body = ApiError),
        (status = 503, description = "micad is unreachable (`micad_unreachable`)", body = ApiError),
        (status = 504, description = "micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "POST required (`method_not_allowed`)", body = ApiError),
    ))]
pub(crate) async fn api_v1_update_reject(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<DeploymentRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    match state.api.reject_deployment(&request.deployment_id).await {
        Ok(()) => {
            state.audit.record(
                "update-reject",
                &format!("{} {}", REQUESTED, request.deployment_id),
                &source,
            );
            api_response(
                StatusCode::OK,
                serde_json::json!({"deploymentId":request.deployment_id}),
            )
        }
        Err(error) => update_bus_error(&error),
    }
}

const ROLLBACK_REASONS: [&str; 3] = [
    "candidate_pending",
    "running_not_confirmed",
    "no_usable_fallback",
];

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RollbackResponse {
    deployment_id: String,
    target: String,
    next_step: String,
}

fn rollback_reason_code(reason: Option<&str>) -> &'static str {
    reason
        .and_then(|reason| ROLLBACK_REASONS.into_iter().find(|known| *known == reason))
        .unwrap_or("rollback_refused")
}

/// Reject the confirmed running deployment and select its retained fallback.
/// The native backend revalidates this operation under its transaction lock.
/// Reboot remains a separate action governed by the shared reboot gate.
#[utoipa::path(post, path = V1_UPDATE_ROLLBACK_PATH, context_path = API, tag = "update",
    responses(
        (status = 200, description = "Rollback committed; reboot to run the retained fallback", body = RollbackResponse),
        (status = 401, description = "Authentication required (`not_authenticated`)", body = ApiError),
        (status = 403, description = "Browser CSRF token required (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "Native state refuses rollback: `candidate_pending`, `running_not_confirmed`, `no_usable_fallback`, or `rollback_refused`", body = ApiError),
        (status = 500, description = "The native backend refused or failed rollback (`micad_failed`)", body = ApiError),
        (status = 503, description = "micad is unreachable (`micad_unreachable`)", body = ApiError),
        (status = 504, description = "micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "POST required (`method_not_allowed`)", body = ApiError),
    ))]
pub(crate) async fn api_v1_update_rollback(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    let update = match state.api.get_update_state().await {
        Ok(update) => update,
        Err(error) => return update_bus_error(&error),
    };
    let reason = update.pointer("/rollback/reason").and_then(Value::as_str);
    let target = update
        .pointer("/rollback/target")
        .and_then(Value::as_str)
        .filter(|id| valid_deployment_id(id));
    let running = update
        .pointer("/boot/deploymentId")
        .and_then(Value::as_str)
        .filter(|id| valid_deployment_id(id));
    let permitted = update
        .pointer("/rollback/permitted")
        .and_then(Value::as_bool)
        == Some(true);
    let (Some(target), Some(running), true) = (target, running, permitted) else {
        let code = rollback_reason_code(reason);
        state
            .audit
            .record("update-rollback", &format!("refused: {code}"), &source);
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                code,
                format!(
                    "the native deployment state does not permit rollback: {}",
                    reason.unwrap_or("missing or invalid verdict")
                ),
            ),
        );
    };
    match state.api.rollback_deployment(running).await {
        Ok(()) => {
            state.audit.record(
                "update-rollback",
                &format!("{running} to {target}"),
                &source,
            );
            api_response(
                StatusCode::OK,
                RollbackResponse {
                    deployment_id: running.into(),
                    target: target.into(),
                    next_step: "POST /api/v1/actions/reboot".into(),
                },
            )
        }
        Err(error) => update_bus_error(&error),
    }
}

/// `POST /api/v1/update/reboot-override` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RebootOverrideRequest {
    /// Override TTL in seconds: at least 1, at most the policy's
    /// `overrideMaxSeconds` (never above 3600).
    seconds: u32,
}

/// The armed override, as micad recorded it.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RebootOverride(Value);

/// Arm the bounded administrative override of the safe-to-reboot gate.
///
/// Lifts health-report blocks for the TTL — never an install in flight —
/// and expires on its own. Audited on both sides: this route records the
/// event, and micad logs who armed it and until when.
#[utoipa::path(
    post,
    path = V1_UPDATE_REBOOT_OVERRIDE_PATH,
    context_path = API,
    tag = "update",
    request_body = RebootOverrideRequest,
    responses(
        (status = 200, description = "The override is armed until the answered instant", body = RebootOverride),
        (status = 400, description = "The body is not JSON or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 422, description = "A TTL of zero or above the granted ceiling (`validation_failed`)", body = ApiError),
        (status = 500, description = "micad failed to arm it (`micad_failed`)", body = ApiError),
        (status = 503, description = "The call to micad could not be made (`micad_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_reboot_override(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<RebootOverrideRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    match state.api.set_reboot_override(request.seconds).await {
        Ok(record) => {
            state
                .audit
                .record("update-reboot-override", "armed", &source);
            api_response(StatusCode::OK, RebootOverride(record))
        }
        Err(err) => update_bus_error(&err),
    }
}

/// The operator's update document as micad saved it: the layer-2 keys and
/// nothing resolved.
///
/// A key the operator never set is **absent** and one they set to `null` is
/// **`null`**; both mean "take the baked default" and they are still two
/// different statements about the document. The resolved values — what this
/// device will actually dial and follow — are `GET /api/v1/update`'s
/// `lifecycle.policy` and `GET /api/v1/provisioning/status`'s `effective`.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct UpdateConfig(Value);

/// The body of the update-configuration write: a **patch**, not a document.
///
/// A key this body omits is left exactly as it is on the device; a key set to
/// `null` clears an operator override so the baked default applies again; a
/// key with a value overrides it. That is what makes the write safe from a
/// console: a client that sent the whole resolved document back would pin the
/// image's defaults into the operator's layer and the device would stop
/// following its image, which nobody asked for.
///
/// Accepted keys: `policy` (`off`/`check`/`auto`), `checkIntervalMinutes`,
/// `rebootPolicy` (`manual`/`window`), `source.url`, `source.channel`,
/// `network`, `maintenance` and `rebootGate`. Anything else — including a
/// trust anchor under any name, at any depth — is **422** naming the key: the
/// address this device dials is the operator's, what it will accept is baked
/// into the image, and no body may move that line.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub(crate) struct UpdateConfigWrite(Value);

/// Write the operator's update configuration.
///
/// The one route that changes what this device does on its own: which channel
/// it follows, which address it dials, whether it checks, fetches and installs
/// unattended, and inside which maintenance window. It takes the same
/// administrator authority every other management write takes, and there is no
/// unauthenticated or fleet-derived path to it.
///
/// **apid does not write the file.** micad owns `/mica/config/updates.json` and
/// is its only writer; this route asks. The validation is therefore the same
/// code the update subsystem reads the document with, so a document that is
/// accepted here is one that loads, and a rejected one is refused with the
/// offending field named **before** anything is replaced. `auto` with no
/// maintenance window is refused here rather than failing closed some hours
/// later at a check the operator would have to go looking for.
///
/// Answers **200** with the document as saved.
#[utoipa::path(
    post,
    path = V1_UPDATE_CONFIG_PATH,
    context_path = API,
    tag = "update",
    request_body = UpdateConfigWrite,
    responses(
        (status = 200, description = "The operator document as saved: the keys layer 2 carries, absent and `null` still distinct", body = UpdateConfig),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "The document already on the device does not load, so there is no base to change; nothing was written (`policy_refused`)", body = ApiError),
        (status = 422, description = "The patch names a key this document does not have, a trust anchor, or a value the reader would refuse — `auto` with no maintenance window, a window that is not `HH:MM` (`validation_failed`)", body = ApiError),
        (status = 500, description = "micad could not store it (`micad_failed`)", body = ApiError),
        (status = 503, description = "The call to micad could not be made (`micad_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to micad timed out (`micad_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_config(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<UpdateConfigWrite>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(UpdateConfigWrite(patch)) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    match state.api.set_update_config(&patch).await {
        Ok(document) => {
            state.audit.record(UPDATE_CONFIG_EVENT, "written", &source);
            api_response(StatusCode::OK, UpdateConfig(document))
        }
        Err(err) => update_bus_error(&err),
    }
}

/// The empty 202 the three long-running admissions answer: accepted, running
/// on micad's background task, outcome via the state document.
fn accepted() -> Response {
    use axum::http::header::CACHE_CONTROL;
    use axum::response::IntoResponse;
    (
        StatusCode::ACCEPTED,
        [(
            CACHE_CONTROL,
            crate::assets::mime::CacheClass::NoStore.header_value(),
        )],
    )
        .into_response()
}
