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
//! likewise no route that returns the secret-bearing import document,
//! re-applies it, or clears the record.
//!
//! **It returns no value the document carried.** The document's own fields are
//! applied into the subtrees that own them; what this serves is the applied
//! version, the digest, the last import attempt and whether the device is
//! still unclaimed. Everything it does serve is passed through
//! [`crate::redact`] on the way out, so a secret-named field that ever
//! appeared under `provisioning` would be substituted rather than served —
//! `docs/design/api.md` §2.2's rule, applied to a third root for the reason it
//! is applied to the second. The separate baked manifest is public build
//! configuration and is returned whole with digests of its allowlisted files.
//!
//! **`operator` and `effective` do not pass through that redactor, and do not
//! borrow the baked half's argument for it either** (PLAN-070 §8's last
//! paragraph forbids exactly that). `/mos/config/` is credential material and
//! is never returned whole; what is returned is
//! [`mosd_settings::configuration::provisioning_status`]'s projection over
//! three named fields, built key by key rather than serialized from the
//! document. A key the projection does not name cannot reach a caller however
//! the schema grows — which is the allowlist argument again, applied to a
//! document that may not be served whole, and it is stronger here than the
//! fail-open denylist would be.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use mosd_settings::configuration;
use serde_json::Value;
use sha2::{Digest, Sha256};

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
    /// The public configuration baked into the verity root, as read.
    baked: Value,
    /// SHA-256 by relative path under `/usr/share/mos/meta/`.
    baked_digests: BTreeMap<String, String>,
    /// What the operator wrote in `/mos/config/updates.json`, projected onto
    /// `update.source`, `update.channel` and `update.policy` and nothing else.
    ///
    /// A field the operator did not write is **absent**; one they wrote as
    /// `null` is **`null`**. Both resolve to the baked default and they are
    /// still two different statements about the document, so the reading is
    /// preserved rather than flattened. `{}` when the document overrides
    /// nothing, which is a factory-reset device (PLAN-070 §4.1).
    operator: Value,
    /// What this device actually runs on after §5.1's precedence:
    /// `update.source`, `update.channel`, `update.policy`, and `fleet.url` /
    /// `fleet.enabled`.
    ///
    /// The fleet pair is the **baked** value and is not a placeholder for a
    /// resolution not yet written: `/mos/config/`'s fleet document is
    /// PLAN-072 §2's and does not exist, so nothing on the device reads an
    /// operator layer for it either. When that slice lands it extends the
    /// library's resolver; it does not add a second one here.
    effective: Value,
}

/// Read only the allowlisted public files. Never traverse a link or expose an
/// unexpected file merely because it appeared beside the public manifest.
///
/// Addressed by the manifest, and the tree derived from it, so this walk and
/// the resolver that parses the manifest cannot be looking at two different
/// trees. The derivation is checked rather than assumed: a manifest with no
/// grandparent directory is an error here, not a panic, because the path can
/// come from a caller.
fn baked_configuration(manifest_path: &Path) -> Result<(Value, BTreeMap<String, String>)> {
    let root = manifest_path
        .parent()
        .and_then(Path::parent)
        .with_context(|| {
            format!(
                "{} has no baked metadata tree above it",
                manifest_path.display()
            )
        })?;
    let mut files = BTreeMap::new();
    read_baked_files(root, root, &mut files)?;
    let manifest = files
        .get("updates/manifest.json")
        .context("baked manifest is missing")?;
    let document: Value = serde_json::from_slice(manifest).context("parse baked manifest")?;
    ensure!(
        document["schema"] == "mos/meta/v1",
        "unsupported baked manifest schema"
    );
    let digests = files
        .into_iter()
        .map(|(path, bytes)| (path, format!("{:x}", Sha256::digest(bytes))))
        .collect();
    Ok((document, digests))
}

fn read_baked_files(root: &Path, at: &Path, files: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    ensure!(
        std::fs::symlink_metadata(at)?.is_dir(),
        "baked metadata directory is not a directory"
    );
    let entries = std::fs::read_dir(at).context("read baked metadata directory")?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .context("non-UTF-8 baked path")?
            .to_string();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            ensure!(
                relative == "updates",
                "unexpected baked directory {relative}"
            );
            read_baked_files(root, &path, files)?;
        } else {
            ensure!(
                kind.is_file(),
                "baked path {relative} is not a regular file"
            );
            ensure!(
                matches!(relative.as_str(), "updates/manifest.json" | "GENERATED"),
                "unexpected baked file {relative}"
            );
            let bytes = std::fs::read(&path).context("read baked public file")?;
            ensure!(!bytes.is_empty(), "baked file {relative} is empty");
            files.insert(relative, bytes);
        }
    }
    Ok(())
}

/// Read what a provisioning document did to this device.
///
/// Two settings reads plus the public baked configuration: the record mosd
/// wrote when it last met a document, and whether an administrator credential
/// exists. Nothing is observed from a medium at request time — the media are staged and read
/// before anything is listening, so a request cannot make a device look at a
/// stick.
#[utoipa::path(
    get,
    path = V1_PROVISIONING_STATUS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "Import history and claim state, the public baked manifest with SHA-256 digests by baked file path, and the operator and effective readings of `update.source`, `update.channel` and `update.policy` beside the baked `fleet` pair. The secret-bearing import document is never returned.", body = ProvisioningStatus),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to answer (`mosd_failed`), the baked configuration could not be read (`baked_configuration_unavailable`), or `/mos/config/updates.json` did not read, parse or validate (`configuration_unavailable`) — which is refused rather than answered with the baked value", body = ApiError),
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
    // ONE path for both halves of layer 1. `meta_manifest` names the file the
    // resolver parses and the tree the digests cover is derived from it, so a
    // response cannot report one device's baked manifest beside another's
    // digests.
    let manifest_path = state.meta_manifest.as_path();
    let (baked, baked_digests) = match baked_configuration(manifest_path) {
        Ok(value) => value,
        Err(err) => {
            return api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::mosd("baked_configuration_unavailable", format!("{err:#}")),
            );
        }
    };
    // F9's operator/effective half, through the library resolver rather than
    // a second reader here (RFCT-313 owns it; apid links `mosd-settings` and
    // cannot link the `mosd` binary). One resolver is the whole point: this
    // route and the update path answering differently about the same device is
    // the failure the arrangement exists to prevent.
    //
    // `provisioning_status_at` and not `provisioning_status`: the no-argument
    // form reads the production manifest, which would be a SECOND reading of
    // layer 1 beside the digests above. `updates_path` remains the fixed
    // production operator document outside tests; its private test seam lets
    // route tests use an isolated file without changing that source.
    //
    // A layer-2 read, parse, anchor-key or validation failure is an error
    // HERE TOO. The route refusing and the update path refusing are one fact
    // reaching two surfaces, and a baked value served in the `effective` slot
    // would read as correct on every device that has overridden nothing —
    // which is all of them today, so nothing would catch it.
    let updates_path = state.updates_path.as_path();
    let mut resolved = match configuration::provisioning_status_at(manifest_path, updates_path) {
        Ok(value) => value,
        Err(err) => {
            return api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::mosd("configuration_unavailable", format!("{err:#}")),
            );
        }
    };
    let operator = resolved["operator"].take();
    let effective = resolved["effective"].take();
    api_response(
        StatusCode::OK,
        ProvisioningStatus {
            baked,
            baked_digests,
            operator,
            effective,
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
