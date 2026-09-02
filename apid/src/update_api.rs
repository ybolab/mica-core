//! The update cluster: one state read and six actions over mosd's update
//! surface (`GetUpdateState`, `CheckUpdate`, `FetchUpdate`, `InstallUpdate`,
//! `MarkUpdate`, `SetRebootOverride`).
//!
//! Six actions over five action members and no sixth: the guarded rollback is
//! composed here out of `GetUpdateState` (which carries mosd's own rollback
//! verdict) and `MarkUpdate`, rather than being a second way into RAUC.
//!
//! A bounded module beside `routes.rs` rather than more of it: the routes
//! here share that file's session gate ([`ApiCredential`]), envelope and
//! audit sink, and add exactly one mapping of their own — mosd's
//! `AccessDenied`, which on this surface means "the update policy said no"
//! and is answered as **409** `policy_refused` rather than a 500.
//!
//! Every action is POST-only behind the credential extractor, so navigation
//! and prefetch cannot trigger one, and every action is audited before the
//! response leaves.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use crate::audit::Source;
use crate::routes::{API, ApiCredential, ApiError, AppState, api_response, bus_api_error};

/// The cluster's paths, one spelling each (single-segment, so axum and
/// OpenAPI agree without a doc variant).
pub(crate) const V1_UPDATE_PATH: &str = "/v1/update";
pub(crate) const V1_UPDATE_CHECK_PATH: &str = "/v1/update/check";
pub(crate) const V1_UPDATE_FETCH_PATH: &str = "/v1/update/fetch";
pub(crate) const V1_UPDATE_INSTALL_PATH: &str = "/v1/update/install";
pub(crate) const V1_UPDATE_MARK_PATH: &str = "/v1/update/mark";
pub(crate) const V1_UPDATE_ROLLBACK_PATH: &str = "/v1/update/rollback";
pub(crate) const V1_UPDATE_REBOOT_OVERRIDE_PATH: &str = "/v1/update/reboot-override";

/// The D-Bus error name mosd's update surface refuses policy-forbidden
/// actions with. Not in `routes.rs`'s table: only this cluster produces it.
const FDO_ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";

