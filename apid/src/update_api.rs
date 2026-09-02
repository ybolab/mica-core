//! The update cluster: one state read and five actions over mosd's update
//! surface (`GetUpdateState`, `CheckUpdate`, `FetchUpdate`, `InstallUpdate`,
//! `MarkUpdate`, `SetRebootOverride`).
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
pub(crate) const V1_UPDATE_REBOOT_OVERRIDE_PATH: &str = "/v1/update/reboot-override";

/// The D-Bus error name mosd's update surface refuses policy-forbidden
/// actions with. Not in `routes.rs`'s table: only this cluster produces it.
const FDO_ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";

/// The update state document, verbatim from mosd: `lifecycle` (state machine,
/// policy, reboot gate), per-slot status, `booted_slot`, `primary`,
/// `pending_not_confirmed`, `install` and `last_mark`.
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
/// download or install, and it is never answered from a stale record.
#[utoipa::path(
    get,
    path = V1_UPDATE_PATH,
    context_path = API,
    tag = "update",
    responses(
        (status = 200, description = "The update state: `lifecycle` (state machine with reason strings — `update-unavailable` carries the `/mos/updates` workspace's `unavailable`/`degraded` verdict, mirrored under `lifecycle.workspace` —, effective policy, safe-to-reboot gate and override), per-slot status, `booted_slot`, `primary`, `pending_not_confirmed`, `install`, `last_mark`", body = UpdateState),
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
