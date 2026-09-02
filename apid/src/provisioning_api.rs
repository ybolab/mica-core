//! The provisioning-document status: one read-only route over what a
//! provisioning document did to this device (PLAN-046 / RFCT-282).
//!
//! A bounded module beside `routes.rs` rather than more of it, on
//! `update_api.rs`'s reasoning: the route shares that file's session gate
//! ([`ApiCredential`]), envelope and response helper, and adds no mapping of
//! its own.
//!
//! **There is exactly one route here and it is a GET.** The write side of this
//! capability is a file on a boot medium or a stick, applied by mosd before
//! anything is listening, and that is deliberate — a document is the channel
//! for a device with NO network, so an HTTP route that applied one would be a
//! second, differently-trusted write path for the same thing. There is
//! likewise no route that returns the document, re-applies it, or clears the
//! record.
//!
//! **It returns no value the document carried.** The document's own fields are
//! applied into the subtrees that own them; what this serves is the applied
//! version, the digest, the last import attempt and whether the device is
//! still unclaimed. Everything it does serve is passed through
//! [`crate::redact`] on the way out, so a secret-named field that ever
//! appeared under `provisioning` would be substituted rather than served —
//! `docs/design/api.md` §2.2's rule, applied to a third root for the reason it
//! is applied to the second.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

use crate::redact;
use crate::routes::{
    API, ApiCredential, ApiError, AppState, V1_PROVISIONING_STATUS_PATH, api_response,
    bus_api_error,
};

/// The settings subtree the record lives in.
const PROVISIONING_PATH: &str = "provisioning";

/// The settings subtree that answers "is this device claimed".
const ACCESS_PATH: &str = "access";

/// What a provisioning document did to this device.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProvisioningStatus {
    /// `version` of the document last applied; `null` when none ever was.
    document_version: Option<u64>,
    /// Canonical digest of the document last applied; `null` with the version.
    document_digest: Option<String>,
    /// The last import ATTEMPT: `source` (`boot` or `media`), `outcome`
    /// (`applied`, `unchanged` or `rejected`), `reason` for a rejection, and
    /// `at`, the device clock's reading when it happened. `null` when no
    /// medium has ever been offered to this device.
    last_import: Option<Value>,
    /// Whether the device still has no administrator credential.
    ///
    /// The same question `GET /api/v1/session` answers with `state: "setup"`,
    /// asked here because it is what decides whether a document offered on a
    /// medium would be applied at all: a claimed device refuses one.
    unclaimed: bool,
}

/// Read what a provisioning document did to this device.
///
/// Two settings reads and no other source: the record mosd wrote when it last
/// met a document, and whether an administrator credential exists. Nothing is
/// observed from a medium at request time — the media are staged and read
/// before anything is listening, so a request cannot make a device look at a
/// stick.
#[utoipa::path(
    get,
    path = V1_PROVISIONING_STATUS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The applied document's version and canonical digest (`null` when none was ever applied), the last import attempt with its source, outcome, rejection reason and clock reading, and whether the device is still unclaimed. No value the document carried is returned.", body = ProvisioningStatus),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to answer (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_provisioning_status(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    let provisioning = match state.api.get_settings(PROVISIONING_PATH).await {
        Ok(value) => redact::redact(value, PROVISIONING_PATH),
        Err(err) => return bus_api_error(&err, Some(PROVISIONING_PATH)),
    };
    // Redacted too, although only the PRESENCE of the hash is read: this
    // handler must be unable to serve the value even if it were changed to
    // pass the subtree through, which is what makes the rule structural rather
    // than a property of these particular lines.
    let access = match state.api.get_settings(ACCESS_PATH).await {
        Ok(value) => redact::redact(value, ACCESS_PATH),
        Err(err) => return bus_api_error(&err, Some(ACCESS_PATH)),
    };
    let document = provisioning.get("document");
    api_response(
        StatusCode::OK,
        ProvisioningStatus {
            document_version: document
                .and_then(|record| record.get("appliedVersion"))
                .and_then(Value::as_u64),
            document_digest: document
                .and_then(|record| record.get("appliedDigest"))
                .and_then(Value::as_str)
                .map(str::to_string),
            last_import: document
                .and_then(|record| record.get("lastImport"))
                .cloned(),
            // A device is claimed exactly when it carries an administrator
            // password hash, which is the same predicate the session route
            // reads to decide `setup`. Read as "is the hash there", never as
            // "what is the hash": the value arrives redacted and its presence
            // is all this needs.
            unclaimed: access
                .get("webAdmin")
                .and_then(|admin| admin.get("password_hash"))
                .is_none(),
        },
    )
}