/// The update state document, verbatim from mosd: `lifecycle` (state machine,
/// policy, reboot gate), per-slot status, `booted_slot`, `primary`,
/// `pending_not_confirmed`, `rollback`, `install` and `last_mark`.
///
/// `rollback` is the one place slot state is offered as a decision: mosd's
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
                ApiError::mosd("policy_refused", message),
            );
        }
        if name.as_str() == "org.freedesktop.DBus.Error.InvalidArgs" {
            return api_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiError::mosd("validation_failed", message),
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
/// Asks mosd's `GetUpdateState`, which queries RAUC and re-derives the
/// lifecycle first — this is the polling surface for a UI watching a check,
/// download or install, and it is never answered from a stale record. The
/// `rollback` object is derived from the same fresh slot list, so an operator
/// deciding whether to roll back reads the verdict and the slots it came from
/// in one answer.
#[utoipa::path(
    get,
    path = V1_UPDATE_PATH,
    context_path = API,
    tag = "update",
    responses(
        (status = 200, description = "The update state: `lifecycle` (state machine with reason strings — `update-unavailable` carries the `/mos/updates` workspace's `unavailable`/`degraded` verdict, mirrored under `lifecycle.workspace` —, effective policy, safe-to-reboot gate and override), per-slot status, `booted_slot`, `primary`, `pending_not_confirmed`, `rollback` (`target`, `permitted`, `reason`), `install`, `last_mark`", body = UpdateState),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to answer, e.g. RAUC unreachable (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
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
/// Answers **202**: mosd admits the check and runs it on a background task;
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
        (status = 500, description = "The update client is absent or another operation is running (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
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
            state.audit.record("update-check", "requested", &source);
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
        (status = 500, description = "The update client is absent or another operation is running (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
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
            state.audit.record("update-fetch", "requested", &source);
            accepted()
        }
        Err(err) => update_bus_error(&err),
    }
}

/// `POST /api/v1/update/install` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstallRequest {
    /// Absolute path of the bundle to install. Omitted = the staged bundle
    /// the last fetch/import verified (the lifecycle's `bundle`).
    #[serde(default)]
    bundle_path: Option<String>,
}

/// Install a bundle through mosd.
///
/// With no body (or no `bundlePath`), installs the bundle the lifecycle has
/// staged as `ready` — the path `rauc-update` verified. With `bundlePath`,
/// forwards that explicit operator path to `InstallUpdate` unchanged, which
/// is the manual/offline route after `rauc-update import`; mosd admits it
/// only when it is a regular file inside `/mos/updates/verified` and not a
/// `.part`, so a partial or a file anywhere else is never handed to RAUC.
/// Answers **202**: the install runs on mosd's background task; poll the
/// state document.
#[utoipa::path(
    post,
    path = V1_UPDATE_INSTALL_PATH,
    context_path = API,
    tag = "update",
    request_body = InstallRequest,
    responses(
        (status = 202, description = "The install was admitted and is running; poll `GET /api/v1/update`"),
        (status = 400, description = "The body is not JSON or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "Outside the maintenance window or the policy file is unreadable (`policy_refused`); or nothing is staged and no path was given (`no_staged_bundle`)", body = ApiError),
        (status = 422, description = "The bundle path is not an absolute existing regular file inside `/mos/updates/verified`, or is a `.part` partial (`validation_failed`)", body = ApiError),
        (status = 500, description = "An install is already running, or mosd failed (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_install(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<InstallRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // A JSON body is required (`{}` for "install what is staged"): an
    // unparseable body must be a 400, never a silently defaulted install.
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    let explicit = request.bundle_path;
    let bundle = match explicit {
        Some(path) => path,
        // The staged path comes from the recorded lifecycle rather than a
        // guess: it is the last verified output `rauc-update` printed, and
        // there is no other path this route will fill in.
        None => match state.api.get_state("update.lifecycle").await {
            Ok(lifecycle) => match lifecycle.get("bundle").and_then(Value::as_str) {
                Some(path) => path.to_string(),
                None => {
                    return api_response(
                        StatusCode::CONFLICT,
                        ApiError::apid(
                            "no_staged_bundle",
                            "no verified bundle is staged; run a fetch or import first, \
                             or name a bundlePath explicitly"
                                .to_string(),
                        ),
                    );
                }
            },
            Err(err) => return update_bus_error(&err),
        },
    };
    match state.api.install_update(&bundle).await {
        Ok(()) => {
            state.audit.record("update-install", "requested", &source);
            accepted()
        }
        Err(err) => update_bus_error(&err),
    }
}

/// `POST /api/v1/update/mark` request body: mosd's offered vocabulary,
/// verbatim.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MarkRequest {
    /// `good` or `bad`.
    state: String,
    /// `booted` or `other`. Concrete slot names and `active` are refused by
    /// mosd — activation is the installer's job.
    slot: String,
}

/// The mark's answer: RAUC's resolved slot name and message.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarkResponse {
    slot_name: String,
    message: String,
}

/// Manually mark a slot good or bad.
///
/// The operator escape hatch over the boot health gate, forwarded to mosd's
/// `MarkUpdate` (the gate owns the automatic confirm; mosd never marks on
/// its own).
#[utoipa::path(
    post,
    path = V1_UPDATE_MARK_PATH,
    context_path = API,
    tag = "update",
    request_body = MarkRequest,
    responses(
        (status = 200, description = "The mark was applied; RAUC's resolved slot name and message", body = MarkResponse),
        (status = 400, description = "The body is not JSON or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 422, description = "A state outside `good`/`bad` or a slot outside `booted`/`other` (`validation_failed`)", body = ApiError),
        (status = 500, description = "RAUC refused or failed the mark (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_mark(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<MarkRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => return body_rejection(rejection),
    };
    match state.api.mark_update(&request.state, &request.slot).await {
        Ok((slot_name, message)) => {
            state.audit.record("update-mark", &request.state, &source);
            api_response(StatusCode::OK, MarkResponse { slot_name, message })
        }
        Err(err) => update_bus_error(&err),
    }
}

/// The guard's refusal reasons, exactly as `rollback_eligibility` in
/// `pkgs/mosd/mosd/src/rauc.rs` writes them into the state document.
///
/// Listed here because `ApiError`'s code is a `&'static str` and mosd's
/// verdict arrives as a bus string: this is the vocabulary apid serves, and a
/// reason outside it is answered as the generic `rollback_refused` carrying
/// mosd's word verbatim, so a renamed reason degrades to something honest
/// instead of being silently reported as one of these.
const ROLLBACK_REASONS: [&str; 7] = [
    "no_alternate_slot",
    "alternate_is_booted_slot",
    "alternate_never_installed",
    "alternate_marked_bad",
    "alternate_is_newer",
    "install_order_unknown",
    "booted_slot_not_confirmed",
];

/// What a rollback did: RAUC's answer to the mark, and the slot the next boot
/// will therefore come from.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RollbackResponse {
    /// RAUC's resolved name for the slot that was marked bad — the one this
    /// system is running from.
    slot_name: String,
    /// RAUC's own message for the mark.
    message: String,
    /// The slot the next boot comes from.
    target: String,
    /// What the operator must still do; this route deliberately does not.
    next_step: String,
}

/// mosd's verdict as an error code: itself when this surface knows it, and
/// the generic `rollback_refused` when it does not.
fn rollback_reason_code(reason: Option<&str>) -> &'static str {
    reason
        .and_then(|reason| ROLLBACK_REASONS.into_iter().find(|known| *known == reason))
        .unwrap_or("rollback_refused")
}

/// **409** for a rollback the device's slot state forbids.
///
/// Not 422: the request is well formed and authenticated, and it is the
/// device that says no. The reason from mosd is the error code when it is one
/// this surface knows, and the message carries it either way.
fn rollback_refused(reason: Option<&str>) -> Response {
    let code = rollback_reason_code(reason);
    api_response(
        StatusCode::CONFLICT,
        ApiError::apid(
            code,
            format!(
                "the device's slot state does not permit a rollback ({}); \
                 read `rollback` in `GET /api/v1/update` for the resolved target",
                reason.unwrap_or("the update state carries no rollback verdict"),
            ),
        ),
    )
}

/// Roll back to the alternate slot, if the guard permits it.
///
/// The guard is mosd's `rollback_eligibility`, read out of the same
/// `GetUpdateState` document `GET /api/v1/update` serves — one derivation of
/// the slot state, not a second one here. It refuses when there is no
/// alternate slot, when the alternate is the booted slot, when the alternate
/// was never written or is marked bad, when the alternate is NOT the older of
/// the two installs (a rollback goes backward; a newer or unorderable target
/// is a pending update, not a rollback target), and when the booted slot is
/// itself pending-not-confirmed (that window belongs to the bootloader's
/// attempt counter, and a manual rollback inside it races the credit being
/// spent).
///
/// What it then does is ONE mark: `bad` on the **booted** slot. That is what
/// makes the bootloader pick the other one, and it is why this route cannot
/// confirm the slot it rolls back to — PLAN-048's "cannot mark an unverified
/// slot good" holds structurally, not by review. The unguarded
/// `POST /api/v1/update/mark` remains the operator escape hatch beside it;
/// this route is the guarded one.
///
/// It does NOT reboot. A rollback is a boot-order change, and the reboot that
/// realises it goes through the safe-to-reboot gate like every other
/// (`docs/design/updates.md` §4) — folding it in here would either bypass
/// that gate or duplicate its override semantics. The answer names the next
/// step instead.
#[utoipa::path(
    post,
    path = V1_UPDATE_ROLLBACK_PATH,
    context_path = API,
    tag = "update",
    responses(
        (status = 200, description = "The booted slot was marked bad; the next boot comes from `target`. Reboot with `POST /api/v1/actions/reboot`", body = RollbackResponse),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "The device's slot state forbids it: `no_alternate_slot`, `alternate_is_booted_slot`, `alternate_never_installed`, `alternate_marked_bad`, `alternate_is_newer`, `install_order_unknown`, `booted_slot_not_confirmed`, or `rollback_refused` for a verdict this surface does not know", body = ApiError),
        (status = 500, description = "RAUC refused or failed the mark, or mosd could not read the slot state (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_update_rollback(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    // No request body: this action takes no parameters, and a slot it could
    // name is a slot the guard did not resolve.
    let update = match state.api.get_update_state().await {
        Ok(update) => update,
        Err(err) => return update_bus_error(&err),
    };
    let rollback = update.get("rollback");
    let permitted = rollback
        .and_then(|rollback| rollback.get("permitted"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let reason = rollback
        .and_then(|rollback| rollback.get("reason"))
        .and_then(Value::as_str);
    // A permitted verdict always names its target; a document that permits
    // without one is refused rather than acted on, so the answer can never
    // claim a slot the guard did not resolve.
    let target = rollback
        .and_then(|rollback| rollback.get("target"))
        .and_then(Value::as_str);
    let (Some(target), true) = (target, permitted) else {
        let code = rollback_reason_code(reason);
        state
            .audit
            .record("update-rollback", &format!("refused: {code}"), &source);
        return rollback_refused(reason);
    };
    let target = target.to_string();
    // The one mark a rollback emits; `RollbackEligibility::mark` in
    // `pkgs/mosd/mosd/src/rauc.rs` is where that is a proven invariant.
    match state.api.mark_update("bad", "booted").await {
        Ok((slot_name, message)) => {
            state
                .audit
                .record("update-rollback", &format!("to {target}"), &source);
            api_response(
                StatusCode::OK,
                RollbackResponse {
                    slot_name,
                    message,
                    target,
                    next_step: "POST /api/v1/actions/reboot".to_string(),
                },
            )
        }
        Err(err) => update_bus_error(&err),
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

/// The armed override, as mosd recorded it.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RebootOverride(Value);

/// Arm the bounded administrative override of the safe-to-reboot gate.
///
/// Lifts health-report blocks for the TTL — never an install in flight —
/// and expires on its own. Audited on both sides: this route records the
/// event, and mosd logs who armed it and until when.
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
        (status = 500, description = "mosd failed to arm it (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
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

/// The empty 202 the three long-running admissions answer: accepted, running
/// on mosd's background task, outcome via the state document.
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
