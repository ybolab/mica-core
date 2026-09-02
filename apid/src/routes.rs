//! HTTP routing for the JSON management API and its two UI entry points.
//!
//! `/api/` owns every management read and mutation. `/_ui` is the built-in SPA
//! embedded in the binary. `/` serves a valid active custom bundle and
//! otherwise redirects to `/_ui/`; custom assets are considered only by the
//! final fallback, so neither UI can shadow the API or health endpoint.

use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
// `PathBuf` and not `Path`: axum's own `Path` extractor is imported below, and
// the filesystem type of that name would shadow it.
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;

use axum::body::Body;
use axum::extract::{FromRequestParts, OriginalUri, Path, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, HOST, LOCATION, RETRY_AFTER,
    SET_COOKIE,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
// `delete` and `put` are imported on their own lines rather than folded into
// the routing import below, which is how they were added: apid had served no
// write verb at all until these two arrived.
use axum::routing::delete;
use axum::routing::put;
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use mosd_settings::{
    ApiToken, AuthorizedKey, ClaimChannel, ClaimSettings, IfaceKind, IfaceSettings,
    MIN_ADMIN_PASSWORD_LEN, ResetTier, SettingsError, WifiNetwork, WireguardPeer,
    parse_authorized_key, quote_path_segment, validate_api_tokens, validate_authorized_keys,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::access_cache::{ACCESS_PATH, AccessCache};
use crate::assets::mime::CacheClass;
use crate::assets::serve;
use crate::audit::{Audit, Source};
use crate::auth::{self, GuardStore};
use crate::bundle::{CandidateUnavailable, CustomCandidate, Installed, Rejection, Store};
use crate::diagnostics::{self, Collector, SnapshotStore, SnapshotSummary};
use crate::redact;
use crate::session::{self, SessionStore};
use crate::settings_api::{InvalidTaskPayload, SettingsApi};
use crate::task_registry::{TaskRecord, TaskRegistry};
use crate::token;

/// Shared handler state.
#[derive(Clone)]
pub struct AppState {
    pub(crate) api: Arc<dyn SettingsApi>,
    sessions: Arc<SessionStore>,
    guard: Arc<GuardStore>,
    pub(crate) audit: Arc<Audit>,
    bundles: Arc<Store>,
    /// Serialises custom-UI pointer mutations so concurrent API requests are
    /// deterministic and cannot contend for the atomic replacement link.
    ui_selection: Arc<tokio::sync::Mutex<()>>,
    /// The gate's cache of the `access` subtree, kept honest by the
    /// `SettingsChanged` watcher (`bus_client::watch_settings_changed`) and
    /// by the two handlers that write under `access` themselves.
    access_cache: Arc<AccessCache>,
    /// Notification-fed apply-task mirror. It serves only while its
    /// `TaskChanged` subscription is live.
    task_registry: Arc<TaskRegistry>,
    /// The diagnostic snapshot store (PLAN-052): `/mos/diagnostics` on a
    /// device, a temporary directory in tests. A path and no syscall until
    /// the first publish.
    diagnostics: Arc<SnapshotStore>,
    /// Serialises snapshot collection. One at a time: a second request while
    /// one runs is refused (409) rather than queued, because each one reads
    /// every mosd surface and the second would only repeat the first.
    collecting: Arc<tokio::sync::Mutex<()>>,
    /// The ONE physical-presence seam (`docs/design/recovery.md` §4), keyed by
    /// the [`PRESENCE_CAPABILITY`] board capability. Every presence-gated
    /// operation asks this and nothing else.
    presence: Arc<dyn Presence>,
}

impl AppState {
    /// State around a settings backend and the cookie signing key.
    ///
    /// The bundle store is constructed here and reads nothing: §6.1 forbids
    /// bundle discovery before the listeners bind, and `Store::at_default` is
    /// a path and no syscall. Discovery and the start-up compatibility
    /// re-check are separate work.
    /// The backoff counter and the audit trail default to their
    /// non-persistent forms so that constructing state needs no filesystem;
    /// `with_persistence` is what production calls, and access.md §6's "a
    /// power cycle must not reset the clock" is that call, not this one.
    pub fn new(api: Arc<dyn SettingsApi>, signing_key: [u8; 32]) -> Self {
        Self {
            api,
            sessions: Arc::new(SessionStore::new(signing_key)),
            guard: Arc::new(GuardStore::ephemeral()),
            audit: Arc::new(Audit::journal_only()),
            bundles: Arc::new(Store::at_default()),
            ui_selection: Arc::new(tokio::sync::Mutex::new(())),
            access_cache: Arc::new(AccessCache::new()),
            task_registry: Arc::new(TaskRegistry::new()),
            diagnostics: Arc::new(SnapshotStore::at_default()),
            collecting: Arc::new(tokio::sync::Mutex::new(())),
            // A path and no syscall, like the bundle and snapshot stores: the
            // marker is read when something asks for presence and never at
            // construction.
            presence: Arc::new(ConsolePresence::at_default()),
        }
    }

    /// The gate's access cache, for `main.rs` to hand to the
    /// `SettingsChanged` watcher, and for the tests that drive its
    /// subscription state by hand.
    pub(crate) fn access_cache(&self) -> &Arc<AccessCache> {
        &self.access_cache
    }

    pub(crate) fn task_registry(&self) -> &Arc<TaskRegistry> {
        &self.task_registry
    }

    /// One task from the live registry, or a bounded direct bus read while the
    /// subscription is unavailable. The generation check prevents that read
    /// from overwriting a signal that arrived while it was in flight.
    async fn task_record(&self, id: &str) -> anyhow::Result<TaskRecord> {
        if let Some(task) = self.task_registry.get(id) {
            return Ok(task);
        }
        let stale = self.task_registry.stale(id);
        let generation = self.task_registry.generation();
        let task = match self.api.get_task(id).await {
            Ok(task) => task,
            Err(err) if is_task_not_found(&err) && stale.is_some() => {
                let mut task = stale.expect("matched Some above");
                if !task.terminal() {
                    task.status = "finished".to_string();
                    task.finished_at = Some(
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    );
                    task.outcome = Some("interrupted".to_string());
                    task.message = Some(
                        "mosd restarted or rolled over its bounded task history before this apply's terminal signal was retained; startup reconciliation converges persisted settings"
                            .to_string(),
                    );
                }
                task
            }
            Err(err) => return Err(err),
        };
        self.task_registry.fill(generation, task.clone());
        Ok(task)
    }

    /// The bounded task history from the live signal mirror, or from mosd's
    /// live-state snapshot while the subscription is unavailable.
    async fn task_records(&self) -> anyhow::Result<Vec<TaskRecord>> {
        if let Some(tasks) = self.task_registry.list() {
            return Ok(tasks);
        }
        let generation = self.task_registry.generation();
        let state = self.api.get_state("").await?;
        let tasks = match state.get("tasks") {
            Some(tasks) => serde_json::from_value(tasks.clone()).map_err(InvalidTaskPayload)?,
            None => Vec::new(),
        };
        self.task_registry.fill_list(generation, tasks.clone());
        Ok(self.task_registry.list().unwrap_or(tasks))
    }

    /// Root the backoff counter and the audit ring in `state_dir`
    /// (`docs/design/access.md` §6).
    ///
    /// The directory must already exist — `main.rs` creates it before this is
    /// called, on the same path that holds the TLS material.
    pub fn with_persistence(mut self, state_dir: &std::path::Path) -> Self {
        self.guard = Arc::new(GuardStore::load(state_dir.join("login_guard.json")));
        self.audit = Arc::new(Audit::at(state_dir.to_path_buf()));
        self
    }

    /// The `/mos/ui` bundle store the asset router reads (§5.2).
    pub(crate) fn bundles(&self) -> &Store {
        &self.bundles
    }

    /// The audit sink, for the start-up path (`main.rs` hands it to bundle
    /// discovery so a staged custom UI's activation is recorded too).
    pub(crate) fn audit(&self) -> &Arc<Audit> {
        &self.audit
    }

    /// Root the bundle store somewhere else, for tests that install one.
    ///
    /// Test-only on purpose: §5.2 fixes the shipped location and nothing
    /// configures it.
    #[cfg(test)]
    pub fn with_bundle_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.bundles = Arc::new(Store::new(root));
        self
    }

    /// Root the diagnostic snapshot store somewhere else, for tests.
    ///
    /// Test-only for the bundle root's reason: the shipped location is
    /// fixed and nothing configures it.
    #[cfg(test)]
    pub fn with_diagnostics_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.diagnostics = Arc::new(SnapshotStore::new(root));
        self
    }

    /// Drive the presence seam from a test.
    ///
    /// Test-only, and it is the reason the seam is a trait: a presence
    /// assertion is an action at the DEVICE (`docs/design/recovery.md` §4.2),
    /// so nothing a test can do to a shipped build establishes one, and
    /// nothing a shipped build offers can either — which is the property the
    /// gate is for.
    #[cfg(test)]
    pub fn with_presence(mut self, presence: Arc<dyn Presence>) -> Self {
        self.presence = presence;
        self
    }
}

/// The HTTPS application router.
///
/// §4.1's precedence rule is this function's declaration order, and it is
/// total. Rules 1-3 are `.route`/`.nest` declarations and rule 4 is the
/// `.fallback`; axum matches declared routes before it consults a fallback, so
/// a bundle that ships a file at `api/v1/settings`, at `healthz` or at `login`
/// cannot capture any of them. The custom asset resolver additionally rejects
/// ambiguous encoded or repeated-separator spellings of the reserved roots;
/// those aliases fail closed instead of being normalised into another domain.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve::root))
        .nest(
            "/_ui",
            Router::new()
                .route("/", get(crate::assets::builtin::index))
                .route("/{*path}", get(crate::assets::builtin::serve)),
        )
        .route("/_ui/", get(crate::assets::builtin::index))
        .route("/healthz", get(healthz))
        .nest(API, api_router())
        .route("/api/", any(api_not_found))
        .fallback(serve::fallback)
        .with_state(state)
}

/// The reserved subtree's own not-found handler (§4.1 rule 1, §4.2's "why
/// 404s inside `/api/` are the API's own").
///
/// §2.4's envelope, which is what makes a mistyped path a machine-readable
/// answer rather than an empty body.
async fn api_not_found(OriginalUri(uri): OriginalUri) -> Response {
    api_response(
        StatusCode::NOT_FOUND,
        ApiError::apid("not_found", format!("no API route at {}", uri.path())),
    )
}

// §2.1's API surface: the reserved subtree's declared routes, their bodies
// and the session check that guards them.

/// The reserved prefix, and the paths §2.1 declares under it.
///
/// The leaves are the paths as the nested router sees them; the OpenAPI
/// document composes them with the prefix through `context_path`, and
/// [`is_declared_api_route`] composes them to get what the gate sees. One
/// spelling each.
pub(crate) const API: &str = "/api";
const VERSIONS_PATH: &str = "/versions";
const V1_META_PATH: &str = "/v1/meta";

/// §2.4 case 3's second, differently-scoped health endpoint.
///
/// Not `/healthz` and never a replacement for it: `/healthz` answers *"is
/// apid's listener up"* and this answers *"is this appliance manageable"*.
/// Both sentences are true and neither implies the other, which is why there
/// are two paths and not one.
const V1_HEALTH_PATH: &str = "/v1/health";

/// The live-state key the health route probes, and the value it reports as
/// `checkedAt`.
///
/// One bus call answers both questions §2.4 case 3 asks. It proves the round
/// trip — mosd serves this key by reading `/proc/uptime` at request time
/// (`docs/design/api.md` §2.2 item 3), so a value coming back means a real
/// exchange happened and not that a cached flag was read — and the value it
/// returns is the only clock on this appliance a health answer may be stamped
/// with, there being no trusted wall clock anywhere in the crate (§3.2's
/// expiry paragraph).
const HEALTH_PROBE_PATH: &str = "uptime";

/// §2.3's actions namespace, with its one shipped verb. A password change is
/// an operation and not a resource — the namespace is named `actions`
/// precisely so no reader expects a `GET` to work there.
const V1_CHANGE_PASSWORD_PATH: &str = "/v1/actions/change-password";

/// §2.2's two read-only roots, in the three spellings they need.
///
/// The prefix is the shared one and the only one the gate predicate tests. The
/// other two exist because axum names a wildcard segment `{*path}` and OpenAPI
/// names a template parameter `{path}`, so the served path and the documented
/// path cannot be the same string; `the_resource_path_spellings_agree` holds
/// them to the prefix so they cannot drift apart.
#[cfg(test)]
const V1_SETTINGS_PREFIX: &str = "/v1/settings/";
const V1_SETTINGS_ROUTE: &str = "/v1/settings/{*path}";
const V1_SETTINGS_DOC: &str = "/v1/settings/{path}";
#[cfg(test)]
const V1_STATE_PREFIX: &str = "/v1/state/";
const V1_STATE_ROUTE: &str = "/v1/state/{*path}";
const V1_STATE_DOC: &str = "/v1/state/{path}";

/// §2.1's action route for a WireGuard key rotation. A new route, and
/// therefore additive.
///
/// One spelling and not three, unlike the resource roots above: `{iface}` is a
/// single-segment parameter, which axum and OpenAPI spell the same way, so
/// there is nothing here for a test to hold together. The prefix and the leaf
/// exist separately because [`is_declared_api_route`] has to recognise the
/// shape without a router to ask.
/// §3.2's token collection and its item route.
///
/// The item route needs its prefix separately for the same reason the rotate
/// action does: [`is_declared_api_route`] has to recognise the shape with no
/// router to ask.
const V1_TOKENS_PATH: &str = "/v1/tokens";
const V1_TOKEN_ROUTE: &str = "/v1/tokens/{id}";

/// The dot-path the token collection lives at, which every envelope raised
/// about it names.
const API_TOKENS_PATH: &str = "access.apiTokens";

const V1_WIREGUARD_ROTATE_ROUTE: &str = "/v1/actions/wireguard/{iface}/rotate-key";

/// M7's three verbs.
///
/// No collection, no identifier and nothing to read back, so each is one
/// constant rather than the prefix/route/doc triple a resource path needs.
const V1_REBOOT_PATH: &str = "/v1/actions/reboot";
const V1_POWEROFF_PATH: &str = "/v1/actions/poweroff";
const V1_TRANSIENT_PASSWORD_PATH: &str = "/v1/actions/transient-root-password";

/// Apply-task history and one task by id.
const V1_TASKS_PATH: &str = "/v1/tasks";
const V1_TASK_ROUTE: &str = "/v1/tasks/{id}";

/// The read-only time-synchronization status (PLAN-044).
///
/// A fixed path like the action verbs, not a resource triple: the status is
/// observed from timesyncd at request time, names no dot-path, and has no
/// write counterpart — there is deliberately no route beside it that could
/// pause or stop synchronization.
const V1_TIME_STATUS_PATH: &str = "/v1/time/status";

/// The read-only storage status (PLAN-049).
///
/// A fixed path on the time status's reasoning, and one more besides: this is
/// the whole of the storage surface. There is no sibling constant here for a
/// format, repartition, resize, wipe or mount action, and
/// [`crate::tests`]'s route-surface scan asserts that there is not — the
/// layout is fixed by the image assembler, and an API that could rewrite it
/// would be a remote destructive surface with no product use.
const V1_STORAGE_STATUS_PATH: &str = "/v1/storage/status";

/// The system-information surface (PLAN-052): what this device is, in one
/// read. A fixed path on the time status's reasoning: observed at request
/// time, no dot-path, no write counterpart.
const V1_SYSTEM_INFO_PATH: &str = "/v1/system/info";

/// Board telemetry (PLAN-052): temperature, watchdog, reset reason. Same
/// shape and same reasoning.
const V1_SYSTEM_TELEMETRY_PATH: &str = "/v1/system/telemetry";

/// The OBSERVED network state (PLAN-052), distinct from `/v1/network`'s
/// desired map: carrier, addresses, lease, routes, DNS, association and the
/// radio capabilities, and nothing from the settings tree.
///
/// A static segment under the prefix `/v1/network/{iface}` is declared on:
/// the router matches the static route first, so an interface literally
/// named `status` is not addressable through the item route. That is
/// documented in `docs/design/diagnostics.md` and is the cost of spelling
/// the three status surfaces the same way.
const V1_NETWORK_STATUS_PATH: &str = "/v1/network/status";

/// The diagnostic snapshot collection and its item route (PLAN-052).
///
/// The collection takes `GET` (list) and `POST` (collect one, bounded); the
/// item takes `GET` (export) and `DELETE` (the explicit retention
/// operation). There is no route that uploads a snapshot anywhere, and none
/// that reads a source the snapshot schema does not name.
const V1_DIAGNOSTICS_SNAPSHOTS_PATH: &str = "/v1/diagnostics/snapshots";
const V1_DIAGNOSTICS_SNAPSHOT_ROUTE: &str = "/v1/diagnostics/snapshots/{id}";

/// M8's one route.
///
/// Not under `/v1/actions/`, and the reason is the reason section 2.3 item
/// (ii) gives for it needing a milestone of its own: it is neither a settings
/// write (it writes three subtrees), nor a collection, and calling it an
/// action understates that it is the device's one unauthenticated write. It is
/// the first-run operation, so it is named for that and nothing else.
const V1_SETUP_PATH: &str = "/v1/setup";

/// The read-only provisioning-document status (PLAN-046 / RFCT-282).
///
/// A fixed path on the time status's reasoning, and the whole of this
/// surface: there is no sibling constant here that applies, re-applies,
/// returns or clears a provisioning document. The document is the channel for
/// a device with NO network — it is applied by mosd from a boot medium or a
/// stick before anything is listening — so an HTTP route that applied one
/// would be a second, differently-trusted write path for the same thing. The
/// handler is [`crate::provisioning_api::api_v1_provisioning_status`].
pub(crate) const V1_PROVISIONING_STATUS_PATH: &str = "/v1/provisioning/status";

/// The reset tiers (PLAN-048 / RFCT-284), one path for all three.
///
/// A fixed path and a `POST` alone. The tier is in the BODY and not in the
/// path because `docs/design/recovery.md` §2.2 makes naming the tier part of
/// the request that gets audited, and a path segment per tier would make
/// "which resets does this device offer" a question about the route table
/// rather than about `ResetTier` — which is where the answer that there is no
/// fourth tier has to live. Not under `/v1/actions/` for the reason
/// [`V1_SETUP_PATH`] is not: this writes a device-lifecycle intent, and
/// calling it an action understates it.
const V1_RESET_PATH: &str = "/v1/reset";

/// The dot-path the staged intent lives at, which every envelope about it
/// names.
const RESET_PATH: &str = "reset";

/// Credential recovery (`docs/design/recovery.md` §5), the one operation on
/// this surface whose authority is physical presence rather than a credential.
///
/// Under `recovery/` and not `actions/` deliberately: `actions` is the
/// namespace of things an authenticated operator does, and §5.2 says in terms
/// that an authenticated session may not run this one.
const V1_RECOVERY_CREDENTIAL_PATH: &str = "/v1/recovery/credential";

/// Browser authentication state. Unlike the old HTML login form, every
/// operation stays inside the reserved JSON API surface.
const V1_SESSION_PATH: &str = "/v1/session";

/// How this device was claimed, and whether its credential must be rotated.
///
/// A fixed path and a `GET` alone: a claim is caused by
/// [`V1_SETUP_PATH`] or by a provisioning document, never by a write here.
/// Beside `/v1/session` rather than under it because it describes the DEVICE
/// and not the browser: the answer is the same for a bearer client.
const V1_CLAIM_PATH: &str = "/v1/claim";
const V1_UI_PATH: &str = "/v1/ui";
const V1_UI_ACTIVE_PATH: &str = "/v1/ui/active";
const V1_UI_BUNDLES_PATH: &str = "/v1/ui/bundles";
const V1_UI_BUNDLE_ROUTE: &str = "/v1/ui/bundles/{generation}";

/// M5's two array collections and their item routes.
///
/// Each collection needs its prefix separately for the reason the token
/// collection does: [`is_declared_api_route`] has to recognise the item shape
/// with no router to ask.
const V1_SSH_KEYS_PATH: &str = "/v1/ssh/authorized-keys";
const V1_SSH_KEY_ROUTE: &str = "/v1/ssh/authorized-keys/{fingerprint}";
const V1_WIFI_NETWORKS_PATH: &str = "/v1/wifi/client/networks";
const V1_WIFI_NETWORK_ROUTE: &str = "/v1/wifi/client/networks/{ssid}";

/// The dot-path the WiFi station's known-network list lives at, which every
/// envelope raised about it names.
///
/// The SSH list's is [`SSH_KEYS_PATH`], declared beside the pane that already
/// writes it: one dot-path for one list, whichever surface is writing it.
const WIFI_NETWORKS_PATH: &str = "wifi.client.networks";

/// M6's network cluster: the
/// interface map, one interface, and a tunnel's peer collection.
///
/// The prefix is needed separately for the reason the token collection's is:
/// [`is_declared_api_route`] has to recognise all three shapes under it with
/// no router to ask.
const V1_NETWORK_PATH: &str = "/v1/network";
const V1_NETWORK_IFACE_ROUTE: &str = "/v1/network/{iface}";
const V1_NETWORK_PEERS_ROUTE: &str = "/v1/network/{iface}/peers";
const V1_NETWORK_PEER_ROUTE: &str = "/v1/network/{iface}/peers/{publicKey}";

/// The settings dot-path the interface map lives at, which every envelope
/// raised about the whole map names.
const NETWORK_SETTINGS_PATH: &str = "network";

/// Each root's three spellings as one tuple, for the test that holds them
/// together.
#[cfg(test)]
pub(crate) const SETTINGS_SPELLINGS: (&str, &str, &str) =
    (V1_SETTINGS_PREFIX, V1_SETTINGS_ROUTE, V1_SETTINGS_DOC);
#[cfg(test)]
pub(crate) const STATE_SPELLINGS: (&str, &str, &str) =
    (V1_STATE_PREFIX, V1_STATE_ROUTE, V1_STATE_DOC);

/// The error names mosd maps its `SettingsError` onto, and the five rows of
/// §2.4's table that name one. The first two are interface-scoped: the fdo
/// vocabulary has no name that separates a missing dot-path or a read-only
/// one from a bad value, so mosd coins its own for those and keeps the
/// standard names for everything else.
const MOSD_NOT_FOUND: &str = "com.mos.mosd1.Error.NotFound";
const MOSD_READ_ONLY: &str = "com.mos.mosd1.Error.ReadOnly";
const FDO_INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
const FDO_IO_ERROR: &str = "org.freedesktop.DBus.Error.IOError";
const FDO_FAILED: &str = "org.freedesktop.DBus.Error.Failed";

/// §2.4's `Retry-After` on the one class that carries it.
const RETRY_AFTER_SECONDS: &str = "5";

/// The major versions this build serves — §2.1's *served set*, which is an
/// array because it can legitimately have more than one member.
const SERVED_VERSIONS: [&str; 1] = ["v1"];

/// The member of the served set a client with no preference should use.
const CURRENT_VERSION: &str = "v1";

/// The reserved `/api` subtree: §2.1's declared routes, and the not-found
/// handler every other path under the prefix reaches.
///
/// Each route is declared here from the same constant its `utoipa::path`
/// attribute documents it under, so the served path and the documented path
/// are one string and cannot disagree.
fn api_router() -> Router<AppState> {
    Router::new()
        .route(VERSIONS_PATH, get(api_versions))
        .route(V1_META_PATH, get(api_v1_meta))
        .route(
            V1_SESSION_PATH,
            get(api_v1_session_status)
                .post(api_v1_session_create)
                .delete(api_v1_session_delete),
        )
        .route(V1_CLAIM_PATH, get(api_v1_claim))
        .route(V1_UI_PATH, get(api_v1_ui_status))
        .route(
            V1_UI_BUNDLES_PATH,
            get(api_v1_ui_bundles).post(api_v1_ui_upload),
        )
        .route(V1_UI_BUNDLE_ROUTE, delete(api_v1_ui_delete))
        .route(
            V1_UI_ACTIVE_PATH,
            put(api_v1_ui_activate).delete(api_v1_ui_deactivate),
        )
        .route(V1_HEALTH_PATH, get(api_v1_health))
        .route(
            V1_SETTINGS_ROUTE,
            get(api_v1_settings).put(api_v1_settings_write),
        )
        .route(V1_STATE_ROUTE, get(api_v1_state))
        // GET only: the status is observed, and pausing synchronization is a
        // control this API deliberately does not have.
        .route(V1_TIME_STATUS_PATH, get(api_v1_time_status))
        // GET only, and alone: the layout is fixed, so there is no verb here
        // that could format or repartition anything.
        .route(V1_STORAGE_STATUS_PATH, get(api_v1_storage_status))
        .route(V1_SYSTEM_INFO_PATH, get(api_v1_system_info))
        .route(V1_SYSTEM_TELEMETRY_PATH, get(api_v1_system_telemetry))
        .route(V1_NETWORK_STATUS_PATH, get(api_v1_network_status))
        .route(
            V1_DIAGNOSTICS_SNAPSHOTS_PATH,
            get(api_v1_diagnostics_list).post(api_v1_diagnostics_collect),
        )
        .route(
            V1_DIAGNOSTICS_SNAPSHOT_ROUTE,
            get(api_v1_diagnostics_snapshot).delete(api_v1_diagnostics_delete),
        )
        .route(V1_TASKS_PATH, get(api_v1_tasks_list))
        .route(V1_TASK_ROUTE, get(api_v1_task))
        .route(V1_CHANGE_PASSWORD_PATH, post(api_v1_change_password))
        // Token lifecycle. Automation and authenticated browser sessions use
        // the same JSON routes. Session mutations also require CSRF.
        .route(
            V1_TOKENS_PATH,
            get(api_v1_tokens_list).post(api_v1_tokens_mint),
        )
        .route(V1_TOKEN_ROUTE, delete(api_v1_tokens_revoke))
        // Array resources share the same credential extractor as the rest of
        // the management API.
        .route(
            V1_SSH_KEYS_PATH,
            get(api_v1_ssh_keys_list).post(api_v1_ssh_keys_add),
        )
        .route(V1_SSH_KEY_ROUTE, delete(api_v1_ssh_keys_remove))
        .route(
            V1_WIFI_NETWORKS_PATH,
            get(api_v1_wifi_networks_list).post(api_v1_wifi_networks_add),
        )
        .route(V1_WIFI_NETWORK_ROUTE, delete(api_v1_wifi_networks_remove))
        // M6's network cluster. Typed rather than a dot-path passthrough
        // because the rules under `network` are relational: a bridge port has to
        // name a declared entry, which no check confined to the entry being
        // written could see. `PUT /api/v1/settings/network...` is refused at
        // 409 by [`settings_write_refusal`] and names these routes.
        .route(
            V1_NETWORK_PATH,
            get(api_v1_network_read).put(api_v1_network_write),
        )
        .route(
            V1_NETWORK_IFACE_ROUTE,
            put(api_v1_network_iface_write).delete(api_v1_network_iface_remove),
        )
        .route(
            V1_NETWORK_PEERS_ROUTE,
            get(api_v1_peers_list).post(api_v1_peers_add),
        )
        .route(V1_NETWORK_PEER_ROUTE, delete(api_v1_peers_remove))
        // POST only, for the reason the power and SSH mutations are: no GET
        // handler exists, so nothing that merely follows a link can replace a
        // tunnel's identity.
        .route(V1_WIREGUARD_ROTATE_ROUTE, post(api_v1_wireguard_rotate))
        // The update cluster (`update_api.rs`): one state read, six
        // POST-only actions, all behind the same credential extractor.
        .route(
            crate::update_api::V1_UPDATE_PATH,
            get(crate::update_api::api_v1_update_state),
        )
        .route(
            crate::update_api::V1_UPDATE_CHECK_PATH,
            post(crate::update_api::api_v1_update_check),
        )
        .route(
            crate::update_api::V1_UPDATE_FETCH_PATH,
            post(crate::update_api::api_v1_update_fetch),
        )
        .route(
            crate::update_api::V1_UPDATE_INSTALL_PATH,
            post(crate::update_api::api_v1_update_install),
        )
        .route(
            crate::update_api::V1_UPDATE_MARK_PATH,
            post(crate::update_api::api_v1_update_mark),
        )
        .route(
            crate::update_api::V1_UPDATE_ROLLBACK_PATH,
            post(crate::update_api::api_v1_update_rollback),
        )
        .route(
            crate::update_api::V1_UPDATE_REBOOT_OVERRIDE_PATH,
            post(crate::update_api::api_v1_update_reboot_override),
        )
        // Actions are POST-only so navigation and prefetch cannot trigger
        // state changes.
        .route(V1_REBOOT_PATH, post(api_v1_reboot))
        .route(V1_POWEROFF_PATH, post(api_v1_poweroff))
        .route(
            V1_TRANSIENT_PASSWORD_PATH,
            post(api_v1_transient_root_password),
        )
        // First-run setup is the only unauthenticated write and remains
        // POST-only.
        .route(V1_SETUP_PATH, post(api_v1_setup))
        // GET only: what a provisioning document did is observed; applying one
        // is a file on a medium, read before anything is listening.
        .route(
            V1_PROVISIONING_STATUS_PATH,
            get(crate::provisioning_api::api_v1_provisioning_status),
        )
        // POST only, for the reason the power actions are: no GET handler
        // exists, so nothing that merely follows a link can stage a reset.
        .route(V1_RESET_PATH, post(api_v1_reset))
        // POST only, and with no credential extractor: §5.2's authority is
        // physical presence, and an authenticated session is refused rather
        // than admitted.
        .route(
            V1_RECOVERY_CREDENTIAL_PATH,
            post(api_v1_recovery_credential),
        )
        // §2.4's envelope on the methods those routes do not serve, declared
        // once for the subtree rather than route by route. It reaches exactly
        // the routes above — it rewrites the method-not-allowed fallback of
        // every `MethodRouter` already registered on *this* router. It must
        // stay below the last `.route`: a route declared after it would not be
        // reached.
        .method_not_allowed_fallback(api_method_not_allowed)
        .fallback(api_not_found)
}

/// §2.4's envelope for a method a declared route does not serve.
///
/// §2.4 states **one** shape for every failure on every `/api/v1/` route, and a
/// wrong method is a failure like any other. Without this the answer is axum's
/// own: a bare 405 with no body and no `Content-Type` at all — measured — so a
/// client that parses the envelope on every other failure had nothing to parse
/// on this one.
///
/// The `Allow` header is left to axum deliberately. axum accumulates it from
/// the very `get`/`post` calls that declare each route above and attaches it to
/// whatever this handler returns unless the response already carries one, so
/// the header cannot name a method a route does not serve or omit one it does.
/// A hand-written `Allow` here would be a second opinion about the route table,
/// and second opinions drift.
///
/// `source` is `"apid"`: the router made this decision and no bus call was
/// made, so there is nothing mosd could be asked about it. There is no `path`
/// member for the same reason the not-found envelope has none — a wrong method
/// names no settings dot-path.
///
/// It is reached without an authentication check, which is what the shipped
/// tree already did: [`is_declared_api_route`] tests the path and not the
/// method, so the gate hands a wrong-method request on a declared path off just
/// as it hands off the right one. The status is 405 either way; this changes
/// what is in the body, not who may see it.
async fn api_method_not_allowed(method: Method, OriginalUri(uri): OriginalUri) -> Response {
    api_response(
        StatusCode::METHOD_NOT_ALLOWED,
        ApiError::apid(
            "method_not_allowed",
            format!("{method} is not a method {} serves", uri.path()),
        ),
    )
}

/// Every `/api/` response is JSON and is never cacheable.
pub(crate) fn api_response(status: StatusCode, body: impl serde::Serialize) -> Response {
    (
        status,
        [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
        Json(body),
    )
        .into_response()
}

/// The one shape every failure under `/api/` takes.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ApiError {
    error: ApiErrorDetail,
}

/// The API error envelope payload.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ApiErrorDetail {
    code: &'static str,
    message: String,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}

impl ApiError {
    pub(crate) fn apid(code: &'static str, message: String) -> Self {
        Self::new(code, message, "apid")
    }

    pub(crate) fn mosd(code: &'static str, message: String) -> Self {
        Self::new(code, message, "mosd")
    }

    fn new(code: &'static str, message: String, source: &'static str) -> Self {
        Self {
            error: ApiErrorDetail {
                code,
                message,
                source,
                path: None,
            },
        }
    }

    fn at(mut self, path: &str) -> Self {
        self.error.path = Some(path.to_string());
        self
    }
}

/// The browser's current authentication state.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionStatus {
    /// `setup`, `unauthenticated`, or `authenticated`.
    state: &'static str,
    /// Returned only for an authenticated browser session.
    #[serde(skip_serializing_if = "Option::is_none")]
    csrf_token: Option<String>,
}

impl SessionStatus {
    fn setup() -> Self {
        Self {
            state: "setup",
            csrf_token: None,
        }
    }

    fn unauthenticated() -> Self {
        Self {
            state: "unauthenticated",
            csrf_token: None,
        }
    }

    fn authenticated(csrf_token: String) -> Self {
        Self {
            state: "authenticated",
            csrf_token: Some(csrf_token),
        }
    }
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct SessionLoginRequest {
    password: String,
}

/// Report setup and browser authentication state without redirecting.
#[utoipa::path(
    get,
    path = V1_SESSION_PATH,
    context_path = API,
    tag = "session",
    responses(
        (status = 200, description = "Setup and browser authentication state", body = SessionStatus),
        (status = 500, description = "The access settings could not be read", body = ApiError),
        (status = 503, description = "mosd is unavailable", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_session_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let access = match access_settings(&state).await {
        Ok(value) => value,
        Err(err) => return bus_api_error(&err, Some("access")),
    };
    if password_hash(&access).is_none() {
        return api_response(StatusCode::OK, SessionStatus::setup());
    }
    let status = session::cookie_from_headers(&headers)
        .and_then(|cookie| state.sessions.csrf_token(&cookie))
        .map_or_else(SessionStatus::unauthenticated, SessionStatus::authenticated);
    api_response(StatusCode::OK, status)
}

/// Authenticate a browser and create its HttpOnly session cookie.
#[utoipa::path(
    post,
    path = V1_SESSION_PATH,
    context_path = API,
    tag = "session",
    request_body = SessionLoginRequest,
    responses(
        (status = 201, description = "The browser session was created", body = SessionStatus),
        (status = 400, description = "The body is not JSON", body = ApiError),
        (status = 401, description = "The password is incorrect", body = ApiError),
        (status = 409, description = "The device still requires first-run setup", body = ApiError),
        (status = 422, description = "The JSON body has the wrong shape", body = ApiError),
        (status = 429, description = "Login attempts are temporarily throttled", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_session_create(
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request: SessionLoginRequest = match json_body(body, None) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    if !state.guard.begin_attempt() {
        state.audit.record("login", "throttled", &source);
        return api_response(
            StatusCode::TOO_MANY_REQUESTS,
            ApiError::apid(
                "login_throttled",
                "too many failed logins; retry shortly".to_string(),
            ),
        );
    }
    let access = match state.api.get_settings("access").await {
        Ok(value) => value,
        Err(err) => return bus_api_error(&err, Some("access")),
    };
    let Some(hash) = password_hash(&access) else {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "setup_required",
                "complete first-run setup before signing in".to_string(),
            ),
        );
    };
    let hash = hash.to_string();
    let password = request.password;
    let verified = tokio::task::spawn_blocking(move || auth::verify_password(&hash, &password))
        .await
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "password verification task failed");
            false
        });
    if !verified {
        state.guard.confirm_failure();
        state.audit.record("login", "wrong-password", &source);
        return api_response(
            StatusCode::UNAUTHORIZED,
            ApiError::apid(
                "invalid_credentials",
                "the password is incorrect".to_string(),
            ),
        );
    }

    state.guard.record_success();
    state.audit.record("login", "success", &source);
    let session = state.sessions.create();
    (
        StatusCode::CREATED,
        [
            (
                CACHE_CONTROL,
                CacheClass::NoStore.header_value().to_string(),
            ),
            (SET_COOKIE, session::session_cookie(&session.cookie)),
        ],
        Json(SessionStatus::authenticated(session.csrf_token)),
    )
        .into_response()
}

/// Revoke the acting browser session.
#[utoipa::path(
    delete,
    path = V1_SESSION_PATH,
    context_path = API,
    tag = "session",
    responses(
        (status = 204, description = "The browser session was revoked"),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 403, description = "The browser CSRF token is absent or invalid", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_session_delete(
    State(state): State<AppState>,
    Source(source): Source,
    credential: ApiCredential,
) -> Response {
    if let ApiCredential::Session(cookie) = credential {
        state.sessions.remove(&cookie);
        state.audit.record("logout", "ok", &source);
    }
    (
        StatusCode::NO_CONTENT,
        [
            (
                CACHE_CONTROL,
                CacheClass::NoStore.header_value().to_string(),
            ),
            (SET_COOKIE, session::clear_cookie()),
        ],
    )
        .into_response()
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiStatus {
    /// `builtIn` when no custom bundle is active, otherwise `custom`.
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    custom: Option<CustomUiStatus>,
    /// The newest usable retained generation, or the newest unusable one with
    /// a reason when no generation passes validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    available_custom: Option<AvailableCustomUiStatus>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CustomUiStatus {
    generation: u64,
    index_readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest_matches: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatible: Option<bool>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AvailableCustomUiStatus {
    generation: u64,
    index_readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest_matches: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatible: Option<bool>,
    usable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<CustomUiUnavailableReason>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiBundleList {
    #[serde(skip_serializing_if = "Option::is_none")]
    active_generation: Option<u64>,
    bundles: Vec<UiBundleDetails>,
    retention_limit: usize,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UiBundleDetails {
    generation: u64,
    index_readable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compressed_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expanded_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest_matches: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatible: Option<bool>,
    usable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_reason: Option<CustomUiUnavailableReason>,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct ActivateUiRequest {
    generation: u64,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CustomUiUnavailableReason {
    MissingActivationRecord,
    UnsafeTree,
    IndexUnavailable,
    ManifestInvalid,
    DigestMismatch,
    Incompatible,
}

impl From<CandidateUnavailable> for CustomUiUnavailableReason {
    fn from(reason: CandidateUnavailable) -> Self {
        match reason {
            CandidateUnavailable::MissingActivationRecord => Self::MissingActivationRecord,
            CandidateUnavailable::UnsafeTree => Self::UnsafeTree,
            CandidateUnavailable::IndexUnavailable => Self::IndexUnavailable,
            CandidateUnavailable::ManifestInvalid => Self::ManifestInvalid,
            CandidateUnavailable::DigestMismatch => Self::DigestMismatch,
            CandidateUnavailable::Incompatible => Self::Incompatible,
        }
    }
}

impl From<CustomCandidate> for AvailableCustomUiStatus {
    fn from(candidate: CustomCandidate) -> Self {
        let (name, version) = candidate.manifest.map_or((None, None), |manifest| {
            (Some(manifest.name), Some(manifest.version))
        });
        Self {
            generation: candidate.generation,
            index_readable: candidate.index_readable,
            name,
            version,
            digest_matches: candidate.digest_matches,
            compatible: candidate.compatible,
            usable: candidate.usable,
            unavailable_reason: candidate.unavailable_reason.map(Into::into),
        }
    }
}

impl From<CustomCandidate> for UiBundleDetails {
    fn from(candidate: CustomCandidate) -> Self {
        let (name, version) = candidate.manifest.map_or((None, None), |manifest| {
            (Some(manifest.name), Some(manifest.version))
        });
        Self {
            generation: candidate.generation,
            index_readable: candidate.index_readable,
            name,
            version,
            digest: candidate.digest,
            compressed_bytes: candidate.compressed_bytes,
            expanded_bytes: candidate.expanded_bytes,
            digest_matches: candidate.digest_matches,
            compatible: candidate.compatible,
            usable: candidate.usable,
            unavailable_reason: candidate.unavailable_reason.map(Into::into),
        }
    }
}

fn ui_status(store: &Store) -> anyhow::Result<UiStatus> {
    let available_custom = store
        .available_custom(&SERVED_VERSIONS)?
        .map(AvailableCustomUiStatus::from);
    Ok(match store.status()? {
        Installed::BuiltIn => UiStatus {
            mode: "builtIn",
            custom: None,
            available_custom,
        },
        Installed::Custom(ui) => {
            let (name, version) = ui.manifest.map_or((None, None), |manifest| {
                (Some(manifest.name), Some(manifest.version))
            });
            let (digest_matches, compatible) = ui.recorded.map_or((None, None), |recorded| {
                let compatible = match recorded.compat {
                    crate::bundle::CompatCheck::NotRun => None,
                    crate::bundle::CompatCheck::Ran { compatible, .. } => Some(compatible),
                };
                (Some(recorded.digest_matches), compatible)
            });
            UiStatus {
                mode: "custom",
                custom: Some(CustomUiStatus {
                    generation: ui.generation,
                    index_readable: ui.index_readable,
                    name,
                    version,
                    digest_matches,
                    compatible,
                }),
                available_custom,
            }
        }
    })
}

async fn load_ui_status(state: &AppState) -> anyhow::Result<UiStatus> {
    let bundles = Arc::clone(&state.bundles);
    match tokio::task::spawn_blocking(move || ui_status(&bundles)).await {
        Ok(status) => status,
        Err(err) => Err(anyhow::Error::new(err)),
    }
}

fn ui_bundle_list(store: &Store) -> anyhow::Result<UiBundleList> {
    Ok(UiBundleList {
        active_generation: store.active_generation()?,
        bundles: store
            .candidates(&SERVED_VERSIONS)?
            .into_iter()
            .map(UiBundleDetails::from)
            .collect(),
        retention_limit: 32,
    })
}

async fn load_ui_bundles(state: &AppState) -> anyhow::Result<UiBundleList> {
    let bundles = Arc::clone(&state.bundles);
    tokio::task::spawn_blocking(move || ui_bundle_list(&bundles))
        .await
        .map_err(anyhow::Error::new)?
}

/// List every retained custom UI generation.
#[utoipa::path(
    get,
    path = V1_UI_BUNDLES_PATH,
    context_path = API,
    tag = "ui",
    responses(
        (status = 200, description = "Every retained UI version and the active generation", body = UiBundleList),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 500, description = "The bundle store could not be read", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ui_bundles(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match load_ui_bundles(&state).await {
        Ok(list) => api_response(StatusCode::OK, list),
        Err(err) => ui_status_error(&err),
    }
}

/// Stream a `.mos-ui.zip` to DATA, validate and extract it off the async worker,
/// then install it without changing the active generation.
#[utoipa::path(
    post,
    path = V1_UI_BUNDLES_PATH,
    context_path = API,
    tag = "ui",
    request_body(content = String, content_type = "application/zip"),
    responses(
        (status = 201, description = "The package was installed but not activated", body = UiBundleList),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 403, description = "The browser CSRF token is absent or invalid", body = ApiError),
        (status = 409, description = "The package duplicates, conflicts with, or is incompatible with retained versions", body = ApiError),
        (status = 413, description = "The compressed package exceeds 64 MiB", body = ApiError),
        (status = 415, description = "The body is not application/zip", body = ApiError),
        (status = 422, description = "The ZIP or manifest violates the UI package contract", body = ApiError),
        (status = 507, description = "Writable /mos storage or required headroom is unavailable", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ui_upload(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    request: Request<Body>,
) -> Response {
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if content_type != Some("application/zip") {
        return ui_upload_refusal(
            &state,
            &source,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "ui_package_type",
            "upload a .mos-ui.zip as application/zip",
        );
    }
    if request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > mos_ui_bundle::MAX_COMPRESSED_BYTES)
    {
        return ui_upload_refusal(
            &state,
            &source,
            StatusCode::PAYLOAD_TOO_LARGE,
            "ui_package_too_large",
            "the compressed package limit is 64 MiB",
        );
    }

    let upload_dir = state.bundles.root().join("staging");
    match tokio::fs::create_dir(&upload_dir).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let real_directory = tokio::fs::symlink_metadata(&upload_dir)
                .await
                .is_ok_and(|metadata| metadata.file_type().is_dir());
            if !real_directory {
                tracing::error!("UI upload staging path is not a real directory");
                return ui_upload_refusal(
                    &state,
                    &source,
                    StatusCode::INSUFFICIENT_STORAGE,
                    "ui_storage_unavailable",
                    "writable /mos UI storage is unavailable",
                );
            }
        }
        Err(err) => {
            tracing::error!(error = %err, "creating UI upload staging directory failed");
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::INSUFFICIENT_STORAGE,
                "ui_storage_unavailable",
                "writable /mos UI storage is unavailable",
            );
        }
    }
    if let Err(err) =
        tokio::fs::set_permissions(&upload_dir, std::fs::Permissions::from_mode(0o700)).await
    {
        tracing::error!(error = %err, "protecting UI upload staging directory failed");
        return ui_upload_refusal(
            &state,
            &source,
            StatusCode::INSUFFICIENT_STORAGE,
            "ui_storage_unavailable",
            "writable /mos UI storage is unavailable",
        );
    }
    let upload = upload_dir.join(format!("{:032x}", rand::random::<u128>()));
    let mut file = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&upload)
        .await
    {
        Ok(file) => file,
        Err(err) => {
            tracing::error!(error = %err, "creating UI upload file failed");
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::INSUFFICIENT_STORAGE,
                "ui_storage_unavailable",
                "the upload staging file could not be created",
            );
        }
    };
    let mut stream = request.into_body().into_data_stream();
    let mut compressed = 0_u64;
    use tokio::io::AsyncWriteExt as _;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                let _ = tokio::fs::remove_file(&upload).await;
                tracing::warn!(error = %err, "UI package upload was interrupted");
                return ui_upload_refusal(
                    &state,
                    &source,
                    StatusCode::BAD_REQUEST,
                    "ui_upload_interrupted",
                    "the upload was interrupted",
                );
            }
        };
        compressed = compressed.saturating_add(chunk.len() as u64);
        if compressed > mos_ui_bundle::MAX_COMPRESSED_BYTES {
            let _ = tokio::fs::remove_file(&upload).await;
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::PAYLOAD_TOO_LARGE,
                "ui_package_too_large",
                "the compressed package limit is 64 MiB",
            );
        }
        if let Err(err) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&upload).await;
            tracing::error!(error = %err, "writing UI upload failed");
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::INSUFFICIENT_STORAGE,
                "ui_storage_unavailable",
                "the package could not be written to /mos",
            );
        }
    }
    if let Err(err) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&upload).await;
        tracing::error!(error = %err, "syncing UI upload failed");
        return ui_upload_refusal(
            &state,
            &source,
            StatusCode::INSUFFICIENT_STORAGE,
            "ui_storage_unavailable",
            "the package could not be persisted to /mos",
        );
    }
    drop(file);

    let inspect_path = upload.clone();
    let info =
        match tokio::task::spawn_blocking(move || mos_ui_bundle::inspect(&inspect_path)).await {
            Ok(Ok(info)) => info,
            Ok(Err(err)) => {
                let _ = tokio::fs::remove_file(&upload).await;
                tracing::warn!(error = %err, "inspecting UI package failed");
                return ui_upload_refusal(
                    &state,
                    &source,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ui_package_invalid",
                    "the ZIP or manifest violates the UI package contract",
                );
            }
            Err(err) => {
                let _ = tokio::fs::remove_file(&upload).await;
                tracing::error!(error = %err, "UI package validator did not complete");
                return ui_upload_refusal(
                    &state,
                    &source,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ui_package_failed",
                    "the package validator did not complete",
                );
            }
        };

    const HEADROOM: u64 = 128 * 1024 * 1024;
    let root = state.bundles.root().to_path_buf();
    let required = info.expanded_bytes.saturating_add(HEADROOM);
    let free = tokio::task::spawn_blocking(move || {
        let stat = rustix::fs::statvfs(&root)?;
        Ok::<u64, std::io::Error>(stat.f_bavail.saturating_mul(stat.f_frsize))
    })
    .await;
    match free {
        Ok(Ok(free)) if free >= required => {}
        Ok(Ok(_)) => {
            let _ = tokio::fs::remove_file(&upload).await;
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::INSUFFICIENT_STORAGE,
                "ui_storage_headroom",
                "upload refused because extraction would leave less than 128 MiB free",
            );
        }
        Ok(Err(err)) => {
            let _ = tokio::fs::remove_file(&upload).await;
            tracing::error!(error = %err, "reading UI storage capacity failed");
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::INSUFFICIENT_STORAGE,
                "ui_storage_unavailable",
                "free space under /mos could not be measured",
            );
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&upload).await;
            tracing::error!(error = %err, "UI storage capacity check did not complete");
            return ui_upload_refusal(
                &state,
                &source,
                StatusCode::INTERNAL_SERVER_ERROR,
                "ui_package_failed",
                "the storage capacity check did not complete",
            );
        }
    }

    let _selection_guard = state.ui_selection.lock().await;
    let store = Arc::clone(&state.bundles);
    let upload_for_install = upload.clone();
    let package_sizes = (info.compressed_bytes, info.expanded_bytes);
    let installed = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
        let generation = store.next_generation()?;
        let staging = store.staging_dir(generation);
        let extraction = mos_ui_bundle::extract(&upload_for_install, &staging);
        if let Err(err) = extraction {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(err);
        }
        if let Err(err) = store.install(
            generation,
            &SERVED_VERSIONS,
            package_sizes.0,
            package_sizes.1,
        ) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(err);
        }
        Ok(generation)
    })
    .await;
    let _ = tokio::fs::remove_file(&upload).await;
    match installed {
        Ok(Ok(_generation)) => {
            state.audit.record("custom-ui-upload", "installed", &source);
            match load_ui_bundles(&state).await {
                Ok(list) => api_response(StatusCode::CREATED, list),
                Err(err) => ui_status_error(&err),
            }
        }
        Ok(Err(err)) => {
            let (status, code, message) = match err.downcast_ref::<Rejection>() {
                Some(
                    Rejection::DuplicatePackage(_)
                    | Rejection::NameVersionConflict { .. }
                    | Rejection::RetentionLimit(_)
                    | Rejection::Incompatible { .. },
                ) => (
                    StatusCode::CONFLICT,
                    "ui_package_conflict",
                    "the package duplicates, conflicts with, or is incompatible with retained versions",
                ),
                Some(_) => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "ui_package_rejected",
                    "the package tree or manifest violates the UI package contract",
                ),
                None => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "ui_package_failed",
                    "the package could not be installed",
                ),
            };
            tracing::warn!(error = %err, "installing UI package failed");
            ui_upload_refusal(&state, &source, status, code, message)
        }
        Err(err) => {
            tracing::error!(error = %err, "UI package installer did not complete");
            ui_upload_refusal(
                &state,
                &source,
                StatusCode::INTERNAL_SERVER_ERROR,
                "ui_package_failed",
                "the package installer did not complete",
            )
        }
    }
}

fn ui_upload_refusal(
    state: &AppState,
    source: &str,
    status: StatusCode,
    code: &'static str,
    message: &str,
) -> Response {
    state.audit.record("custom-ui-upload", "refused", source);
    ui_upload_error(status, code, message)
}

fn ui_upload_error(status: StatusCode, code: &'static str, message: &str) -> Response {
    api_response(status, ApiError::apid(code, message.to_string()))
}

/// Report whether `/` currently selects a custom UI bundle.
#[utoipa::path(
    get,
    path = V1_UI_PATH,
    context_path = API,
    tag = "ui",
    responses(
        (status = 200, description = "The active UI selection and custom bundle health", body = UiStatus),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 500, description = "The bundle store could not be read", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ui_status(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match load_ui_status(&state).await {
        Ok(status) => api_response(StatusCode::OK, status),
        Err(err) => {
            tracing::error!(error = %err, "reading custom UI status failed");
            api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid(
                    "ui_status_failed",
                    "the custom UI status could not be read".to_string(),
                ),
            )
        }
    }
}

/// Select one exact retained custom bundle after repeating every safety and
/// compatibility check.
#[utoipa::path(
    put,
    path = V1_UI_ACTIVE_PATH,
    context_path = API,
    tag = "ui",
    request_body = ActivateUiRequest,
    responses(
        (status = 200, description = "A validated retained custom UI is now selected", body = UiStatus),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 403, description = "The browser CSRF token is absent or invalid", body = ApiError),
        (status = 409, description = "No retained custom UI passes validation (`custom_ui_unavailable`)", body = ApiError),
        (status = 500, description = "The custom UI pointer could not be updated", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ui_activate(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    Json(request): Json<ActivateUiRequest>,
) -> Response {
    let _selection_guard = state.ui_selection.lock().await;
    let bundles = Arc::clone(&state.bundles);
    let selection = match tokio::task::spawn_blocking(move || {
        bundles.select_generation(request.generation, &SERVED_VERSIONS)
    })
    .await
    {
        Ok(selection) => selection,
        Err(err) => Err(anyhow::Error::new(err)),
    };
    match selection {
        Ok(Some(selection)) => {
            state.audit.record(
                "custom-ui",
                if selection.changed {
                    "activated"
                } else {
                    "no-op"
                },
                &source,
            );
            match load_ui_status(&state).await {
                Ok(status) => api_response(StatusCode::OK, status),
                Err(err) => ui_status_error(&err),
            }
        }
        Ok(None) => api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "custom_ui_unavailable",
                "no retained custom UI passes the current safety and API compatibility checks"
                    .to_string(),
            ),
        ),
        Err(err) => {
            tracing::error!(error = %err, "activating retained custom UI failed");
            api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid(
                    "ui_activation_failed",
                    "the custom UI pointer could not be updated".to_string(),
                ),
            )
        }
    }
}

/// Delete one inactive retained custom UI generation.
#[utoipa::path(
    delete,
    path = V1_UI_BUNDLE_ROUTE,
    context_path = API,
    tag = "ui",
    params(("generation" = u64, Path, description = "The retained generation to delete")),
    responses(
        (status = 204, description = "The inactive retained generation was deleted"),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 403, description = "The browser CSRF token is absent or invalid", body = ApiError),
        (status = 409, description = "The generation is active", body = ApiError),
        (status = 500, description = "The generation could not be deleted", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ui_delete(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    Path(generation): Path<u64>,
) -> Response {
    let _selection_guard = state.ui_selection.lock().await;
    let bundles = Arc::clone(&state.bundles);
    let deleted = tokio::task::spawn_blocking(move || bundles.delete(generation)).await;
    match deleted {
        Ok(Ok(())) => {
            state.audit.record("custom-ui-delete", "deleted", &source);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(err))
            if matches!(
                err.downcast_ref::<Rejection>(),
                Some(Rejection::DeleteWhileActive(_))
            ) =>
        {
            ui_upload_error(
                StatusCode::CONFLICT,
                "ui_bundle_active",
                "deactivate this UI version before deleting it",
            )
        }
        Ok(Err(err)) => {
            tracing::error!(error = %err, generation, "deleting custom UI failed");
            ui_upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ui_delete_failed",
                "the custom UI version could not be deleted",
            )
        }
        Err(err) => {
            tracing::error!(error = %err, generation, "custom UI deletion task did not complete");
            ui_upload_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ui_delete_failed",
                "the custom UI version could not be deleted",
            )
        }
    }
}

/// Deactivate the custom bundle so `/` selects the built-in UI.
#[utoipa::path(
    delete,
    path = V1_UI_ACTIVE_PATH,
    context_path = API,
    tag = "ui",
    responses(
        (status = 200, description = "The built-in UI is now selected", body = UiStatus),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 403, description = "The browser CSRF token is absent or invalid", body = ApiError),
        (status = 500, description = "The custom UI pointer could not be removed", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ui_deactivate(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    let _selection_guard = state.ui_selection.lock().await;
    let bundles = Arc::clone(&state.bundles);
    let deactivated = match tokio::task::spawn_blocking(move || bundles.deactivate()).await {
        Ok(deactivated) => deactivated,
        Err(err) => Err(anyhow::Error::new(err)),
    };
    match deactivated {
        Ok(removed) => {
            state.audit.record(
                "custom-ui",
                if removed { "deactivated" } else { "no-op" },
                &source,
            );
            match load_ui_status(&state).await {
                Ok(status) => api_response(StatusCode::OK, status),
                Err(err) => ui_status_error(&err),
            }
        }
        Err(err) => {
            tracing::error!(error = %err, "deactivating custom UI failed");
            api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid(
                    "ui_deactivation_failed",
                    "the custom UI pointer could not be removed".to_string(),
                ),
            )
        }
    }
}

fn ui_status_error(err: &anyhow::Error) -> Response {
    tracing::error!(error = %err, "reading custom UI status failed");
    api_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        ApiError::apid(
            "ui_status_failed",
            "the custom UI status could not be read".to_string(),
        ),
    )
}

/// `GET /api/versions` response.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ApiVersions {
    versions: Vec<&'static str>,
    current: &'static str,
}

/// `GET /api/v1/meta` response.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiMeta {
    api: &'static str,
    settings_schema_version: u32,
    daemon: &'static str,
}

/// List the API major versions this build serves.
#[utoipa::path(
    get,
    path = VERSIONS_PATH,
    context_path = API,
    tag = "discovery",
    responses(
        (status = 200, description = "The major API versions this device serves", body = ApiVersions),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_versions() -> Response {
    api_response(
        StatusCode::OK,
        ApiVersions {
            versions: SERVED_VERSIONS.to_vec(),
            current: CURRENT_VERSION,
        },
    )
}

/// Report what the caller is talking to.
///
/// Answers the API major version, the daemon name, and
/// `settingsSchemaVersion` — the shape of the settings tree on disk, which
/// moves independently of the API version. Authenticated.
#[utoipa::path(
    get,
    path = V1_META_PATH,
    context_path = API,
    tag = "discovery",
    responses(
        (status = 200, description = "What this daemon is and which schema it speaks", body = ApiMeta),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_meta(_credential: ApiCredential) -> Response {
    api_response(
        StatusCode::OK,
        ApiMeta {
            api: CURRENT_VERSION,
            settings_schema_version: mosd_settings::SCHEMA_VERSION,
            daemon: "apid",
        },
    )
}

// Optional members are omitted rather than sent null, the same rule the error
// envelope follows: a member present with a meaningless value is worse than an
// absent one.
/// The body of `GET /api/v1/health`.
///
/// `apid` and `mosd` are always present. `checkedAt` is present only on the
/// reachable answer and `detail` only on the unreachable one.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApiHealth {
    // On the wire so a client reads one document rather than inferring half of
    // it from the fact that a response arrived.
    /// Always `"ok"`: a request that got a body was served by an apid that is
    /// up.
    apid: &'static str,
    // Two values and no third: a health answer that needs a taxonomy is not
    // one a monitor can act on.
    /// `"ok"` when the probe completed, `"unreachable"` when it did not.
    mosd: &'static str,
    // Uptime and not a wall-clock stamp: there is no trusted wall clock in
    // this crate, and `SessionStore` is monotonic `Instant` throughout.
    /// Whole seconds since boot, at the moment the probe answered.
    ///
    /// The same value and spelling `GET /api/v1/state/uptime` serves.
    #[serde(skip_serializing_if = "Option::is_none")]
    checked_at: Option<u64>,
    /// Why the probe did not complete, in the words of whatever refused it.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

// Never serve this from `access_cache`: that cache answers from apid's own
// memory, so a health route reading it would report mosd healthy for as long
// as the last fill survived. `GetState("uptime")` is the probe because it is
// one call, cheap, and also yields `checkedAt`.
/// Report whether this appliance is manageable.
///
/// Answers **200 in every state**, including when mosd is unreachable — a
/// failure is reported in the body, never as a status code, so it cannot be
/// confused with this endpoint being down. mosd's state is determined by one
/// live bus call per request.
///
/// Authenticated. For plain listener liveness use the unauthenticated
/// `/healthz` instead; neither answer implies the other.
#[utoipa::path(
    get,
    path = V1_HEALTH_PATH,
    context_path = API,
    tag = "diagnostics",
    responses(
        (status = 200, description = "Whether this appliance is manageable. **200 in both states**: a dead mosd is reported as `mosd: \"unreachable\"` in the body, never as a status code", body = ApiHealth),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_health(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    let (mosd, checked_at, detail) = match state.api.get_state(HEALTH_PROBE_PATH).await {
        // Any answer that is not the number of seconds mosd documents is
        // classified with the failures rather than reported as health. `ok`
        // has to mean "the round trip completed and produced a usable answer";
        // a state key that came back the wrong shape did not.
        Ok(value) => match value.as_u64() {
            Some(seconds) => ("ok", Some(seconds), None),
            None => (
                "unreachable",
                None,
                Some(format!(
                    "mosd answered GetState(\"{HEALTH_PROBE_PATH}\") with {value}, which is not a count of seconds"
                )),
            ),
        },
        Err(err) => ("unreachable", None, Some(format!("{err:#}"))),
    };
    api_response(
        StatusCode::OK,
        ApiHealth {
            apid: "ok",
            mosd,
            checked_at,
            detail,
        },
    )
}

/// The body of a resource `GET`: the value at the dot-path, as mosd holds it.
///
// `privateKey` is on the redaction list as a fail-closed guard: no shipped
// schema has such a field yet.
/// Any JSON value: a dot-path can name a subtree, an array or a scalar.
///
/// Secrets are replaced by the string `"<redacted>"`, which a client can
/// receive anywhere inside the body. Every field named `psk`, `passwordHash`,
/// `password_hash`, `hash` or `privateKey` carries the sentinel instead of its
/// value, at any depth and inside arrays, and so does the whole body when the
/// dot-path names one of those fields directly.
///
/// The sentinel is read-only: writing it back is refused at **422** rather
/// than stored, because storing it would destroy the credential.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(transparent)]
pub(crate) struct ResourceValue(Value);

/// A persisted settings write whose reconciliation continues asynchronously.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskAccepted {
    /// Poll this id at `GET /api/v1/tasks/{id}`.
    task_id: String,
}

/// Read the settings tree at a dot-path.
///
/// The dot-path is the resource identifier. Secrets are redacted in the
/// response. An empty path returns the whole tree. Answers **404** when the
/// path names nothing and **422** when it is not a well-formed dot-path.
#[utoipa::path(
    get,
    path = V1_SETTINGS_DOC,
    context_path = API,
    tag = "resources",
    params(("path" = String, Path, description = "The settings dot-path, verbatim: `hostname`, `access.ssh`, `wifi.ap`")),
    responses(
        (status = 200, description = "The value at the dot-path, redacted", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 404, description = "The dot-path does not exist (`settings_not_found`)", body = ApiError),
        (status = 422, description = "mosd rejected the dot-path (`settings_rejected`)", body = ApiError),
        (status = 500, description = "mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_settings(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    resource_response(state.api.get_settings(&path).await, &path)
}

/// What the value at a writable dot-path has to be.
///
/// Four shapes and not one validator per path, because the write surface
/// admits its paths on one ground: each value's validity *"depends on nothing
/// else in the tree"*. A hostname, which `valid_hostname` decides on its own;
/// three switches, which *"cannot be invalid at all"*; and the two `time`
/// values, whose rules ([`mosd_settings::validate_timezone_name`] and
/// [`mosd_settings::validate_ntp_servers`]) are each self-contained — stated
/// once, in the crate that owns the model, and only CALLED here, so this
/// surface cannot drift from what `Settings::set` enforces. Anything
/// relational is a later milestone by construction.
#[derive(Clone, Copy)]
enum ScalarShape {
    /// A JSON string [`valid_hostname`] accepts.
    Hostname,
    /// A JSON boolean, and nothing else.
    Flag,
    /// A JSON string [`mosd_settings::validate_timezone_name`] accepts.
    Timezone,
    /// A JSON array of strings [`mosd_settings::validate_ntp_servers`]
    /// accepts, written whole — the dot-path syntax has no array indexing.
    NtpServers,
}

/// The dot-paths `PUT /api/v1/settings/{path}` writes, and the shape each
/// value has to have.
///
/// An allowlist rather than a passthrough, and the reason is measured rather
/// than stylistic. `Settings::set`'s documented contract is *"Missing
/// intermediate map entries are created (e.g. setting `network.eth1.dhcp`
/// creates `eth1`)"* (`Settings::set` in `pkgs/mosd/mosd-settings/src/model.rs`), so a
/// `PUT` to a mistyped path handed straight through to mosd does not fail —
/// it grows a new subtree, of whatever kind the schema defaults to, and the
/// reconciler is the first thing to notice. That exact failure is reachable on
/// `POST /network/peers/add`, where adding a peer to an
/// undeclared `wg9` writes a physical-kind `network.wg9` carrying a WireGuard
/// block. Every path this list does not carry is refused by
/// [`settings_write_refusal`] before any bus call is made.
const WRITABLE_SETTINGS: [(&str, ScalarShape); 6] = [
    ("hostname", ScalarShape::Hostname),
    ("access.ssh.enabled", ScalarShape::Flag),
    ("container.enabled", ScalarShape::Flag),
    ("mqtt.enabled", ScalarShape::Flag),
    ("time.ntp.servers", ScalarShape::NtpServers),
    ("time.timezone", ScalarShape::Timezone),
];

/// The resource the write route's not-found envelope names, being the settings
/// root itself: what is absent is a place in the tree, not an item in a list.
const SETTINGS_COLLECTION: &str = "settings";

/// The sentence a refusal carries when nothing more specific is true of the
/// path: it is a real part of the tree, and this route is not how it is
/// written.
const WRITES_SIX: &str = "this route writes `hostname`, `access.ssh.enabled`, `container.enabled`, `mqtt.enabled`, `time.ntp.servers` and `time.timezone` and no other dot-path; every other subtree is written through its own resource route";

/// The shape `path` is written with, when this route writes it at all.
fn writable_shape(path: &str) -> Option<ScalarShape> {
    WRITABLE_SETTINGS
        .iter()
        .find(|(writable, _)| *writable == path)
        .map(|(_, shape)| *shape)
}

/// `value` checked against `shape`, or the sentence the 422 carries.
fn check_scalar(shape: ScalarShape, value: &Value) -> Result<(), String> {
    match shape {
        ScalarShape::Flag => value.is_boolean().then_some(()).ok_or_else(|| {
            "this setting is a switch: the body is the JSON literal `true` or `false`".to_string()
        }),
        ScalarShape::Hostname => {
            let name = value
                .as_str()
                .ok_or_else(|| "this setting is text: the body is a JSON string".to_string())?;
            // Not trimmed, unlike `hostname_submit`. That handler trims because
            // a browser sends whatever was typed into a text input; a client
            // that built a JSON string chose its bytes, and silently writing
            // something other than what it sent is the worse answer.
            valid_hostname(name)
                .then_some(())
                .ok_or_else(|| HOSTNAME_RULES.to_string())
        }
        ScalarShape::Timezone => {
            let zone = value
                .as_str()
                .ok_or_else(|| "this setting is text: the body is a JSON string".to_string())?;
            mosd_settings::validate_timezone_name(zone)
        }
        ScalarShape::NtpServers => {
            let servers: Vec<String> = value
                .as_array()
                .and_then(|items| {
                    items
                        .iter()
                        .map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .ok_or_else(|| {
                    "this setting is a list: the body is a JSON array of server strings".to_string()
                })?;
            mosd_settings::validate_ntp_servers(&servers)
        }
    }
}

/// Whether `root` is a top-level key of the settings schema.
///
/// Derived from `Settings::default()` rather than listed here: every field of
/// that struct serialises unconditionally, so the default tree's key set *is*
/// the schema's top level, and a key added to the struct is covered with no
/// edit in apid. A hand-written list would be a second opinion about a schema
/// that already exists, and second opinions drift.
fn is_settings_root(root: &str) -> bool {
    serde_json::to_value(mosd_settings::Settings::default())
        .is_ok_and(|tree| tree.get(root).is_some())
}

/// Why this route will not write `path`, in §2.4's envelope.
///
/// Three answers, and they are not interchangeable. **422** is a path that is
/// not a path — an empty segment, an unterminated quote — which is the
/// *malformed* half of the
/// rule. **404** is a path that names nothing, which is the *well-formed but
/// absent* half, and it comes from [`item_not_found`] so the rule is inherited
/// rather than remembered. **409** is a path that names something real which
/// this route does not write, which is the condition §2.4 already spends 409
/// on: the body is well formed and nothing about it is wrong, and what refuses
/// it is the state of the surface.
///
/// "Names nothing" is decided on the **first segment** and not on the whole
/// path, and that is a statement about writes rather than a shortcut. A write's
/// job may be to create the leaf it names — `Settings::set` creates missing
/// intermediates — so a missing leaf is not an absent resource. The one thing
/// a write cannot create is a top-level key the typed schema has no field for:
/// such a tree does not deserialize, so `network.eth9.dhcp` is a write that
/// may legitimately create `eth9`, while `netwrok.eth0.dhcp` can never be
/// anything but a typo.
fn settings_write_refusal(path: &str) -> Response {
    let refused = |message: String| {
        api_response(
            StatusCode::CONFLICT,
            ApiError::apid("settings_read_only", message).at(path),
        )
    };
    // `.` is the whole tree, not a malformed path: `Settings::set` documents
    // `""` and `"."` as replacing the root. It is a real path this route
    // refuses, so it takes the refusal rather than the 422 below.
    if path == "." {
        return refused(WRITES_SIX.to_string());
    }
    let Some(segments) = mosd_settings::path_segments(path) else {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                "not a settings dot-path: segments are separated by `.`, a segment is either bare or double-quoted, and no segment is empty".to_string(),
            )
            .at(path),
        );
    };
    // `path_segments` yields at least one segment for every path it accepts.
    let root = segments[0].as_str();
    if !is_settings_root(root) {
        return item_not_found(SETTINGS_COLLECTION, path);
    }
    refused(match root {
        // Read-only in the tree itself and not merely here, which is a
        // different sentence from the one below: `Settings::set` answers
        // `SettingsError::ReadOnly` for it, and no later milestone widens this
        // route to cover it.
        "schema_version" => "`schema_version` is read-only in the settings tree itself: the store refuses every write that would change it, and the version moves only when a migration moves it".to_string(),
        // Named rather than folded into the sentence below, because this is
        // the subtree where a passthrough is actively destructive rather than
        // merely wrong.
        "network" => "the `network` subtree is not written through this route: it is written through the typed network routes — `PUT /api/v1/network/{iface}` and the `DELETE` beside it, `PUT /api/v1/network` for the whole map, and the peer collection under each interface. A raw write here would create an entry of the default kind for an interface that has none, and would run none of the relational rules: a bridge naming a port that does not exist would be accepted".to_string(),
        _ => WRITES_SIX.to_string(),
    })
}

/// The body of a settings `PUT`: the value to write, and nothing around it.
///
/// A bare JSON value, the same shape `GET` answers.
///
/// The schema is wide because the dot-path decides what is acceptable. What
/// is actually accepted is narrow: a JSON string for `hostname` and
/// `time.timezone`, `true` or `false` for the three switches, and a JSON
/// array of server strings for `time.ntp.servers`. Anything else is **422**.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub(crate) struct SettingsWrite(Value);

// The success body names the queued apply only; it never echoes the setting,
// which would invite a client to trust the echo over its own GET.
/// Write one scalar setting by dot-path.
///
/// Accepts six paths and no others: `hostname`, `access.ssh.enabled`,
/// `container.enabled`, `mqtt.enabled`, `time.ntp.servers` and
/// `time.timezone`. Any other path is refused.
///
/// Answers **202** with the queued task id on success. Takes a bearer token.
#[utoipa::path(
    put,
    path = V1_SETTINGS_DOC,
    context_path = API,
    tag = "resources",
    params(("path" = String, Path, description = "The settings dot-path to write: `hostname`, `access.ssh.enabled`, `container.enabled`, `mqtt.enabled`, `time.ntp.servers` or `time.timezone`")),
    request_body = SettingsWrite,
    responses(
        (status = 202, description = "The value was persisted and its scoped reconciliation was queued", body = TaskAccepted),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "The dot-path names no root the settings schema has (`settings_not_found`)", body = ApiError),
        (status = 409, description = "A dot-path that exists and that this route does not write (`settings_read_only`)", body = ApiError),
        (status = 422, description = "The body carries the redaction sentinel, or is the wrong shape for this setting, or the dot-path is malformed (`validation_failed`); or mosd rejected the write (`settings_rejected`)", body = ApiError),
        (status = 500, description = "mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_settings_write(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(path): Path<String>,
    Source(source): Source,
    body: Result<Json<SettingsWrite>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(SettingsWrite(value)) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return api_response(
                StatusCode::BAD_REQUEST,
                ApiError::apid("request_invalid", rejection.body_text()).at(&path),
            );
        }
    };
    // Before the allowlist and not after it, deliberately. This is a rule about
    // the body rather than about the path, so a later milestone that widens the
    // allowlist inherits it instead of stepping around it, and the client this
    // protects — one that read a subtree, edited a field and wrote the whole
    // thing back — is answered about the thing it actually got wrong.
    if redact::carries_sentinel(&value) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                format!(
                    "the body carries `{}`, which is what a read substitutes for a secret and never a value to write: writing it back would destroy the credential it stands for. Send only the fields you meant to change",
                    redact::REDACTED
                ),
            )
            .at(&path),
        );
    }
    let Some(shape) = writable_shape(&path) else {
        return settings_write_refusal(&path);
    };
    if let Err(message) = check_scalar(shape, &value) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", message).at(&path),
        );
    }
    let task_id = match state.api.set_settings(&path, &value).await {
        Ok(task_id) => task_id,
        Err(err) => return bus_api_error(&err, Some(&path)),
    };
    state.audit.record("settings-write", "accepted", &source);
    // No `access_cache` invalidation, and that is not an omission: mosd emits
    // `SettingsChanged` for the path it wrote and the subscription drops the
    // cache for anything under `access`, which is exactly what the `access.ssh`
    // form path already relies on. The token routes invalidate by hand because
    // a revocation must bite on the very next request; nothing here is a
    // credential.
    api_response(StatusCode::ACCEPTED, TaskAccepted { task_id })
}

/// List mosd's bounded apply-task history, oldest first.
#[utoipa::path(
    get,
    path = V1_TASKS_PATH,
    context_path = API,
    tag = "tasks",
    responses(
        (status = 200, description = "The bounded apply-task history, oldest first", body = Vec<TaskRecord>),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd returned an invalid task record or failed to answer (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_tasks_list(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match state.task_records().await {
        Ok(tasks) => api_response(StatusCode::OK, tasks),
        Err(err) => bus_api_error(&err, None),
    }
}

/// Read one apply task by the id returned with a settings or transient-password write.
#[utoipa::path(
    get,
    path = V1_TASK_ROUTE,
    context_path = API,
    tag = "tasks",
    params(("id" = String, Path, description = "The task id returned by a 202 response")),
    responses(
        (status = 200, description = "The latest known task record", body = TaskRecord),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 404, description = "No retained task has this id (`task_not_found`)", body = ApiError),
        (status = 500, description = "mosd returned an invalid task record or failed to answer (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_task(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.task_record(&id).await {
        Ok(task) => api_response(StatusCode::OK, task),
        Err(err) => task_api_error(&err, &id),
    }
}

/// Read the live-state tree at a dot-path.
///
/// Live state is observed, not configured, and is a separate root from
/// settings. Read-only: there is no write counterpart. Answers **404** when
/// the path names nothing and **422** when it is not a well-formed dot-path.
#[utoipa::path(
    get,
    path = V1_STATE_DOC,
    context_path = API,
    tag = "resources",
    params(("path" = String, Path, description = "The live-state dot-path, verbatim: `hostname`, `network`, `power`")),
    responses(
        (status = 200, description = "The value at the dot-path, redacted", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 404, description = "The dot-path does not resolve (`settings_not_found`)", body = ApiError),
        (status = 422, description = "mosd rejected the dot-path (`settings_rejected`); a dot-path that does not resolve is the 404 above", body = ApiError),
        (status = 500, description = "mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_state(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> Response {
    let value = state.api.get_state(&path).await;
    resource_response(value, &path)
}

/// Read the time-synchronization status.
///
/// Observed from timesyncd at request time and classified by mosd:
/// `synchronized`, `synchronizing`, `offline-degraded` (no reachable server;
/// retries continue on the pinned 30-second policy), `invalid-source` (a
/// server answered and its replies cannot be used), or `unknown` (timesyncd
/// itself is not observable). Read-only: there is no route that pauses or
/// stops synchronization.
#[utoipa::path(
    get,
    path = V1_TIME_STATUS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The classified status with the evidence it rests on: the selected server, the kernel's synchronized bit, and the last sample with its offset and a `correction` member telling a clock step from ordinary drift", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to observe (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_time_status(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    // No dot-path: the status names no setting, so a failure envelope carries
    // no `path` member — the power actions' shape.
    match state.api.get_time_status().await {
        Ok(value) => api_response(StatusCode::OK, ResourceValue(redact::redact(value, ""))),
        Err(err) => bus_api_error(&err, None),
    }
}

/// Read the storage status.
///
/// Observed by mosd at request time: every fixed tier of the layout (the A/B
/// rootfs slots, boot, META, STATE, EPHEMERAL/`var` and DATA at `/mnt/data`)
/// with its device, size, mount and read-only state, its space accounting
/// including the filesystem's reserved pool, and whatever the system recorded
/// about its last check; PLAN-063's two bind namespaces, `/mos` and `/srv`,
/// each with its readiness against the DATA tier it must live on; every
/// physical medium with normalized wear where the device exports it and an
/// explicit `unsupported` with a reason where it does not; the low-space
/// thresholds with their hysteresis band; the reserved update workspace under
/// `/mos/updates`; and the explicit lifecycle decisions.
///
/// `/mos` and `/srv` are two namespaces of ONE filesystem and share its
/// capacity pool, so their bytes are reported once, on the `data` tier, and
/// never a second time under each bind.
///
/// Read-only. There is no route that formats, repartitions, resizes, mounts
/// or erases storage, and adding one is a product decision this surface does
/// not anticipate.
#[utoipa::path(
    get,
    path = V1_STORAGE_STATUS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The fixed tiers with their space, mount and check evidence; the `/mos` and `/srv` bind namespaces with their readiness (one shared capacity pool, reported once on the `data` tier); the physical media with normalized wear or an explicit `unsupported` reason; the low-space policy and reserved update workspace; and the explicit data-lifecycle decisions", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to observe (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_storage_status(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    // No dot-path: the status names no setting, so a failure envelope carries
    // no `path` member — the time status's shape.
    match state.api.get_storage_status().await {
        Ok(value) => api_response(StatusCode::OK, ResourceValue(redact::redact(value, ""))),
        Err(err) => bus_api_error(&err, None),
    }
}

/// Read the system-information surface.
///
/// One read answers what this device is: the machine id, the board, the
/// kernel, the distribution release, the image version with its git stamp
/// and build date, every installed package with its version (from the
/// shipped manifest), the booted slot and the uptime. mosd assembles it at
/// request time from the seams that already carry each fact; nothing is
/// restated. Every member carries `available`, and an absent fact says why.
#[utoipa::path(
    get,
    path = V1_SYSTEM_INFO_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The surface: `machineId`, `board`, `kernel`, `release`, `system` (version, `gitStamp`, `commitDate`, `fileEpoch`), `daemon`, `packages`, `slot`, `uptime`; each an object carrying `available`", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to observe (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_system_info(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match state.api.get_system_info().await {
        Ok(value) => api_response(StatusCode::OK, ResourceValue(redact::redact(value, ""))),
        Err(err) => bus_api_error(&err, None),
    }
}

/// Read the board telemetry.
///
/// Temperature (thermal zones and hwmon inputs), every watchdog device with
/// its boot status, and the reset reason as far as the kernel's generic
/// sources tell it: a watchdog's `cardReset` flag or a crash record in
/// pstore. Absence is explicit — a board that exports no source reports
/// `available: false` with the reason, never a healthy reading.
#[utoipa::path(
    get,
    path = V1_SYSTEM_TELEMETRY_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "`thermal`, `watchdog` and `reset`, each carrying `available`; `reset.reason` is `watchdog`, `kernel-crash` or `unknown`, with the evidence beside it", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to observe (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_system_telemetry(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match state.api.get_telemetry().await {
        Ok(value) => api_response(StatusCode::OK, ResourceValue(redact::redact(value, ""))),
        Err(err) => bus_api_error(&err, None),
    }
}

/// Read the observed network state.
///
/// What the network stack actually sees, distinct from the desired map at
/// `/v1/network`: per interface the link and carrier state, the addresses
/// with where each came from, the DHCP lease, the DNS servers, and the
/// Wi-Fi association; the default routes; DNS reachability by a single
/// bounded probe through resolved; and the radio and modem capabilities,
/// with cellular explicitly unsupported. Nothing from the settings tree is
/// in this answer, so a configured interface with no carrier reads as
/// exactly that.
#[utoipa::path(
    get,
    path = V1_NETWORK_STATUS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "`interfaces`, `defaultRoutes`, `dns`, `wifi` and `capabilities`, each carrying `available` or `supported`; absent evidence carries the reason", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "mosd failed to observe (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_network_status(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match state.api.get_observed_network().await {
        Ok(value) => api_response(StatusCode::OK, ResourceValue(redact::redact(value, ""))),
        Err(err) => bus_api_error(&err, None),
    }
}

/// The retention and schema facts a client needs to read the collection.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotRetention {
    /// The most snapshots kept; publishing one more removes the oldest.
    max_snapshots: usize,
    /// The most bytes kept across all snapshots.
    max_total_bytes: u64,
    /// The most bytes one snapshot may be; larger is refused.
    max_snapshot_bytes: usize,
    /// The snapshot schema version this build produces.
    schema_version: u64,
    /// The redaction schema version this build applies.
    redaction_schema_version: u64,
}

/// The snapshot collection.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotList {
    /// Every stored snapshot, oldest first.
    snapshots: Vec<SnapshotSummary>,
    /// The bounds the store enforces.
    retention: SnapshotRetention,
}

/// The answer to a collection: the stored snapshot and how collecting went.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotCollected {
    /// The snapshot as the list describes it.
    snapshot: SnapshotSummary,
    /// Wall time the collection took.
    elapsed_millis: u64,
    /// Per source, `ok`, `unavailable` or `timeout`.
    sections: std::collections::BTreeMap<String, String>,
    /// Fields the redaction schema did not name and therefore dropped.
    dropped_fields: usize,
    /// Fields and strings the redaction pass replaced.
    redacted_fields: usize,
}

fn snapshot_retention() -> SnapshotRetention {
    SnapshotRetention {
        max_snapshots: diagnostics::MAX_SNAPSHOTS,
        max_total_bytes: diagnostics::MAX_TOTAL_BYTES,
        max_snapshot_bytes: diagnostics::MAX_SNAPSHOT_BYTES,
        schema_version: diagnostics::SCHEMA_VERSION,
        redaction_schema_version: diagnostics::REDACTION_SCHEMA_VERSION,
    }
}

/// A store failure, as §2.4's envelope: apid's own, 500.
fn diagnostics_io_error(err: &anyhow::Error, doing: &str) -> Response {
    tracing::error!(error = %err, "diagnostics store: {doing} failed");
    api_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        ApiError::apid("diagnostics_io", format!("{doing} failed: {err:#}")),
    )
}

fn snapshot_not_found(id: &str) -> Response {
    api_response(
        StatusCode::NOT_FOUND,
        ApiError::apid(
            "snapshot_not_found",
            format!("no diagnostic snapshot `{id}`"),
        ),
    )
}

/// List the stored diagnostic snapshots.
///
/// Oldest first, each with its id, size, `collectedAt`, schema version and
/// machine id, plus the retention bounds the store enforces.
#[utoipa::path(
    get,
    path = V1_DIAGNOSTICS_SNAPSHOTS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The stored snapshots and the retention bounds", body = SnapshotList),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "The store could not be read (`diagnostics_io`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_diagnostics_list(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    let store = Arc::clone(&state.diagnostics);
    match tokio::task::spawn_blocking(move || store.list()).await {
        Ok(Ok(snapshots)) => api_response(
            StatusCode::OK,
            SnapshotList {
                snapshots,
                retention: snapshot_retention(),
            },
        ),
        Ok(Err(err)) => diagnostics_io_error(&err, "listing snapshots"),
        Err(err) => diagnostics_io_error(&anyhow::anyhow!(err), "listing snapshots"),
    }
}

/// Collect a diagnostic snapshot.
///
/// Reads every mosd surface the schema names — system information,
/// telemetry, failure evidence, storage, time, observed network, live
/// state — under a per-section timeout inside one deadline, assembles the
/// versioned snapshot, redacts it against the allowlist schema (a field the
/// schema does not name does not ship), and publishes it whole to the store
/// or not at all. A source that does not answer is an absent member with
/// the reason; the snapshot is produced regardless. Bounded on disk: the
/// oldest snapshots are removed to stay under the retention caps.
///
/// Answers **201** with the stored snapshot's summary and the collection
/// report. **409** while another collection is running.
#[utoipa::path(
    post,
    path = V1_DIAGNOSTICS_SNAPSHOTS_PATH,
    context_path = API,
    tag = "actions",
    responses(
        (status = 201, description = "The snapshot was collected, redacted and published", body = SnapshotCollected),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "A collection is already running (`diagnostics_busy`)", body = ApiError),
        (status = 500, description = "The snapshot could not be published (`diagnostics_io`): the store is unwritable or the snapshot is above the size cap; nothing was written", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_diagnostics_collect(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    let Ok(_guard) = state.collecting.try_lock() else {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "diagnostics_busy",
                "a snapshot is being collected; retry when it has been published".to_string(),
            ),
        );
    };
    let collected = Collector::new(state.api.as_ref()).collect().await;
    let store = Arc::clone(&state.diagnostics);
    let snapshot = collected.snapshot;
    let report = collected.report;
    let published = tokio::task::spawn_blocking(move || store.publish(&snapshot)).await;
    match published {
        Ok(Ok(summary)) => {
            state
                .audit
                .record("diagnostics-snapshot", "collected", &source);
            api_response(
                StatusCode::CREATED,
                SnapshotCollected {
                    snapshot: summary,
                    elapsed_millis: u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
                    sections: report
                        .sections
                        .iter()
                        .map(|(name, status)| ((*name).to_string(), status.as_str().to_string()))
                        .collect(),
                    dropped_fields: report.redaction.dropped_fields,
                    redacted_fields: report.redaction.redacted_fields,
                },
            )
        }
        Ok(Err(err)) => {
            state
                .audit
                .record("diagnostics-snapshot", "refused", &source);
            diagnostics_io_error(&err, "publishing the snapshot")
        }
        Err(err) => diagnostics_io_error(&anyhow::anyhow!(err), "publishing the snapshot"),
    }
}

/// Export one diagnostic snapshot.
///
/// The stored bytes, verbatim and already redacted, as an attachment named
/// `mos-diagnostics-<machine id prefix>-<id>.json` so a browser saves it
/// under a name support can file. Never cached.
#[utoipa::path(
    get,
    path = V1_DIAGNOSTICS_SNAPSHOT_ROUTE,
    context_path = API,
    tag = "resources",
    params(("id" = String, Path, description = "The snapshot id from the collection listing")),
    responses(
        (status = 200, description = "The snapshot document (`schemaVersion` names its shape); `Content-Disposition: attachment`", body = ResourceValue),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 404, description = "No such snapshot (`snapshot_not_found`)", body = ApiError),
        (status = 500, description = "The store could not be read (`diagnostics_io`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_diagnostics_snapshot(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Ok(snapshot_id) = id.parse::<u64>() else {
        return snapshot_not_found(&id);
    };
    let store = Arc::clone(&state.diagnostics);
    match tokio::task::spawn_blocking(move || store.read(snapshot_id)).await {
        Ok(Ok(Some(bytes))) => {
            let machine: String = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/system/machineId/id")
                        .and_then(Value::as_str)
                        .map(|id| id.chars().take(8).collect())
                })
                .unwrap_or_else(|| "unknown".to_string());
            let disposition =
                format!("attachment; filename=\"mos-diagnostics-{machine}-{snapshot_id}.json\"");
            (
                StatusCode::OK,
                [
                    (CONTENT_TYPE, "application/json"),
                    (CACHE_CONTROL, CacheClass::NoStore.header_value()),
                    (CONTENT_DISPOSITION, disposition.as_str()),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Ok(None)) => snapshot_not_found(&id),
        Ok(Err(err)) => diagnostics_io_error(&err, "reading the snapshot"),
        Err(err) => diagnostics_io_error(&anyhow::anyhow!(err), "reading the snapshot"),
    }
}

/// Delete one diagnostic snapshot.
///
/// The explicit retention operation: the store also removes the oldest to
/// stay under its caps, and this is how an operator removes one sooner.
/// Answers **204**; **404** when there is no such snapshot.
#[utoipa::path(
    delete,
    path = V1_DIAGNOSTICS_SNAPSHOT_ROUTE,
    context_path = API,
    tag = "actions",
    params(("id" = String, Path, description = "The snapshot id from the collection listing")),
    responses(
        (status = 204, description = "The snapshot was removed"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No such snapshot (`snapshot_not_found`)", body = ApiError),
        (status = 500, description = "The store could not be written (`diagnostics_io`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_diagnostics_delete(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    Path(id): Path<String>,
) -> Response {
    let Ok(snapshot_id) = id.parse::<u64>() else {
        return snapshot_not_found(&id);
    };
    let store = Arc::clone(&state.diagnostics);
    match tokio::task::spawn_blocking(move || store.delete(snapshot_id)).await {
        Ok(Ok(true)) => {
            state
                .audit
                .record("diagnostics-snapshot", "deleted", &source);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Ok(false)) => snapshot_not_found(&id),
        Ok(Err(err)) => diagnostics_io_error(&err, "deleting the snapshot"),
        Err(err) => diagnostics_io_error(&anyhow::anyhow!(err), "deleting the snapshot"),
    }
}

/// The body of a successful key rotation: the public half, and nothing else.
///
/// There is no `privateKey` member here and there will not be one. The private
/// half never leaves mosd — there is no read-back route for the private key,
/// ever; not redacted-on-read, nonexistent — so this struct is the whole of
/// what a rotation can answer.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireguardRotation {
    /// The new base64 X25519 public key, which is what the far end needs.
    public_key: String,
}

// `iface` goes to mosd unexamined: mosd owns the "declared entry of kind
// wireguard" rule and raises a distinct error name for each half of it. A
// second copy of that rule here could disagree with the first.
/// Rotate a WireGuard interface's private key.
///
/// An action rather than a settings write: the private key lives in a
/// mode-0640 file on STATE that the settings tree does not describe. mosd
/// draws a new key, tears down the device holding the old one and reconciles.
///
/// Answers **200** with the new public key — the tunnel is already running on
/// it. The private half is never returned. **404** when `iface` names no
/// declared `network` entry; **422** when the entry exists but is not of kind
/// `wireguard`.
#[utoipa::path(
    post,
    path = V1_WIREGUARD_ROTATE_ROUTE,
    context_path = API,
    tag = "actions",
    params(("iface" = String, Path, description = "The `network` entry to rotate, which must be one of kind `wireguard`: `wg0`")),
    responses(
        (status = 200, description = "A new key was drawn; the body carries its public half", body = WireguardRotation),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "The name is not a declared `network` entry (`settings_not_found`); the URL names no interface to rotate", body = ApiError),
        (status = 422, description = "The entry exists and is not a WireGuard one (`settings_rejected`)", body = ApiError),
        (status = 500, description = "mosd failed to rotate (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_wireguard_rotate(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(iface): Path<String>,
) -> Response {
    match state.api.rotate_wireguard_key(&iface).await {
        Ok(public_key) => api_response(StatusCode::OK, WireguardRotation { public_key }),
        // §2.4's `path` is the settings dot-path at fault, and this failure has
        // one: the entry whose kind mosd refused.
        Err(err) => bus_api_error(&err, Some(&iface_settings_path(&iface))),
    }
}

// §3.2's token collection: the listing, the mint and the revocation.

// Three members and not four: the digest is not on this wire and neither is
// the plaintext.
/// One row of `GET /api/v1/tokens`.
///
/// Carries no secret: the token's plaintext appears in exactly one response,
/// when it is minted, and never again.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct ApiTokenSummary {
    /// The token's stable identity, which is also its `DELETE` path segment.
    /// Not secret: it is a lookup key, and §3.2 puts it on the wire for that.
    id: String,
    /// The operator's label, the only thing that tells one token from another.
    name: String,
    /// Seconds since the UNIX epoch as the device clock read them at the mint.
    ///
    /// **A label, never a deadline.** No unit on this image syncs a clock, so
    /// the reading may be wrong by any amount and 0 means the clock was unset.
    /// Tokens do not expire; revocation is the whole lifecycle (§3.2).
    created: u64,
}

/// `POST /api/v1/tokens` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct MintTokenRequest {
    /// The label the new token is listed under.
    name: String,
}

/// `POST /api/v1/tokens` response body: the one place a plaintext token
/// appears.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct MintedToken {
    /// The new token's identity, for a later `DELETE`.
    id: String,
    /// The label as it was submitted.
    name: String,
    /// The whole token, `mos_<id>_<secret>`.
    ///
    /// **It appears here and nowhere else, ever.** Only the SHA-256 digest is
    /// stored, so a token that is lost is replaced and never recovered -- the
    /// posture `access.device` already takes.
    token: String,
}

/// List every API token this device holds.
///
/// Returns each token's id, label and creation time. Secrets are not stored
/// and are never returned — a token's secret is shown once, when it is
/// minted.
#[utoipa::path(
    get,
    path = V1_TOKENS_PATH,
    context_path = API,
    tag = "tokens",
    responses(
        (status = 200, description = "The stored tokens: `id`, `name` and `created`, never the digest and never the plaintext", body = Vec<ApiTokenSummary>),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a token list (`settings_invalid`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_tokens_list(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match stored_tokens(&state).await {
        Ok(tokens) => api_response(
            StatusCode::OK,
            tokens
                .into_iter()
                .map(|entry| ApiTokenSummary {
                    id: entry.id,
                    name: entry.name,
                    created: entry.created,
                })
                .collect::<Vec<_>>(),
        ),
        Err(response) => *response,
    }
}

// Cookie deliberately not accepted: that would put a permanent-credential
// factory on the browser surface.
/// Mint an API token.
///
/// Takes either a stored bearer token or an authenticated browser session. The
/// secret is returned once, in this response, and is not retrievable
/// afterwards.
///
/// A signed-in browser can mint the first additional token through this API.
#[utoipa::path(
    post,
    path = V1_TOKENS_PATH,
    context_path = API,
    tag = "tokens",
    request_body = MintTokenRequest,
    responses(
        (status = 201, description = "The token was created; the body carries the plaintext, which is not recoverable afterwards", body = MintedToken),
        (status = 400, description = "The body is not JSON, or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "The device already holds the maximum number of tokens (`token_limit_reached`); revoke one first", body = ApiError),
        (status = 422, description = "The name is empty, over 256 bytes, or holds a control character (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a token list (`settings_invalid`), no free id was drawn (`mint_failed`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_tokens_mint(
    _credential: ApiCredential,
    State(state): State<AppState>,
    body: Result<Json<MintTokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return api_response(
                StatusCode::BAD_REQUEST,
                ApiError::apid("request_invalid", rejection.body_text()),
            );
        }
    };
    let mut tokens = match stored_tokens(&state).await {
        Ok(tokens) => tokens,
        Err(response) => return *response,
    };
    // The cap is answered here and not only by the store. The validator makes
    // a full list a hard refusal, and without this check the caller meets that
    // refusal as a failed write -- a 500 about mosd -- rather than as an answer
    // about the request they made. 409 and not 422: the body is well formed and
    // nothing about it is wrong, and what refuses it is the collection's
    // current state, which is the condition §2.4 already spends 409 on.
    if tokens.len() >= mosd_settings::MAX_TOKENS {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "token_limit_reached",
                format!(
                    "this device already holds the maximum of {} API tokens; revoke one before minting another",
                    mosd_settings::MAX_TOKENS
                ),
            )
            .at(API_TOKENS_PATH),
        );
    }
    let Some(minted) = token::mint(&tokens) else {
        return api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid(
                "mint_failed",
                "no free token id was drawn; nothing was written".to_string(),
            )
            .at(API_TOKENS_PATH),
        );
    };
    tokens.push(ApiToken {
        id: minted.id.clone(),
        name: request.name.clone(),
        hash: minted.hash,
        created: device_clock_seconds(),
    });
    if let Err(response) = write_tokens(&state, &tokens).await {
        return *response;
    }
    api_response(
        StatusCode::CREATED,
        MintedToken {
            id: minted.id,
            name: request.name,
            token: minted.wire,
        },
    )
}

// Identity is the id and never a list position: an index is meaningful only
// against the list the caller last read, and a concurrent mint slides it.
/// Revoke one API token by its id.
///
/// Takes effect on the next request; the token set is read per request.
/// Answers **404** when no token carries that id.
#[utoipa::path(
    delete,
    path = V1_TOKEN_ROUTE,
    context_path = API,
    tag = "tokens",
    params(("id" = String, Path, description = "The token id, as `POST /api/v1/tokens` returned it: 1 to 64 lowercase hex characters")),
    responses(
        (status = 204, description = "The token was revoked; it stops being accepted on the next request"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No stored token carries that id (`settings_not_found`). Well-formed and absent, which is a different answer from malformed", body = ApiError),
        (status = 422, description = "The id is not a token id at all (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a token list (`settings_invalid`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_tokens_revoke(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    // Malformed and absent are different answers and must not share a status.
    // An id that is not an id could never name
    // an entry, so a 404 here would send the caller looking for a token they
    // deleted instead of at the URL they typed.
    if !mosd_settings::is_api_token_id(&id) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                "a token id is 1 to 64 lowercase hex characters".to_string(),
            )
            .at(API_TOKENS_PATH),
        );
    }
    let mut tokens = match stored_tokens(&state).await {
        Ok(tokens) => tokens,
        Err(response) => return *response,
    };
    let Some(index) = tokens.iter().position(|entry| entry.id == id) else {
        return item_not_found(API_TOKENS_PATH, &id);
    };
    tokens.remove(index);
    if let Err(response) = write_tokens(&state, &tokens).await {
        return *response;
    }
    (
        StatusCode::NO_CONTENT,
        [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
    )
        .into_response()
}

/// §2.4's envelope for the condition every API collection item route shares: a
/// well-formed identifier that names no item.
///
/// One shared function and not one per handler, which is what the rule
/// requires: a collection route added later inherits the 404 by reaching for
/// this, rather than by remembering a
/// decision, and the 422 beside it stays reserved for an identifier that is not
/// well formed at all.
///
/// The HTML panes answer **422** for the same condition, deliberately and on
/// the record. Its paired test is
/// `the_builtin_revoke_pane_answers_422_where_the_api_answers_404`.
fn item_not_found(collection: &str, identifier: &str) -> Response {
    api_response(
        StatusCode::NOT_FOUND,
        ApiError::apid(
            "settings_not_found",
            format!("no item of `{collection}` is identified by `{identifier}`"),
        )
        .at(collection),
    )
}

/// The stored token list, or the envelope for whatever prevented reading it.
///
/// The read is direct rather than from the gate's `access` cache: this is the
/// read half of a read-modify-write, and the freshest list is the one least
/// likely to drop somebody else's entry.
async fn stored_tokens(state: &AppState) -> Result<Vec<ApiToken>, Box<Response>> {
    let access = match state.api.get_settings("access").await {
        Ok(value) => value,
        Err(err) => return Err(Box::new(bus_api_error(&err, Some(API_TOKENS_PATH)))),
    };
    parse_tokens(&access).map_err(|err| {
        Box::new(api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid(
                "settings_invalid",
                format!("the stored token list could not be read: {err}"),
            )
            .at(API_TOKENS_PATH),
        ))
    })
}

/// The token list inside an `access` subtree.
///
/// An absent list is an empty list -- the model omits the field entirely when
/// nothing is stored -- but a list that is present and unreadable is an error
/// and never an empty list, for the reason the SSH key list gives: treating it
/// as empty would let a mint or a revoke overwrite tokens the operator cannot
/// see.
fn parse_tokens(access: &Value) -> Result<Vec<ApiToken>, serde_json::Error> {
    match access.get("apiTokens") {
        Some(value) => serde_json::from_value(value.clone()),
        None => Ok(Vec::new()),
    }
}

/// Validate and write a rewritten token list.
///
/// Read-modify-write of the whole array, because the dot-path syntax has no
/// array indexing -- the same pattern the SSH key pane uses. **Two concurrent
/// mints lose one token, silently**: both read the list, both append to their
/// own copy, and the second write wins. It is recorded and not fixed; §3.2
/// names it as a cost inherited from the tree, and the alternative is a locking
/// scheme this codebase does not have.
async fn write_tokens(state: &AppState, tokens: &[ApiToken]) -> Result<(), Box<Response>> {
    // The same validator mosd runs, so a list this route accepts is one the
    // store will accept too. Its message names an entry index and never echoes
    // a digest or an id.
    if let Err(err) = validate_api_tokens(tokens) {
        return Err(Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", key_error_message(&err)).at(API_TOKENS_PATH),
        )));
    }
    // Infallible: `ApiToken` is a struct of scalars with no map keys to collide.
    let value = serde_json::to_value(tokens).expect("api tokens serialize");
    if let Err(err) = state.api.set_settings(API_TOKENS_PATH, &value).await {
        return Err(Box::new(bus_api_error(&err, Some(API_TOKENS_PATH))));
    }
    // apid knows its own `access` write happened, so the gate's cache is
    // dropped here rather than waiting for the `SettingsChanged` round trip.
    // The bearer check reads the same subtree, and this is what makes a
    // revocation take effect on the next request.
    state.access_cache.invalidate();
    Ok(())
}

/// The device clock in seconds since the UNIX epoch, saturating at 0.
///
/// A label and never a deadline; see [`ApiTokenSummary::created`]. A clock
/// before the epoch reads 0 rather than failing a mint, because an untrusted
/// clock must not decide whether the operator may hold a credential.
fn device_clock_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

// M5's two array collections: the SSH
// authorized keys, whose identity is a fingerprint, and the WiFi station's
// known networks, whose identity is an SSID.
//
// Both are a read-modify-write of a whole array, because the dot-path syntax
// has no array indexing -- the pattern [`write_tokens`] already records, and
// the reason `redact`'s field list is names rather than dot-paths. **Two
// concurrent writes lose one entry, silently**: both read the list, both edit
// their own copy, and the second write wins. It is recorded and not fixed, for
// the reason section 3.2 gives about the token list: the alternative is a
// locking scheme this codebase does not have.

/// One row of `GET /api/v1/ssh/authorized-keys`.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AuthorizedKeyEntry {
    /// Canonical `<type> <blob>` key text, with the comment in its own field
    /// -- what the parser stored, and not what was submitted.
    key: String,
    /// The operator's label, absent when the key was stored without one.
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    /// This entry's `DELETE` path segment: `SHA256:` followed by the unpadded
    /// base64 of the SHA-256 digest of the decoded blob — the string
    /// `ssh-keygen -lf` prints.
    ///
    /// `null` for a stored line whose blob does not decode. Nothing this route
    /// writes can be in that state -- `parse_authorized_key` refuses it -- but
    /// the settings file is an editable file on STATE, so a list read back is
    /// not necessarily a list this API wrote.
    fingerprint: Option<String>,
}

/// `GET /api/v1/ssh/authorized-keys` response body.
///
/// An object and not a bare array, because of `notice`: the sentence is a
/// property of the collection rather than of any entry, and the contract
/// requires it on the listing **and** on the add. A client that only ever adds keys must still be told what a key
/// grants.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AuthorizedKeyList {
    /// Every stored key, in stored order.
    keys: Vec<AuthorizedKeyEntry>,
    /// Why a key added here is not a key with limited access.
    notice: String,
}

/// `POST /api/v1/ssh/authorized-keys` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct AddAuthorizedKeyRequest {
    /// One authorized-key line, `<type> <blob>` with an optional trailing
    /// comment: exactly what the pane's field takes, handed to exactly the
    /// same parser.
    key: String,
}

/// `POST /api/v1/ssh/authorized-keys` response body.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AddedAuthorizedKey {
    /// The entry as it was stored, canonicalised by the parser, carrying the
    /// fingerprint that is now its `DELETE` path segment.
    key: AuthorizedKeyEntry,
    /// The same sentence the listing carries.
    notice: String,
}

/// One known WiFi network, in both directions.
///
// Held field-for-field against `mosd_settings::WifiNetwork` by a test, so a
// field added to the model cannot go undocumented here.
///
/// `psk` differs by direction. On the way **in** it is the pre-shared key. On
/// the way **out** it is `"<redacted>"` whenever a key is stored. A body
/// carrying the sentinel back is refused at **422** rather than written.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct WifiNetworkEntry {
    /// The network name, which is also this entry's `DELETE` path segment.
    ssid: String,
    /// The pre-shared key; absent for an open network, `"<redacted>"` on
    /// every read of a network that has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    psk: Option<String>,
    /// Whether the network hides its SSID.
    hidden: bool,
    /// Selection preference; higher wins.
    priority: i32,
}

/// `SHA256:`, the prefix every fingerprint this device computes carries.
const FINGERPRINT_PREFIX: &str = "SHA256:";

/// Characters of unpadded base64 a 32-byte digest occupies.
///
/// A SHA-256 digest is 32 bytes, and unpadded base64 spends four characters on
/// every three bytes: 43 for 32 bytes, with no padding written.
const FINGERPRINT_DIGEST_CHARS: usize = 43;

/// Whether `value` is spelled like a fingerprint at all.
///
/// This is what gives section 2.4's rule both of its halves on the SSH item
/// route. A string that could never be a fingerprint is **422**, because it
/// names nothing and could name nothing; a well-formed fingerprint that
/// matches no stored key is **404**, from [`item_not_found`].
///
/// The alphabet includes `/` and `+`: it is standard base64 and not the
/// URL-safe variant, because that is what `ssh-keygen -lf` prints and what
/// [`ssh_fingerprint`] computes. A fingerprint carrying a `/` reaches this
/// route percent-encoded; see [`collection_item`].
fn is_ssh_fingerprint(value: &str) -> bool {
    value
        .strip_prefix(FINGERPRINT_PREFIX)
        .is_some_and(|digest| {
            digest.len() == FINGERPRINT_DIGEST_CHARS
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/')
        })
}

/// A stored key as the API answers it.
fn key_entry(entry: AuthorizedKey) -> AuthorizedKeyEntry {
    AuthorizedKeyEntry {
        fingerprint: ssh_fingerprint(&entry.key),
        key: entry.key,
        comment: entry.comment,
    }
}

/// The stored authorized-key list, or the envelope for whatever prevented
/// reading it.
///
/// The API's own read and not [`stored_keys`], for the reason the token
/// collection has two as well: the pane's helper flattens a failed bus call and
/// an unreadable stored list into one `anyhow::Error`, and section 2.4 answers
/// those with different statuses. An unreadable list is an error and never an
/// empty list -- treating it as empty would let an add overwrite keys the
/// operator cannot see.
async fn api_stored_keys(state: &AppState) -> Result<Vec<AuthorizedKey>, Box<Response>> {
    let ssh = match state.api.get_settings("access.ssh").await {
        Ok(value) => value,
        Err(err) => return Err(Box::new(bus_api_error(&err, Some(SSH_KEYS_PATH)))),
    };
    parse_key_list(&ssh).map_err(|err| {
        Box::new(api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid("settings_invalid", err.to_string()).at(SSH_KEYS_PATH),
        ))
    })
}

/// Validate and write a rewritten key list, in section 2.4's envelope.
///
/// The API's own writer and not [`write_key_list`], which answers a re-rendered
/// pane at 422 and a redirect on success. The **validator** is the same one:
/// `validate_authorized_keys` is what mosd's sshd reconciler runs before it
/// renders the file, so a list either surface accepts is a list the reconciler
/// accepts too.
async fn api_write_keys(state: &AppState, keys: &[AuthorizedKey]) -> Result<(), Box<Response>> {
    if let Err(err) = validate_authorized_keys(keys) {
        return Err(Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", key_error_message(&err)).at(SSH_KEYS_PATH),
        )));
    }
    // Infallible: `AuthorizedKey` is a struct of strings with no map keys that
    // could collide.
    let value = serde_json::to_value(keys).expect("authorized keys serialize");
    if let Err(err) = state.api.set_settings(SSH_KEYS_PATH, &value).await {
        return Err(Box::new(bus_api_error(&err, Some(SSH_KEYS_PATH))));
    }
    Ok(())
}

/// List every authorized SSH key.
///
/// Each entry carries the fingerprint that identifies it for removal. The
/// response also carries a `notice`: every authorized key grants root.
#[utoipa::path(
    get,
    path = V1_SSH_KEYS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The stored keys, each with the fingerprint that is its `DELETE` path segment, and the notice every client of this collection is told", body = AuthorizedKeyList),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a key list (`settings_invalid`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ssh_keys_list(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match api_stored_keys(&state).await {
        Ok(keys) => api_response(
            StatusCode::OK,
            AuthorizedKeyList {
                keys: keys.into_iter().map(key_entry).collect(),
                notice: ROOT_KEY_NOTICE.to_string(),
            },
        ),
        Err(response) => *response,
    }
}

// The line is parsed exactly as submitted, untrimmed: surrounding whitespace
// is one of the things the parser exists to reject, and trimming here would
// accept a line mosd would not.
/// Authorize one SSH public key.
///
/// The key line is validated as submitted; a malformed line is **422**.
///
/// **Every authorized key grants root.** `AuthorizedKeysFile` is `%u`-expanded
/// over one shared list, so the response carries a `notice` saying so
/// alongside the created entry.
#[utoipa::path(
    post,
    path = V1_SSH_KEYS_PATH,
    context_path = API,
    tag = "resources",
    request_body = AddAuthorizedKeyRequest,
    responses(
        (status = 201, description = "The key was authorized; the body carries it canonicalised, with its fingerprint and the notice", body = AddedAuthorizedKey),
        (status = 400, description = "The body is not JSON, or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "A stored key already carries that public key (`key_exists`), or the device already holds the maximum number of keys (`key_limit_reached`); the collection's current state is what refuses the request, not the body", body = ApiError),
        (status = 422, description = "The line is not an authorized key, or the resulting list is one the sshd reconciler would refuse (`validation_failed`); or mosd rejected the write (`settings_rejected`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a key list (`settings_invalid`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ssh_keys_add(
    _credential: ApiCredential,
    State(state): State<AppState>,
    body: Result<Json<AddAuthorizedKeyRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return api_response(
                StatusCode::BAD_REQUEST,
                ApiError::apid("request_invalid", rejection.body_text()).at(SSH_KEYS_PATH),
            );
        }
    };
    let parsed = match parse_authorized_key(&request.key) {
        Ok(parsed) => parsed,
        Err(err) => {
            return api_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiError::apid("validation_failed", key_error_message(&err)).at(SSH_KEYS_PATH),
            );
        }
    };
    let mut keys = match api_stored_keys(&state).await {
        Ok(keys) => keys,
        Err(response) => return *response,
    };
    // Both refusals below are 409 and both are decided **here**, before the
    // shared validator runs, and neither reads a validator message to find out
    // what happened. `validate_authorized_keys` refuses a duplicate and a full
    // list too -- it has to, because the settings file is writable without apid
    // -- but it refuses them as one `Validation` error alongside a malformed
    // key, and inferring which by matching its words would be a parser for
    // prose. The comparison and the bound are both available here, so the
    // answer is decided from the collection rather than recovered from a
    // sentence.
    //
    // The key text and not the whole line: `parse_authorized_key` splits the
    // comment off, so relabelling a stored key and posting it back is the same
    // key under a new name. That is the identity the validator itself uses.
    if keys.iter().any(|stored| stored.key == parsed.key) {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "key_exists",
                "a stored key already carries that public key; remove it before adding it again, and change its label with a remove and an add".to_string(),
            )
            .at(SSH_KEYS_PATH),
        );
    }
    // The cap, answered from `mosd_settings::MAX_KEYS` exactly as the token
    // mint answers its own from `MAX_TOKENS`.
    if keys.len() >= mosd_settings::MAX_KEYS {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "key_limit_reached",
                format!(
                    "this device already holds the maximum of {} authorized keys; remove one first",
                    mosd_settings::MAX_KEYS
                ),
            )
            .at(SSH_KEYS_PATH),
        );
    }
    keys.push(parsed.clone());
    if let Err(response) = api_write_keys(&state, &keys).await {
        return *response;
    }
    api_response(
        StatusCode::CREATED,
        AddedAuthorizedKey {
            key: key_entry(parsed),
            notice: ROOT_KEY_NOTICE.to_string(),
        },
    )
}

// 404 here where the HTML pane answers 422 on the same condition, and that
// split is deliberate: a path segment has one interpretation, a typed form
// field does not. Paired tests name each other so it cannot read as drift.
/// Remove one authorized SSH key by its fingerprint.
///
/// The fingerprint is the only accepted identifier; the key text is not.
/// Answers **404** when no authorized key carries it.
#[utoipa::path(
    delete,
    path = V1_SSH_KEY_ROUTE,
    context_path = API,
    tag = "resources",
    params(("fingerprint" = String, Path, description = "The key's fingerprint, as `GET /api/v1/ssh/authorized-keys` returns it: `SHA256:` and 43 base64 characters. Its alphabet contains `/`, so a fingerprint carrying one is percent-encoded")),
    responses(
        (status = 204, description = "The key was removed; the reconciler has re-rendered the authorized-keys file without it"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No stored key has that fingerprint (`settings_not_found`). Well-formed and absent, which is a different answer from malformed", body = ApiError),
        (status = 422, description = "The path segment is not a fingerprint at all (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a key list (`settings_invalid`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_ssh_keys_remove(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(fingerprint): Path<String>,
) -> Response {
    if !is_ssh_fingerprint(&fingerprint) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                format!(
                    "an authorized-key fingerprint is `{FINGERPRINT_PREFIX}` followed by {FINGERPRINT_DIGEST_CHARS} base64 characters, as `ssh-keygen -lf` prints it"
                ),
            )
            .at(SSH_KEYS_PATH),
        );
    }
    let mut keys = match api_stored_keys(&state).await {
        Ok(keys) => keys,
        Err(response) => return *response,
    };
    let found = keys
        .iter()
        .position(|entry| ssh_fingerprint(&entry.key).as_deref() == Some(fingerprint.as_str()));
    let Some(index) = found else {
        return item_not_found(SSH_KEYS_PATH, &fingerprint);
    };
    keys.remove(index);
    if let Err(response) = api_write_keys(&state, &keys).await {
        return *response;
    }
    (
        StatusCode::NO_CONTENT,
        [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
    )
        .into_response()
}

/// The stored WiFi networks, or the envelope for whatever prevented reading
/// them.
///
/// The same shape as [`api_stored_keys`] and [`stored_tokens`], and an
/// unreadable list is an error for the same reason: reading it as empty would
/// let one `POST` drop every network the operator cannot see, including the one
/// the device is associated through.
async fn stored_networks(state: &AppState) -> Result<Vec<WifiNetwork>, Box<Response>> {
    let value = match state.api.get_settings(WIFI_NETWORKS_PATH).await {
        Ok(value) => value,
        Err(err) => return Err(Box::new(bus_api_error(&err, Some(WIFI_NETWORKS_PATH)))),
    };
    // No absent-is-empty branch, unlike the two `access` lists above, and the
    // difference is in the model rather than in the route: `networks` carries
    // no `skip_serializing_if`, so mosd's serialization of the typed tree
    // always has it, and a dot-path that does not resolve is mosd's own
    // `NotFound` -- a 404 through `bus_api_error`, not an empty list.
    serde_json::from_value(value).map_err(|err| {
        Box::new(api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid(
                "settings_invalid",
                format!("the stored network list could not be read: {err}"),
            )
            .at(WIFI_NETWORKS_PATH),
        ))
    })
}

/// Write a rewritten network list.
///
/// There is no apid-side validator to run first, and that is a measured
/// statement rather than an omission. mosd's own write path is
/// `Settings::set`, which validates by deserializing the candidate tree
/// (`Settings::set` in `pkgs/mosd/mosd-settings/src/model.rs`) -- so the typed
/// `WifiNetwork` this route deserializes into IS the validator mosd runs — plus
/// `mosd_settings::validate_wifi_psk`, which M6 lifted out of the station
/// reconciler's renderer so that the crate holding the model states its own
/// field's rule, which was out of reach while it lived in the binary crate.
async fn write_networks(state: &AppState, networks: &[WifiNetwork]) -> Result<(), Box<Response>> {
    let value = serde_json::to_value(networks).expect("wifi networks serialize");
    if let Err(err) = state.api.set_settings(WIFI_NETWORKS_PATH, &value).await {
        return Err(Box::new(bus_api_error(&err, Some(WIFI_NETWORKS_PATH))));
    }
    Ok(())
}

/// List every known WiFi network.
///
/// Each entry's pre-shared key is redacted. Posting an entry back with the
/// redaction sentinel still in place is refused rather than stored.
#[utoipa::path(
    get,
    path = V1_WIFI_NETWORKS_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "The stored networks, in stored order, each `psk` replaced by `\"<redacted>\"`", body = Vec<WifiNetworkEntry>),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a network list (`settings_invalid`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_wifi_networks_list(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    match stored_networks(&state).await {
        Ok(networks) => {
            // Through `redact::redact` and not by rebuilding each entry field
            // by field. A second copy of the rule about which field names are
            // secret is a second opinion, and section 2.2's whole argument for
            // a structural redactor is that the one list must be the only
            // list. Infallible: `WifiNetwork` is a struct of scalars with no
            // map keys that could collide.
            let value = serde_json::to_value(&networks).expect("wifi networks serialize");
            api_response(StatusCode::OK, redact::redact(value, WIFI_NETWORKS_PATH))
        }
        Err(response) => *response,
    }
}

// This collection has no HTML pane, so there is no form-path behaviour to
// stay consistent with.
/// Add one known WiFi network.
///
/// The redaction sentinel is rejected before anything else: a client that read
/// this list and posted an entry back would otherwise store the literal
/// `"<redacted>"` as the `psk` and destroy a working key.
///
/// Answers **422** for a malformed entry or a redacted `psk`.
#[utoipa::path(
    post,
    path = V1_WIFI_NETWORKS_PATH,
    context_path = API,
    tag = "resources",
    request_body = WifiNetworkEntry,
    responses(
        (status = 201, description = "The network was stored; the body carries it back with its `psk` redacted", body = WifiNetworkEntry),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 409, description = "A stored network already carries that SSID (`ssid_exists`); the SSID is this collection's identity, so the entry is not replaced silently", body = ApiError),
        (status = 422, description = "The body carries the redaction sentinel, is not a network the settings model holds, or carries a `psk` outside IEEE 802.11i's 8..63 characters that is not a 64-digit hex PMK either (`validation_failed`); or mosd rejected the write (`settings_rejected`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a network list (`settings_invalid`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_wifi_networks_add(
    _credential: ApiCredential,
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(value) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return api_response(
                StatusCode::BAD_REQUEST,
                ApiError::apid("request_invalid", rejection.body_text()).at(WIFI_NETWORKS_PATH),
            );
        }
    };
    // Before the shape check and not after it, for the reason the scalar write
    // route gives: this is a rule about the body, and the client it protects --
    // one that read an entry, changed a field and posted the whole thing back
    // -- is answered about the thing it actually got wrong.
    if redact::carries_sentinel(&value) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                format!(
                    "the body carries `{}`, which is what a read substitutes for a secret and never a value to write: writing it back would destroy the pre-shared key it stands for. Send the key itself, or omit `psk` for an open network",
                    redact::REDACTED
                ),
            )
            .at(WIFI_NETWORKS_PATH),
        );
    }
    let network: WifiNetwork = match serde_json::from_value(value) {
        Ok(network) => network,
        Err(err) => {
            return api_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiError::apid("validation_failed", err.to_string()).at(WIFI_NETWORKS_PATH),
            );
        }
    };
    // The pre-shared key's own bounds, run here for the first time. They could
    // not be run before: they lived inside
    // `encode_psk`, a private function of the `mosd` binary crate's station
    // reconciler, so a key outside IEEE 802.11i's range was accepted, stored,
    // and refused later by the renderer with the error visible only in live
    // state. M6 lifted them into `mosd-settings` beside the typed model and
    // the reconciler now calls the lifted copy, so this is the same rule and
    // not a second one — which is what M5 refused to write, and rightly: a
    // second copy could disagree with the first.
    //
    // After the shape check and not before it, unlike the sentinel: this is a
    // rule about one field of a network, so there has to be a network first.
    if let Some(psk) = &network.psk
        && let Err(message) = mosd_settings::validate_wifi_psk(psk)
    {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", message).at(WIFI_NETWORKS_PATH),
        );
    }
    let mut networks = match stored_networks(&state).await {
        Ok(networks) => networks,
        Err(response) => return *response,
    };
    // 409 and not 422: the body is well formed and nothing about it is wrong,
    // and what refuses it is the collection's current state -- the condition
    // section 2.4 already spends 409 on, and the one the token mint's
    // `token_limit_reached` answers. Appending a second entry under one SSID
    // would also destroy the identity the item route below depends on: there
    // would be no answer to which of the two a `DELETE` names.
    if networks.iter().any(|stored| stored.ssid == network.ssid) {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "ssid_exists",
                format!(
                    "a stored network is already named `{}`; remove it before adding another under that SSID",
                    network.ssid
                ),
            )
            .at(WIFI_NETWORKS_PATH),
        );
    }
    // Redacted before the move into the list, and through the same shared
    // redactor the listing uses: what a `POST` echoes back must not be a
    // secret the `GET` beside it would have substituted.
    let echoed = redact::redact(
        serde_json::to_value(&network).expect("a wifi network serializes"),
        WIFI_NETWORKS_PATH,
    );
    networks.push(network);
    if let Err(response) = write_networks(&state, &networks).await {
        return *response;
    }
    api_response(StatusCode::CREATED, echoed)
}

// No 422 here, and that is the rule applied rather than an exception: an SSID
// has no grammar, so no path segment is malformed. Inventing a length bound to
// manufacture a 422 would refuse a network a hand-edited settings file holds.
/// Forget one known WiFi network by its SSID.
///
/// Answers **404** when no stored network carries that SSID. There is no
/// malformed-SSID case: any non-empty path segment is a possible name.
#[utoipa::path(
    delete,
    path = V1_WIFI_NETWORK_ROUTE,
    context_path = API,
    tag = "resources",
    params(("ssid" = String, Path, description = "The network name, as `GET /api/v1/wifi/client/networks` returns it")),
    responses(
        (status = 204, description = "The network was forgotten; the station reconciler has re-rendered its configuration without it"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No stored network carries that SSID (`settings_not_found`)", body = ApiError),
        (status = 500, description = "The stored list could not be read as a network list (`settings_invalid`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_wifi_networks_remove(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(ssid): Path<String>,
) -> Response {
    let mut networks = match stored_networks(&state).await {
        Ok(networks) => networks,
        Err(response) => return *response,
    };
    let Some(index) = networks.iter().position(|stored| stored.ssid == ssid) else {
        return item_not_found(WIFI_NETWORKS_PATH, &ssid);
    };
    networks.remove(index);
    if let Err(response) = write_networks(&state, &networks).await {
        return *response;
    }
    (
        StatusCode::NO_CONTENT,
        [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
    )
        .into_response()
}

// M6's network cluster: the
// interface map, one interface, and a tunnel's peer collection.
//
// **Typed, and not `PUT /api/v1/settings/network.<iface>`.** That is the one
// place the design departs from section 2.2's "the passthrough must not be
// removed" rule, and it is argued rather than assumed. The rules under
// `network` are relational -- a VLAN's parent must name a declared entry, a
// bridge port must name a declared entry, a bridge port must carry no
// addressing of its own, and no port may be claimed by two bridges -- so they
// are properties of the whole tree and not of the entry being written. The
// settings setter validates only that the tree still deserializes
// (`Settings::set` in `pkgs/mosd/mosd-settings/src/model.rs`), and the reconciler that
// does enforce them runs *after* the save with its verdict deliberately not
// propagated to the caller (`MosdService::write_setting` in `pkgs/mosd/mosd/src/bus.rs`). A raw
// passthrough therefore answers 204 to a bridge naming a port that does not
// exist and leaves the device's networking broken, with the only evidence in a
// later state read. These routes run [`validate_entries`] over the candidate
// tree exactly as the pane does, before anything is written.
//
// There is no redaction-sentinel check on this cluster, unlike the WiFi
// collection, and that is measured rather than skipped: no field of
// `IfaceSettings` or of `WireguardPeer` has a name `redact`'s list covers, so
// nothing a read of this subtree returns is ever the sentinel and no client
// can hand one back by round-tripping an entry. `privateKey` *is* on that list
// and is not in this schema -- deliberately, and the model says so -- and
// `deny_unknown_fields` refuses a body that invents it.

// These four structs exist for `openapi.json` and are never deserialized
// from: the routes parse into `mosd_settings`' own types, which are the
// validator mosd runs. A test holds each one against the model field for
// field.
/// `static` addressing.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct StaticAddressing {
    /// Interface address in CIDR notation, e.g. `192.168.1.10/24`.
    address: String,
    /// Default gateway address; absent for a link with no route of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    gateway: Option<String>,
    /// DNS server addresses.
    dns: Vec<String>,
}

/// The 802.1Q parameters of a VLAN interface.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct VlanParameters {
    /// Name of the `network` entry this VLAN sits on. **It must be a declared
    /// entry**; a body naming one that is not is refused at 422.
    parent: String,
    /// 802.1Q VLAN id.
    id: u16,
}

/// The parameters of a software bridge.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct BridgeParameters {
    /// Names of the `network` entries enslaved to this bridge. **Each must be
    /// a declared entry, must carry no addressing of its own, and must not be
    /// a port of another bridge**; a body breaking any of those is refused at
    /// 422 with the rule's own sentence.
    ports: Vec<String>,
}

/// The parameters of a WireGuard tunnel.
///
/// There is no private-key member and there never will be: this subtree is
/// served over `GET /api/v1/settings/network`, so a key in it is a key
/// published to every client. The private key lives in a mode-0640 file on the
/// device and only its public half is surfaced, through
/// `GET /api/v1/state/network` and the rotate action.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct WireguardParameters {
    /// UDP port to listen on; absent lets the kernel pick one.
    #[serde(rename = "listenPort", skip_serializing_if = "Option::is_none")]
    listen_port: Option<u16>,
    /// The far ends of the tunnel. A `PUT` of this interface replaces them;
    /// the peer collection route edits them one at a time.
    peers: Vec<WireguardPeerEntry>,
}

/// One far end of a WireGuard tunnel, in both directions.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct WireguardPeerEntry {
    /// The peer's X25519 public key: 32 bytes in padded base64. It is also
    /// this entry's `DELETE` path segment, and its alphabet contains `/`, so a
    /// key carrying one is percent-encoded there.
    #[serde(rename = "publicKey")]
    public_key: String,
    /// CIDRs routed to this peer.
    #[serde(rename = "allowedIps")]
    allowed_ips: Vec<String>,
    /// `host:port` to send to, for a peer this end initiates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    /// Keepalive interval in seconds, for a peer behind NAT.
    #[serde(
        rename = "persistentKeepalive",
        skip_serializing_if = "Option::is_none"
    )]
    persistent_keepalive: Option<u16>,
}

/// One `network` entry, as the document describes it.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct NetworkInterface {
    /// `physical`, `vlan`, `bridge` or `wireguard`. Absent means `physical`.
    ///
    /// The block below is authoritative and the interface name is not:
    /// `eth0.100` is a convention, not a declaration.
    kind: String,
    /// Whether the interface acquires its address via DHCP.
    dhcp: bool,
    /// Static addressing, used when `dhcp` is false. Absent is an interface
    /// with no addressing at all, which is what a bridge port must be.
    #[serde(rename = "static", skip_serializing_if = "Option::is_none")]
    static_: Option<StaticAddressing>,
    /// VLAN parameters, for `kind = "vlan"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    vlan: Option<VlanParameters>,
    /// Bridge parameters, for `kind = "bridge"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    bridge: Option<BridgeParameters>,
    /// WireGuard parameters, for `kind = "wireguard"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    wireguard: Option<WireguardParameters>,
}

/// The `network` subtree as typed entries, or the envelope for whatever
/// prevented reading it.
///
/// **Strict, unlike [`parse_network`]**, which the pane uses: an entry whose
/// body does not parse is an error here and never a silently dropped entry.
/// The pane can afford to skip one and name it in the page, because a human is
/// reading the result; these routes cannot. Two of them rewrite the whole map,
/// so a dropped entry is a deleted interface, and all of them validate
/// relationally, so an invisible entry turns a legal bridge port into a 422.
/// It is the posture [`api_stored_keys`] and [`stored_networks`] already take:
/// an unreadable list is an error and never an empty one.
async fn api_network_entries(state: &AppState) -> Result<NetworkEntries, Box<Response>> {
    let network = match state.api.get_settings(NETWORK_SETTINGS_PATH).await {
        Ok(value) => value,
        Err(err) => return Err(Box::new(bus_api_error(&err, Some(NETWORK_SETTINGS_PATH)))),
    };
    let (entries, unreadable) = parse_network(&network);
    if !unreadable.is_empty() {
        return Err(Box::new(api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid(
                "settings_invalid",
                format!(
                    "the stored network map holds {} entr{} this build cannot read: {}. Every route under `/api/v1/network` validates the whole map, so none of them can act while part of it is unreadable",
                    unreadable.len(),
                    if unreadable.len() == 1 { "y" } else { "ies" },
                    unreadable.join(", ")
                ),
            )
            .at(NETWORK_SETTINGS_PATH),
        )));
    }
    Ok(entries)
}

/// `iface` checked as an interface name, or the 422 that says it is not one.
///
/// Section 2.4's malformed half, on every route of this cluster. The predicate
/// is [`valid_iface_name`], the one the pane and the setup wizard already use,
/// rather than a second spelling of the same charset.
fn check_iface_name(iface: &str, path: &str) -> Result<(), Box<Response>> {
    if valid_iface_name(iface) {
        return Ok(());
    }
    Err(Box::new(api_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        ApiError::apid(
            "validation_failed",
            "an interface name is 1 to 15 characters of letters, digits, `.`, `_` or `-`"
                .to_string(),
        )
        .at(path),
    )))
}

/// `entries` refused by a relational rule, in section 2.4's envelope.
///
/// The message is [`validate_entries`]' own, which is the reconciler's own
/// sentence: no phrasing this route could pre-write would say which entry and
/// which field made the tree illegal.
fn relational_refusal(entries: &NetworkEntries, path: &str) -> Result<(), Box<Response>> {
    validate_entries(entries).map_err(|message| {
        Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", message).at(path),
        ))
    })
}

/// One entry's static address refused by the wizard's rule, in section 2.4's
/// envelope.
///
/// The message is [`validate_static_address`]'s own -- the sentence the setup
/// wizard and the network pane already print -- so the typed routes and the
/// forms cannot state the rule in two ways.
///
/// The `path` member names the entry and not the subtree the route writes: on
/// `PUT /api/v1/network` that subtree is `network`, which would not say which
/// of the entries the body carried is the wrong one.
///
/// Called on the entries a **request** carries and never on the tree as read.
/// [`api_v1_network_iface_remove`] re-validates the stored map without the
/// removed entry, and putting this rule in that shared re-validation would make
/// removing an unrelated interface start failing on bad data already on disk.
fn address_refusal(iface: &str, cfg: &IfaceSettings) -> Result<(), Box<Response>> {
    let address = cfg
        .static_
        .as_ref()
        .map_or("", |static_| static_.address.as_str());
    validate_static_address(cfg.dhcp, address).map_err(|message| {
        Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", message.to_string())
                .at(&iface_settings_path(iface)),
        ))
    })
}

/// A candidate map written back whole, at the `network` root.
async fn write_network_map(
    state: &AppState,
    entries: &NetworkEntries,
) -> Result<(), Box<Response>> {
    // Infallible: the map's keys are strings and its values are structs of
    // scalars, strings and vectors.
    let value = serde_json::to_value(entries).expect("network entries serialize");
    if let Err(err) = state.api.set_settings(NETWORK_SETTINGS_PATH, &value).await {
        return Err(Box::new(bus_api_error(&err, Some(NETWORK_SETTINGS_PATH))));
    }
    Ok(())
}

/// 204 with `no-store`, the answer every write in this cluster gives.
fn no_content() -> Response {
    (
        StatusCode::NO_CONTENT,
        [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
    )
        .into_response()
}

/// Read a JSON body, or the 400 that says it was not JSON at all.
///
/// The dot-path is an `Option` because §2.4's `path` member is: every route of
/// M6's network cluster writes one subtree and names it, but `POST /api/v1/setup`
/// writes three and a malformed body there is not about any one of them. A
/// member present with a meaningless value is worse than an absent one, which
/// is the rule [`ApiErrorDetail::path`] already states.
fn json_body<T: serde::de::DeserializeOwned>(
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
    path: Option<&str>,
) -> Result<T, Box<Response>> {
    let at = |error: ApiError| match path {
        Some(path) => error.at(path),
        None => error,
    };
    let Json(value) = body.map_err(|rejection| {
        Box::new(api_response(
            StatusCode::BAD_REQUEST,
            at(ApiError::apid("request_invalid", rejection.body_text())),
        ))
    })?;
    serde_json::from_value(value).map_err(|err| {
        Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            at(ApiError::apid("validation_failed", err.to_string())),
        ))
    })
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NetworkOverview {
    /// The declared settings map. This is desired configuration, not proof of
    /// link health.
    configured: Value,
    configured_count: usize,
    /// The current view reported by systemd-networkd.
    observed: ObservedNetwork,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObservedNetwork {
    available: bool,
    interface_count: usize,
    interfaces: Vec<ObservedInterface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ObservedInterface {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    r#type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    driver: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    administrative_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operational_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    carrier_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    address_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ipv4_address_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ipv6_address_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    online_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mtu: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hardware_address: Option<Value>,
    #[serde(default)]
    addresses: Vec<Value>,
    #[serde(default)]
    dns: Vec<Value>,
    #[serde(default)]
    routes: Vec<Value>,
}

/// Read declared network configuration together with current link state.
#[utoipa::path(
    get,
    path = V1_NETWORK_PATH,
    context_path = API,
    tag = "resources",
    responses(
        (status = 200, description = "Configured interfaces and the current systemd-networkd observation", body = NetworkOverview),
        (status = 401, description = "No API credential was supplied", body = ApiError),
        (status = 500, description = "The configured network map could not be read", body = ApiError),
        (status = 503, description = "mosd is unavailable", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_network_read(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    let configured = match state.api.get_settings(NETWORK_SETTINGS_PATH).await {
        Ok(value) => value,
        Err(err) => return bus_api_error(&err, Some(NETWORK_SETTINGS_PATH)),
    };
    let configured_count = configured.as_object().map_or(0, serde_json::Map::len);
    let observed = match state.api.get_network_state().await {
        Ok(value) => {
            let raw_interfaces = value
                .get("interfaces")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let interfaces = raw_interfaces
                .into_iter()
                .filter_map(|interface| serde_json::from_value(interface).ok())
                .collect::<Vec<ObservedInterface>>();
            let interface_count = value
                .get("interfaceCount")
                .and_then(Value::as_u64)
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(interfaces.len());
            ObservedNetwork {
                available: true,
                interface_count,
                interfaces,
                error: None,
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "live network observation unavailable");
            ObservedNetwork {
                available: false,
                interface_count: 0,
                interfaces: Vec::new(),
                error: Some("systemd-networkd state is currently unavailable"),
            }
        }
    };
    api_response(
        StatusCode::OK,
        NetworkOverview {
            configured,
            configured_count,
            observed,
        },
    )
}

/// Replace the whole interface map, validated as one tree.
///
/// The only way to make two interdependent entries legal in one step: adding a
/// bridge and its ports through the per-interface route means declaring the
/// ports first, because a bridge naming an undeclared port is refused.
///
/// Answers **422** with the failing rule's own message when the map does not
/// validate; nothing is written in that case.
#[utoipa::path(
    put,
    path = V1_NETWORK_PATH,
    context_path = API,
    tag = "resources",
    request_body = std::collections::BTreeMap<String, NetworkInterface>,
    responses(
        (status = 204, description = "The map was replaced; the reconciler has re-rendered every unit from it"),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 422, description = "The body is not a map of interfaces, a key is not an interface name, an entry declares a static address that is not IPv4 CIDR notation, or a relational rule refuses it -- a VLAN parent or a bridge port that is not a declared entry, a bridge port carrying addressing, a port claimed twice (`validation_failed`); or mosd rejected the write (`settings_rejected`)", body = ApiError),
        (status = 500, description = "mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_network_write(
    _credential: ApiCredential,
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let entries: NetworkEntries = match json_body(body, Some(NETWORK_SETTINGS_PATH)) {
        Ok(entries) => entries,
        Err(response) => return *response,
    };
    // The keys as well as the bodies. `Settings::set` checks a newly
    // introduced `network` key itself, but it checks it *after* the write is
    // built, and a 422 that says which name is wrong beats one that says the
    // tree did not deserialize.
    for iface in entries.keys() {
        if let Err(response) = check_iface_name(iface, NETWORK_SETTINGS_PATH) {
            return *response;
        }
    }
    if let Err(response) = relational_refusal(&entries, NETWORK_SETTINGS_PATH) {
        return *response;
    }
    // After the relational pass and not before it, so a body that breaks both
    // still gets the answer it got before this rule existed. Every entry here
    // is one the request carried: this route replaces the map rather than
    // merging into it.
    for (iface, cfg) in &entries {
        if let Err(response) = address_refusal(iface, cfg) {
            return *response;
        }
    }
    // No read first, deliberately: this route's whole contract is that the map
    // it sends is the map that ends up stored, so a read-modify-write would be
    // reading something it is about to discard.
    if let Err(response) = write_network_map(&state, &entries).await {
        return *response;
    }
    no_content()
}

// The HTML pane carries stored peers across a save and this route does not,
// because a form posts only the fields it renders while a JSON body says
// exactly what the client meant.
/// Declare or replace one interface, validated against the whole map.
///
/// **A `PUT` replaces the entry entirely**, including a tunnel's peer list: a
/// body with no `wireguard` block on an interface that had one leaves it with
/// none. To change one field, send the whole entry.
///
/// Creates the interface when it does not exist, so there is no 404. A name
/// outside the interface charset, or a map that fails validation, is **422**.
#[utoipa::path(
    put,
    path = V1_NETWORK_IFACE_ROUTE,
    context_path = API,
    tag = "resources",
    params(("iface" = String, Path, description = "The interface to declare or replace: `eth0`, or `eth0.100` for a VLAN. Created when it does not exist")),
    request_body = NetworkInterface,
    responses(
        (status = 204, description = "The entry was written; the reconciler has re-rendered its units"),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 422, description = "The name is not an interface name, the body is not an interface, the entry declares a static address that is not IPv4 CIDR notation, or a relational rule refuses the resulting map (`validation_failed`); or mosd rejected the write (`settings_rejected`)", body = ApiError),
        (status = 500, description = "The stored map holds an entry this build cannot read (`settings_invalid`), or mosd failed (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_network_iface_write(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(iface): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let path = iface_settings_path(&iface);
    if let Err(response) = check_iface_name(&iface, &path) {
        return *response;
    }
    let cfg: IfaceSettings = match json_body(body, Some(&path)) {
        Ok(cfg) => cfg,
        Err(response) => return *response,
    };
    let mut candidate = match api_network_entries(&state).await {
        Ok(entries) => entries,
        Err(response) => return *response,
    };
    // The candidate tree and not the one entry, for the reason the pane's
    // comment gives: every relational rule is about two entries at once.
    candidate.insert(iface.clone(), cfg.clone());
    if let Err(response) = relational_refusal(&candidate, &path) {
        return *response;
    }
    // `cfg` and not `candidate`: the one entry the request carries. A stored
    // entry that already holds an unparseable address is not this request's
    // fault and must not make an edit to a different interface fail.
    if let Err(response) = address_refusal(&iface, &cfg) {
        return *response;
    }
    // The entry's own dot-path and not the whole map, so a concurrent edit of
    // a different interface is not lost. Infallible: `IfaceSettings` is a
    // struct of scalars, strings and vectors with no map keys to collide.
    let value = serde_json::to_value(&cfg).expect("interface settings serialize");
    if let Err(err) = state.api.set_settings(&path, &value).await {
        return bus_api_error(&err, Some(&path));
    }
    no_content()
}

// The whole map is rewritten because the dot-path syntax has no delete: the
// only way to say "this key is gone" is to send the map without it.
/// Remove one interface, re-validating the rest of the map without it.
///
/// Removing an interface another entry depends on — a port still listed by a
/// bridge — is **422** with the failing rule's own message, and nothing is
/// written. Remove the dependent reference first.
///
/// Read-modify-write: two concurrent removals lose one.
#[utoipa::path(
    delete,
    path = V1_NETWORK_IFACE_ROUTE,
    context_path = API,
    tag = "resources",
    params(("iface" = String, Path, description = "The declared interface to remove")),
    responses(
        (status = 204, description = "The entry was removed; the reconciler has swept its units"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No `network` entry has that name (`settings_not_found`). Well-formed and absent, which is a different answer from malformed", body = ApiError),
        (status = 422, description = "The name is not an interface name, or removing the entry breaks a relational rule -- a bridge still lists it as a port, a VLAN still names it as a parent (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored map holds an entry this build cannot read (`settings_invalid`), or mosd failed (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_network_iface_remove(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(iface): Path<String>,
) -> Response {
    let path = iface_settings_path(&iface);
    if let Err(response) = check_iface_name(&iface, &path) {
        return *response;
    }
    let mut candidate = match api_network_entries(&state).await {
        Ok(entries) => entries,
        Err(response) => return *response,
    };
    if candidate.remove(&iface).is_none() {
        return item_not_found(NETWORK_SETTINGS_PATH, &iface);
    }
    if let Err(response) = relational_refusal(&candidate, &path) {
        return *response;
    }
    if let Err(response) = write_network_map(&state, &candidate).await {
        return *response;
    }
    no_content()
}

/// A tunnel's stored peers, or the envelope for whatever refuses the interface.
///
/// The three answers this cluster gives about an `{iface}` that is not a
/// usable tunnel, and none of them is interchangeable with another:
///
/// - **404** when no `network` entry has that name. This is what the pane gets
///   wrong: `POST /network/peers/add` on an undeclared interface *succeeds* there and
///   writes an entry of the default kind carrying a WireGuard block, because
///   the pane's `stored_peers` answers an empty list rather than an error and
///   the settings setter creates missing intermediates by documented contract.
///   `the_pane_peer_add_writes_a_broken_entry_for_an_undeclared_interface`
///   runs that and confirms it. Here the read is the guard, and it happens
///   before anything is written.
/// - **422** when the entry exists and is not of kind `wireguard`. The same
///   split mosd's rotate-key now makes: a well-formed identifier naming a real
///   entry of the wrong kind is a bad argument and not an absent resource.
/// - **422** when the name is not an interface name at all, from
///   [`check_iface_name`].
async fn tunnel_peers(
    state: &AppState,
    iface: &str,
    path: &str,
) -> Result<Vec<WireguardPeer>, Box<Response>> {
    check_iface_name(iface, path)?;
    let entries = api_network_entries(state).await?;
    let Some(cfg) = entries.get(iface) else {
        return Err(Box::new(item_not_found(NETWORK_SETTINGS_PATH, iface)));
    };
    if cfg.kind != IfaceKind::Wireguard {
        return Err(Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                format!(
                    "network.{iface} is not a WireGuard interface: only an entry of kind `wireguard` has peers"
                ),
            )
            .at(path),
        )));
    }
    Ok(cfg
        .wireguard
        .as_ref()
        .map(|wireguard| wireguard.peers.clone())
        .unwrap_or_default())
}

/// Validate and write a rewritten peer list, in section 2.4's envelope.
///
/// The API's own writer and not [`write_peers`], which answers a re-rendered
/// pane at 422 and a redirect on success. The **validator** is the same one:
/// [`validate_peers`] echoes the rules `validate_wireguard` runs in the
/// reconciler, and it names a rejected peer by its index and never by its key,
/// for the reason the reconciler states -- an operator who pasted a *private*
/// key into the field would otherwise find it in the error text, and here that
/// text goes into an HTTP body.
async fn api_write_peers(
    state: &AppState,
    iface: &str,
    peers: &[WireguardPeer],
) -> Result<(), Box<Response>> {
    let path = peers_settings_path(iface);
    if let Err(message) = validate_peers(iface, peers) {
        return Err(Box::new(api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", message).at(&path),
        )));
    }
    // Infallible: a peer is a struct of strings and integers.
    let value = serde_json::to_value(peers).expect("wireguard peers serialize");
    if let Err(err) = state.api.set_settings(&path, &value).await {
        return Err(Box::new(bus_api_error(&err, Some(&path))));
    }
    Ok(())
}

/// List every peer configured on one WireGuard tunnel.
///
/// Answers **404** when `iface` names no declared interface.
#[utoipa::path(
    get,
    path = V1_NETWORK_PEERS_ROUTE,
    context_path = API,
    tag = "resources",
    params(("iface" = String, Path, description = "A declared `network` entry of kind `wireguard`")),
    responses(
        (status = 200, description = "The stored peers, in stored order, each with the public key that is its `DELETE` path segment", body = Vec<WireguardPeerEntry>),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 404, description = "No `network` entry has that name (`settings_not_found`)", body = ApiError),
        (status = 422, description = "The name is not an interface name, or the entry is not a WireGuard one (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored map holds an entry this build cannot read (`settings_invalid`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_peers_list(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(iface): Path<String>,
) -> Response {
    let path = peers_settings_path(&iface);
    match tunnel_peers(&state, &iface, &path).await {
        Ok(peers) => {
            // Through the shared redactor, as the WiFi listing is. No field of
            // a peer is on `redact`'s list today, so this substitutes nothing;
            // it is the fail-closed half of that rule, so the day a peer
            // pre-shared key enters the schema it is already covered.
            // Infallible: a peer is a struct of strings and integers.
            let value = serde_json::to_value(&peers).expect("wireguard peers serialize");
            api_response(StatusCode::OK, redact::redact(value, &path))
        }
        Err(response) => *response,
    }
}

// 409 and not 422 for a duplicate: the public key IS this collection's
// identity — it is the DELETE path segment — so a second entry under one key
// would leave no answer to which of the two a DELETE names.
/// Add one peer to a WireGuard tunnel.
///
/// Answers **404** when `iface` names no declared interface, checked before
/// anything is written. A duplicate public key is **409** with code
/// `peer_exists`; a malformed peer is **422**.
#[utoipa::path(
    post,
    path = V1_NETWORK_PEERS_ROUTE,
    context_path = API,
    tag = "resources",
    params(("iface" = String, Path, description = "A declared `network` entry of kind `wireguard`")),
    request_body = WireguardPeerEntry,
    responses(
        (status = 201, description = "The peer was added; the body carries it back", body = WireguardPeerEntry),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No `network` entry has that name (`settings_not_found`); nothing is written", body = ApiError),
        (status = 409, description = "A stored peer already carries that public key (`peer_exists`); the key is this collection's identity, so the entry is not replaced silently", body = ApiError),
        (status = 422, description = "The name is not an interface name, the entry is not a WireGuard one, or the peer is one the reconciler would refuse -- a public key that is not 32 bytes of base64, an allowed IP that is not a CIDR, an endpoint that is not `host:port` (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored map holds an entry this build cannot read (`settings_invalid`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_peers_add(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path(iface): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let path = peers_settings_path(&iface);
    let peer: WireguardPeer = match json_body(body, Some(&path)) {
        Ok(peer) => peer,
        Err(response) => return *response,
    };
    let mut peers = match tunnel_peers(&state, &iface, &path).await {
        Ok(peers) => peers,
        Err(response) => return *response,
    };
    if peers
        .iter()
        .any(|stored| stored.public_key == peer.public_key)
    {
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "peer_exists",
                // The key is echoed, unlike a validation refusal's message. It
                // is a public key by definition and the caller just sent it;
                // what the reconciler's index-only rule protects against is a
                // *rejected* value, which may be a private key pasted into the
                // wrong field, and a duplicate matched a value already stored.
                format!(
                    "a peer of network.{iface} already carries the public key `{}`; remove it before adding another under that key",
                    peer.public_key
                ),
            )
            .at(&path),
        );
    }
    // Serialized before the move into the list, so the echo is the entry as it
    // was stored and not the body as it arrived.
    let echoed = serde_json::to_value(&peer).expect("a wireguard peer serializes");
    peers.push(peer);
    if let Err(response) = api_write_peers(&state, &iface, &peers).await {
        return *response;
    }
    api_response(StatusCode::CREATED, redact::redact(echoed, &path))
}

// The HTML pane answers 422 where this answers 404, deliberately: a form's
// body is a re-rendered page no consumer reads a status from.
/// Remove one peer from a tunnel, identified by its public key.
///
/// A string that is not 32 bytes of base64 could never be a WireGuard public
/// key and is **422**. A well-formed key that no stored peer carries is
/// **404**.
#[utoipa::path(
    delete,
    path = V1_NETWORK_PEER_ROUTE,
    context_path = API,
    tag = "resources",
    params(
        ("iface" = String, Path, description = "A declared `network` entry of kind `wireguard`"),
        ("publicKey" = String, Path, description = "The peer's public key, as `GET /api/v1/network/{iface}/peers` returns it: 32 bytes in padded base64. Its alphabet contains `/` and `+`, so a key carrying either is percent-encoded"),
    ),
    responses(
        (status = 204, description = "The peer was removed; the reconciler has re-rendered the tunnel without it"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 404, description = "No `network` entry has that name, or no peer of it carries that key (`settings_not_found`)", body = ApiError),
        (status = 422, description = "The interface name is not one, the entry is not a WireGuard one, or the path segment is not a WireGuard public key at all (`validation_failed`)", body = ApiError),
        (status = 500, description = "The stored map holds an entry this build cannot read (`settings_invalid`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_peers_remove(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Path((iface, public_key)): Path<(String, String)>,
) -> Response {
    let path = peers_settings_path(&iface);
    // Before the read, unlike the interface's own absence: a segment that
    // could never be a public key would send the caller looking for a peer
    // they deleted instead of at the URL they typed.
    if !is_wireguard_key(&public_key) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                "a WireGuard public key is 32 bytes spelled in base64".to_string(),
            )
            .at(&path),
        );
    }
    let mut peers = match tunnel_peers(&state, &iface, &path).await {
        Ok(peers) => peers,
        Err(response) => return *response,
    };
    let Some(index) = peers.iter().position(|peer| peer.public_key == public_key) else {
        return item_not_found(&path, &public_key);
    };
    peers.remove(index);
    if let Err(response) = api_write_peers(&state, &iface, &peers).await {
        return *response;
    }
    no_content()
}

/// One answer shape for both roots: the value redacted, or §2.4's envelope
/// classified from what mosd said.
fn resource_response(value: anyhow::Result<Value>, path: &str) -> Response {
    match value {
        Ok(value) => api_response(StatusCode::OK, ResourceValue(redact::redact(value, path))),
        Err(err) => bus_api_error(&err, Some(path)),
    }
}

/// §2.4's table, applied to a failed mosd call.
///
/// The classification is translated and the message is not. mosd maps its
/// `SettingsError` onto five error names — two interface-scoped, three fdo —
/// and zbus carries the name back, so the distinction exists all the way to
/// here and only apid can lose it; the message is mosd's own words because no
/// phrasing apid could pre-write would say which field was wrong.
///
/// The concrete `zbus::Error` is recovered by downcast: `bus_client.rs`
/// converts with `err.into()`, and that conversion stores the error rather
/// than flattening it, so the name is readable here.
///
/// `path` is an `Option` because §2.4 makes the member optional — *"present
/// only when the failure names a dot-path"* — and M7's actions name none: a
/// power verb and a transient root password write no setting at all, so there
/// is no dot-path at fault to report. Every route that does name one passes
/// `Some`, and the classification above is shared rather than copied.
pub(crate) fn bus_api_error(err: &anyhow::Error, path: Option<&str>) -> Response {
    tracing::warn!(error = %err, path = path.unwrap_or_default(), "mosd call failed");
    let (status, error) = if err.downcast_ref::<InvalidTaskPayload>().is_some() {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::mosd("mosd_failed", format!("{err:#}")),
        )
    } else if err
        .downcast_ref::<crate::bus_client::MosdCallTimeout>()
        .is_some()
    {
        (
            StatusCode::GATEWAY_TIMEOUT,
            ApiError::apid("mosd_timeout", format!("{err:#}")),
        )
    } else {
        match err.downcast_ref::<zbus::Error>() {
            Some(zbus::Error::MethodError(name, message, _)) => {
                // An fdo error with no message is still a classification; the name
                // is the most specific thing left to say.
                let message = message.clone().unwrap_or_else(|| name.to_string());
                match name.as_str() {
                    MOSD_NOT_FOUND => (
                        StatusCode::NOT_FOUND,
                        ApiError::mosd("settings_not_found", message),
                    ),
                    MOSD_READ_ONLY => (
                        StatusCode::CONFLICT,
                        ApiError::mosd("settings_read_only", message),
                    ),
                    FDO_INVALID_ARGS => (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        ApiError::mosd("settings_rejected", message),
                    ),
                    FDO_IO_ERROR => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ApiError::mosd("settings_io", message),
                    ),
                    FDO_FAILED => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ApiError::mosd("mosd_failed", message),
                    ),
                    _ => mosd_unreachable(err),
                }
            }
            _ => mosd_unreachable(err),
        }
    };
    // Omitted rather than nulled or emptied when there is none: the member
    // carries `skip_serializing_if`, so an action's envelope simply has no
    // `path` key.
    let error = match path {
        Some(path) => error.at(path),
        None => error,
    };
    let mut response = api_response(status, error);
    // §2.4 gives `Retry-After` to exactly one class, and 503 is that class:
    // apid is up and answering, and the proxy cache is dropped after a failed
    // call so the next request reconnects.
    if status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECONDS));
    }
    response
}

/// Task lookup has the same transport classifications as other mosd calls,
/// but its missing item is not a missing settings path.
fn task_api_error(err: &anyhow::Error, id: &str) -> Response {
    if is_task_not_found(err) {
        return api_response(
            StatusCode::NOT_FOUND,
            ApiError::mosd("task_not_found", format!("task not found: `{id}`")),
        );
    }
    bus_api_error(err, None)
}

fn is_task_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::settings_api::TaskNotFound>()
        .is_some()
        || matches!(
            err.downcast_ref::<zbus::Error>(),
            Some(zbus::Error::MethodError(name, _, _)) if name.as_str() == MOSD_NOT_FOUND
        )
}

/// §2.4's last row, which is exhaustive over everything the three above do not
/// name: the call could not be made at all. `source` is apid because this is a
/// statement about this server rather than about the request.
fn mosd_unreachable(err: &anyhow::Error) -> (StatusCode, ApiError) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        ApiError::apid("mosd_unreachable", format!("{err:#}")),
    )
}

/// Authentication accepted by management API handlers.
///
/// Automation uses a stored bearer token. The built-in and custom SPAs use a
/// signed browser session; state-changing requests made with that session must
/// also present its `X-CSRF-Token` value. Keeping the check in the extractor
/// makes it impossible for a newly added authenticated mutation to forget the
/// browser-side protection.
///
/// The forced rotation that bounds a bootstrap claim is enforced here for the
/// same reason and by the same argument: it applies to every authenticated
/// mutation the API serves, so it belongs in the one place every authenticated
/// mutation already passes through, rather than in a list of routes that a
/// later one can be added outside of.
pub(crate) enum ApiCredential {
    Bearer,
    Session(String),
}

impl FromRequestParts<AppState> for ApiCredential {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let mutation = matches!(
            parts.method,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        );
        let credential = if bearer_is_stored(state, &parts.headers).await {
            Self::Bearer
        } else if let Some(cookie) = session::cookie_from_headers(&parts.headers)
            && state.sessions.verify(&cookie)
        {
            if mutation {
                let presented = parts
                    .headers
                    .get("x-csrf-token")
                    .and_then(|value| value.to_str().ok());
                if !presented.is_some_and(|token| state.sessions.verify_csrf(&cookie, token)) {
                    return Err(api_response(
                        StatusCode::FORBIDDEN,
                        ApiError::apid(
                            "csrf_invalid",
                            "a browser session mutation requires its X-CSRF-Token value"
                                .to_string(),
                        ),
                    ));
                }
            }
            Self::Session(cookie)
        } else {
            return Err(not_authenticated(
                "this route requires a stored bearer token or an authenticated browser session",
            ));
        };
        if mutation && bound_by_rotation(parts.uri.path()) && rotation_required(state).await {
            // Reads stay open, and so do the two mutations that write nothing
            // to the DEVICE. Those exemptions are what make the bound
            // impossible to brick a device with: the operator holding the
            // bootstrap credential can always sign in and out, can always see
            // WHY they were refused (`GET /api/v1/claim`), and can always do
            // the one thing that clears it. Everything else waits.
            //
            // The rejection is built here rather than by the handler because
            // the handler is never reached; a refused mutation writes nothing
            // by construction and not by each route remembering to.
            let Source(source) = Source::from_request_parts(parts, state)
                .await
                .expect("the source extractor is infallible");
            state.audit.record(CLAIM_ROTATION_EVENT, "refused", &source);
            return Err(api_response(
                StatusCode::CONFLICT,
                ApiError::apid(
                    "rotation_required",
                    "this device was claimed with a bootstrap credential that has not been \
                     rotated; change the administrator password with \
                     `POST /api/v1/actions/change-password` before any other write"
                        .to_string(),
                )
                .at("access.claim"),
            ));
        }
        Ok(credential)
    }
}

/// §2.4's 401, in whichever wording the rejecting extractor owes.
fn not_authenticated(message: &str) -> Response {
    api_response(
        StatusCode::UNAUTHORIZED,
        ApiError::apid("not_authenticated", message.to_string()),
    )
}

/// Whether the request carries a bearer token this device stores (§3.2).
///
/// **Not rate limited, and it must not become so.** The secret is 256 bits of
/// `OsRng` and is not guessable online, while a shared counter here would let
/// anyone holding a bad token lock out every script on the appliance. The login
/// backoff ([`auth::GuardStore`]) stays scoped to the password path, which is
/// where a human-chosen secret is.
async fn bearer_is_stored(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(presented) = token::bearer_from_headers(headers) else {
        return false;
    };
    // The subtree the gate already reads, which is why §3.2 put the list under
    // `access` rather than beside it: no second round trip per request.
    let access = match access_settings(state).await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "reading `access` for a bearer check failed");
            return false;
        }
    };
    // A stored list that does not parse authenticates nobody, which errs
    // closed -- the opposite of the read the token routes do, where the same
    // condition is an error rather than an empty list because a write follows.
    token::verify(&parse_tokens(&access).unwrap_or_default(), presented)
}

/// The `access` subtree: from the gate's cache when it is provably fresh, and
/// from mosd otherwise.
///
/// The generation is snapshotted BEFORE the direct read so a change signalled
/// while the read was in flight discards the fill rather than caching a
/// possibly-pre-change snapshot.
async fn access_settings(state: &AppState) -> anyhow::Result<Value> {
    if let Some(value) = state.access_cache.get() {
        return Ok(value);
    }
    let generation = state.access_cache.generation();
    let value = state.api.get_settings("access").await?;
    state.access_cache.fill(generation, value.clone());
    Ok(value)
}

/// Redirect-only router served on the HTTP listener: 308 every request to
/// the HTTPS origin derived from the `Host` header.
pub fn redirect_app(https_port: u16) -> Router {
    Router::new()
        .fallback(redirect_to_https)
        .with_state(https_port)
}

async fn redirect_to_https(State(https_port): State<u16>, request: Request) -> Response {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    let path = request.uri().path_and_query().map_or("/", |pq| pq.as_str());
    let target = if https_port == 443 {
        format!("https://{host}{path}")
    } else {
        format!("https://{host}:{https_port}{path}")
    };
    (StatusCode::PERMANENT_REDIRECT, [(LOCATION, target)]).into_response()
}

/// Whether `access` (the settings subtree) carries an admin password hash.
fn password_hash(access: &Value) -> Option<&str> {
    access
        .get("webAdmin")
        .and_then(|admin| admin.get("password_hash"))
        .and_then(Value::as_str)
}

/// Listener health used by the boot readiness gate.
async fn healthz() -> &'static str {
    "ok"
}

// Validation

const HOSTNAME_RULES: &str =
    "Hostname must be 1-63 letters, digits or hyphens and must not start or end with a hyphen.";

/// Whether an admin password is under [`MIN_ADMIN_PASSWORD_LEN`].
///
/// Bytes and not characters, which is what every call site already measured:
/// `str::len` is the byte length, so a password of eight non-ASCII characters
/// was already over the floor before this function existed. Named rather than
/// inlined so the comparison, and not only the number, has one spelling.
///
/// The number itself is [`mosd_settings::MIN_ADMIN_PASSWORD_LEN`] and no
/// longer a copy of it. apid used to state its own `MIN_PASSWORD_BYTES = 8`
/// beside mosd's, with a comment in each naming the other; two statements of
/// one rule agree with each other right up until one of them moves, and the
/// thing they would disagree about is whether a credential this device
/// accepts is one it will keep accepting. `mosd-settings` is the crate both
/// binaries link, so it is where the bound lives.
fn password_under_floor(password: &str) -> bool {
    password.len() < MIN_ADMIN_PASSWORD_LEN
}

/// `^[a-zA-Z0-9._-]{1,15}$`
fn valid_iface_name(name: &str) -> bool {
    (1..=15).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn valid_ipv4(address: &str) -> bool {
    let octets: Vec<&str> = address.split('.').collect();
    octets.len() == 4
        && octets.iter().all(|octet| {
            !octet.is_empty()
                && octet.len() <= 3
                && octet.bytes().all(|b| b.is_ascii_digit())
                && octet.parse::<u16>().is_ok_and(|value| value <= 255)
        })
}

/// Minimal `a.b.c.d/len` shape with in-range octets and prefix length.
fn valid_cidr(cidr: &str) -> bool {
    let Some((address, prefix)) = cidr.split_once('/') else {
        return false;
    };
    valid_ipv4(address)
        && !prefix.is_empty()
        && prefix.len() <= 2
        && prefix.bytes().all(|b| b.is_ascii_digit())
        && prefix.parse::<u8>().is_ok_and(|value| value <= 32)
}

/// `^[a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?$`
fn valid_hostname(name: &str) -> bool {
    let bytes = name.as_bytes();
    matches!(bytes.len(), 1..=63)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

/// Interface form validation shared by `/network` and the setup wizard.
///
/// DHCP off with an empty address is an interface with **no** addressing, not
/// an error. It used to be one, and it stopped being one when bridges became
/// expressible: a bridge port *must* carry neither `dhcp` nor `static`
/// (`validate_network` in `pkgs/mosd/mosd/src/reconciler/network.rs`), and it must be a
/// declared entry before a bridge may name it, so a pane that insisted on an
/// address made a bridge unbuildable through the form. An address that is
/// present and not a CIDR is still refused.
fn validate_iface(iface: &str, dhcp: bool, address: &str) -> Result<(), &'static str> {
    if !valid_iface_name(iface) {
        return Err("Interface name must be 1-15 characters of letters, digits, '.', '_' or '-'.");
    }
    validate_static_address(dhcp, address)
}

/// The address clause of [`validate_iface`], as its own function.
///
/// It is factored out rather than copied because M6's typed network routes
/// have to run this rule too and must not be able to disagree with the forms
/// about what an address is: `PUT /api/v1/network/{iface}` took an address the
/// kernel cannot parse and answered 204 until this was factored out. Those
/// routes cannot call `validate_iface`
/// itself, because they check the name with [`check_iface_name`] and would
/// otherwise carry two spellings of the name refusal.
///
/// The condition is unchanged and is not widened: DHCP off with an empty
/// address is an interface with no addressing, for the reason
/// [`validate_iface`] states.
fn validate_static_address(dhcp: bool, address: &str) -> Result<(), &'static str> {
    if !dhcp && !address.is_empty() && !valid_cidr(address) {
        return Err("Static address must be IPv4 CIDR notation, e.g. 192.168.1.10/24.");
    }
    Ok(())
}

/// The settings path of `iface`'s entry, with the name quoted when it carries
/// a dot: a VLAN named `eth0.100` is `network."eth0.100"`, not three segments.
fn iface_settings_path(iface: &str) -> String {
    format!("network.{}", quote_path_segment(iface))
}

/// True when `value` parses as an IP address with an optional `/prefix`.
///
/// An echo of the reconciler's `is_ip_or_cidr`
/// (`is_ip_or_cidr` in `pkgs/mosd/mosd/src/reconciler/network.rs`), for the reason
/// `validate_static` states about the address field: apid checks on its write
/// path so the operator gets a readable error, and the reconciler checks again
/// because the settings file is writable without apid. Deliberately not
/// [`valid_cidr`], which is IPv4-only and belongs to the older address field.
fn is_ip_or_cidr(value: &str) -> bool {
    let (addr, prefix) = match value.split_once('/') {
        Some((addr, prefix)) => (addr, Some(prefix)),
        None => (value, None),
    };
    let Ok(addr) = addr.parse::<IpAddr>() else {
        return false;
    };
    match prefix {
        None => true,
        Some(prefix) => prefix
            .parse::<u8>()
            .is_ok_and(|p| p <= if addr.is_ipv4() { 32 } else { 128 }),
    }
}

/// True when `value` is the `host:port` a peer's `endpoint` has to be.
///
/// The same echo, of `is_host_port`
/// (`is_host_port` in `pkgs/mosd/mosd/src/reconciler/network.rs`).
fn is_host_port(value: &str) -> bool {
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    if port.parse::<u16>().is_err() {
        return false;
    }
    if let Some(inner) = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        return inner.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !host.is_empty()
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Length of the base64 spelling of a 32-byte key, padding included.
const WIREGUARD_KEY_LEN: usize = 44;

/// Whether `value` is the base64 X25519 key a peer's `publicKey` has to be.
///
/// The echo of `wgkeys::is_key` (`pkgs/mosd/mosd/src/wgkeys.rs`),
/// which decodes with the standard alphabet's *padded* spelling; the length
/// test is what pins that, because [`mosd_settings::decode_base64`] also
/// accepts the unpadded form and an echo that accepted more than the boundary
/// would hand the operator a form error from the daemon instead of from the
/// field.
fn is_wireguard_key(value: &str) -> bool {
    value.len() == WIREGUARD_KEY_LEN
        && mosd_settings::decode_base64(value).is_some_and(|bytes| bytes.len() == 32)
}

/// The reconciler's peer rules, echoed for a readable form error.
///
/// A rejected peer is named by its index and never by its key, for the reason
/// the reconciler states: an operator who pasted a *private* key into the field
/// would otherwise find it in the error text.
fn validate_peers(iface: &str, peers: &[WireguardPeer]) -> Result<(), String> {
    for (index, peer) in peers.iter().enumerate() {
        if !is_wireguard_key(&peer.public_key) {
            return Err(format!(
                "network.{iface} peer {index} has a public key that is not a WireGuard key: it must be 32 bytes spelled in base64."
            ));
        }
        for allowed in &peer.allowed_ips {
            if !is_ip_or_cidr(allowed) {
                return Err(format!(
                    "network.{iface} peer {index} allowed IP {allowed:?} is not an IP address or CIDR."
                ));
            }
        }
        if let Some(endpoint) = &peer.endpoint
            && !is_host_port(endpoint)
        {
            return Err(format!(
                "network.{iface} peer {index} endpoint {endpoint:?} is not host:port."
            ));
        }
    }
    Ok(())
}

/// The reconciler's relational rules, echoed over the whole candidate subtree.
///
/// Echoed and not forked. `validate_network`
/// (`validate_network` in `pkgs/mosd/mosd/src/reconciler/network.rs`) stays the boundary
/// — it runs on every apply, including the ones that never went through apid —
/// and this runs first so the operator reads which field is wrong instead of a
/// 502 from a failed bus call.
///
/// It is checked over the *candidate* tree rather than over the one entry being
/// written, because every rule here is about two entries at once: a VLAN and
/// its parent, a bridge and its ports. Editing `eth1` to take an address is
/// refused when `br0` claims it, which no check confined to `eth1` could see.
fn validate_entries(entries: &NetworkEntries) -> Result<(), String> {
    for (iface, cfg) in entries {
        if let Some(wireguard) = &cfg.wireguard {
            validate_peers(iface, &wireguard.peers)?;
        }
    }
    // Which bridge claimed each port, so a second claim on one port is an
    // error rather than a race between two `Bridge=` lines for one file.
    let mut claimed_by: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for (iface, cfg) in entries {
        if let Some(vlan) = &cfg.vlan
            && !entries.contains_key(&vlan.parent)
        {
            return Err(format!(
                "network.{iface} has VLAN parent {:?}, which is not a declared network entry.",
                vlan.parent
            ));
        }
        let Some(bridge) = &cfg.bridge else {
            continue;
        };
        for port in &bridge.ports {
            let Some(port_cfg) = entries.get(port) else {
                return Err(format!(
                    "network.{iface} has bridge port {port:?}, which is not a declared network entry."
                ));
            };
            if port_cfg.dhcp || port_cfg.static_.is_some() {
                return Err(format!(
                    "network.{port} is a port of bridge {iface} and must not carry addressing of its own."
                ));
            }
            if let Some(other) = claimed_by.insert(port, iface) {
                return Err(format!(
                    "network.{port} is claimed as a port by both bridge {other} and bridge {iface}."
                ));
            }
        }
    }
    Ok(())
}

// Setup wizard

/// `POST /api/v1/setup` request body.
///
/// No `confirm` member, unlike the form the wizard posts: that field exists so
/// a human who mistyped a password into a box they cannot read is told before
/// it becomes the only credential on the device. A client that built a JSON
/// body knows what it sent, and a second copy of the same string proves
/// nothing about it.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct SetupRequest {
    /// The admin password to set. At least 8 bytes, the floor the wizard and
    /// the change-password route enforce.
    password: String,
    /// The hostname to apply. Absent leaves the stored one alone.
    ///
    /// Not trimmed, for the reason `PUT /api/v1/settings/hostname` gives: a
    /// client that built a JSON string chose its bytes, and silently writing
    /// something other than what it sent is the worse answer.
    #[serde(default)]
    hostname: Option<String>,
    // Merged and not replacing: a route that replaced the map could unmake the
    // entry a factory-fresh device is reachable over.
    /// `network` entries to declare, merged into the stored map by name.
    ///
    /// An entry whose name is already declared is replaced whole; one that is
    /// not is added. Entries the body does not name are left alone.
    #[serde(default)]
    #[schema(value_type = Option<std::collections::BTreeMap<String, NetworkInterface>>)]
    network: Option<NetworkEntries>,
}

/// `POST /api/v1/setup` response body.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetupToken {
    /// The whole token, `mos_<id>_<secret>`.
    ///
    /// **It appears here and nowhere else, ever**, exactly as the mint route's
    /// does: only the SHA-256 digest is stored. The id is not a separate
    /// member because it is the token's own second segment, so a caller that
    /// holds this string can address it for a later `DELETE` without being
    /// told it twice.
    token: String,
    /// CSRF token for the browser session created by setup.
    csrf_token: String,
}

/// The label the setup-minted token is listed under.
///
/// A fixed string and not a request field: the body has no name member, and
/// inventing one would make the first credential's label the one thing about
/// first-run setup a client must get right. It says where the token came from,
/// which is the only thing a listing can usefully say about it.
const SETUP_TOKEN_NAME: &str = "first-run setup";

// The write order fails safe: nothing before `access.webAdmin` takes the
// device out of setup mode, so a failure at any point leaves the wizard
// reachable. The browser wizard writes in a different order and is not changed
// here; closing that gap needs a transactional multi-path write on the bus.
/// First-run setup: set the admin password, optionally a hostname and network
/// entries, and mint an API token.
///
/// **Unauthenticated**, because it creates the device's first credential.
/// Answers **409** once a password exists, which closes it permanently.
///
/// Everything is validated before anything is written; a rejected request
/// leaves the device untouched and still in setup mode. On success the writes
/// run in the order hostname, network, `access.webAdmin`, token.
///
/// Answers **200** with the minted token secret, returned once. Unlike the
/// browser wizard, this route always mints one: a caller driving setup over
/// the API wants API access.
#[utoipa::path(
    post,
    path = V1_SETUP_PATH,
    context_path = API,
    tag = "actions",
    request_body = SetupRequest,
    responses(
        (status = 201, description = "The device is configured; the body carries a newly minted API token, which is not recoverable afterwards", body = SetupToken),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 409, description = "The device already has an admin password, so it is not in setup mode (`already_configured`). Change the password with `POST /api/v1/actions/change-password`", body = ApiError),
        (status = 422, description = "The body is not this shape, the password is under 8 bytes, the hostname is not a hostname, a `network` key is not an interface name, a static address is not IPv4 CIDR notation, or a relational rule refuses the resulting map -- a VLAN parent or a bridge port that is not a declared entry, a bridge port carrying addressing, a port claimed twice (`validation_failed`); or mosd rejected a write (`settings_rejected`). Nothing is written on any of them", body = ApiError),
        (status = 500, description = "Hashing the password failed (`hash_failed`), the stored token list could not be read (`settings_invalid`), no free token id was drawn (`mint_failed`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_setup(
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // No dot-path: this route writes three subtrees and a malformed body is
    // not about any one of them (§2.4's optional member).
    let request: SetupRequest = match json_body(body, None) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    // One read of `access`, answering two questions: whether the device is
    // still in setup mode, and what the token list holds. The form path's
    // condition is `password_hash(&access).is_some()` and this is that
    // condition and not a rendering of it.
    let access = match state.api.get_settings("access").await {
        Ok(value) => value,
        Err(err) => return bus_api_error(&err, Some("access")),
    };
    if password_hash(&access).is_some() {
        // A refused claim is audited, and it is the more interesting record of
        // the two: a claim can only ever succeed once, so every later attempt
        // is either an operator who lost track of a device or somebody probing
        // one. §6's line carries the peer address, which is the whole of what
        // makes the record useful.
        state.audit.record(CLAIM_EVENT, "refused", &source);
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "already_configured",
                "this device already has an admin password, so it is not in setup mode; \
                 change the password with `POST /api/v1/actions/change-password`"
                    .to_string(),
            )
            .at("access.webAdmin"),
        );
    }

    // Everything below this line validates. Nothing below it writes until the
    // last rule has passed and the password has been hashed. The divergence
    // from the wizard that this creates is asserted by
    // `the_api_setup_route_validates_before_writing_where_the_form_path_does_not`,
    // which drives the same invalid request through both surfaces.
    if password_under_floor(&request.password) {
        // The bound and never the password: the message names how long it must
        // be and interpolates nothing the caller sent.
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                format!("the admin password must be at least {MIN_ADMIN_PASSWORD_LEN} bytes"),
            )
            .at("access.webAdmin"),
        );
    }
    if let Some(hostname) = request.hostname.as_deref()
        && !valid_hostname(hostname)
    {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", HOSTNAME_RULES.to_string()).at("hostname"),
        );
    }
    // The candidate tree and not the submitted entries, for the reason the
    // pane's comment gives and M6's routes act on: every relational rule is
    // about two entries at once, so a submitted bridge port may legitimately
    // name an interface the device already declares. The validators are M6's
    // own, called and not copied.
    let candidate = match request.network.as_ref() {
        None => None,
        Some(submitted) => {
            for (iface, cfg) in submitted {
                if let Err(response) = check_iface_name(iface, NETWORK_SETTINGS_PATH) {
                    return *response;
                }
                // The rest of the wizard's entry validator, called and not
                // copied. Its name branch cannot fire here -- `check_iface_name`
                // above tests the same predicate -- so the only message it can
                // produce is the CIDR one, which is a rule that lives in a
                // function whose only two callers are HTML form handlers and is
                // therefore run by no route under `/api/v1/`. Called here
                // because a factory-fresh device configured with an address the
                // kernel cannot parse is exactly the unreachable box this
                // milestone exists to prevent. That M6's network routes do not
                // call it is a finding on its own, not something this route may
                // fix on their behalf.
                let address = cfg
                    .static_
                    .as_ref()
                    .map_or("", |static_| static_.address.as_str());
                if let Err(message) = validate_iface(iface, cfg.dhcp, address) {
                    return api_response(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        ApiError::apid("validation_failed", message.to_string())
                            .at(NETWORK_SETTINGS_PATH),
                    );
                }
            }
            let mut candidate = match api_network_entries(&state).await {
                Ok(entries) => entries,
                Err(response) => return *response,
            };
            for (iface, cfg) in submitted {
                candidate.insert(iface.clone(), cfg.clone());
            }
            if let Err(response) = relational_refusal(&candidate, NETWORK_SETTINGS_PATH) {
                return *response;
            }
            Some(candidate)
        }
    };
    // Read from the subtree already in hand rather than through
    // `stored_tokens`, which would call the bus a second time for the same
    // value. The posture is that helper's: a list that is present and
    // unreadable is an error and never an empty list.
    let tokens = match parse_tokens(&access) {
        Ok(tokens) => tokens,
        Err(err) => {
            return api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid(
                    "settings_invalid",
                    format!("the stored token list could not be read: {err}"),
                )
                .at(API_TOKENS_PATH),
            );
        }
    };
    let Some(minted) = token::mint(&tokens) else {
        return api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid(
                "mint_failed",
                "no free token id was drawn; nothing was written".to_string(),
            )
            .at(API_TOKENS_PATH),
        );
    };
    // Off the async workers for the same reason login verification and the
    // wizard's own hash are: argon2id costs real CPU per call, by design. It
    // is done here, before the first write, because it is the last step that
    // can fail without the caller having asked for anything impossible.
    let password = request.password.clone();
    let hash = match tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .unwrap_or_else(|err| Err(anyhow::anyhow!("password hashing task: {err}")))
    {
        Ok(hash) => hash,
        Err(err) => {
            // Logged, not returned: the error carries argon2's own text and
            // the caller can do nothing with it.
            tracing::error!(error = %err, "password hashing failed");
            return api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid(
                    "hash_failed",
                    "the admin password could not be hashed; nothing was written".to_string(),
                )
                .at("access.webAdmin"),
            );
        }
    };

    // The writes. `hostname` and `network` come first and are their own calls:
    // neither takes the device out of the unclaimed state, so a failure in
    // either leaves an unclaimed, still-claimable device and the caller may
    // simply post the same body again.
    if let Some(hostname) = request.hostname.as_deref()
        && let Err(err) = state
            .api
            .set_settings("hostname", &Value::String(hostname.to_string()))
            .await
    {
        return bus_api_error(&err, Some("hostname"));
    }
    if let Some(candidate) = candidate.as_ref()
        && let Err(response) = write_network_map(&state, candidate).await
    {
        // The whole map in one call and not one call per entry, so N submitted
        // interfaces are still one write and not N chances to half-apply.
        return *response;
    }

    // The claim itself, as ONE write of the whole `access` subtree.
    //
    // The credential, the record of how the device was claimed and the minted
    // token are three keys of one subtree and they commit together. mosd turns
    // one `SetSettings` into one `Settings::set` and one `Store::save`, and
    // `Store::save` commits by rename -- which is
    // `docs/design/provisioning.md` §4.1.3's argument, held here by the same
    // construction. A power loss therefore leaves the device fully unclaimed
    // or fully claimed, and never a credential without its token or a token
    // without its record.
    //
    // It used to be two writes, `access.webAdmin` then `access.apiTokens`,
    // ordered so that the token write was the only one whose failure left a
    // configured device. That ordering was the best a two-write claim could
    // do; one write does not need it.
    //
    // The subtree read at the top of this handler is edited in place rather
    // than rebuilt, so every key this route has no opinion about -- `ssh`,
    // `console`, the first-boot `device` credential -- is written back exactly
    // as it was read.
    let mut tokens = tokens;
    tokens.push(ApiToken {
        id: minted.id,
        name: SETUP_TOKEN_NAME.to_string(),
        hash: minted.hash,
        created: device_clock_seconds(),
    });
    // The same validator mosd runs, so a list this route accepts is one the
    // store will accept too -- [`write_tokens`]'s check, which the whole-subtree
    // write does not go through.
    if let Err(err) = validate_api_tokens(&tokens) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", key_error_message(&err)).at(API_TOKENS_PATH),
        );
    }
    let claim = ClaimSettings {
        via: ClaimChannel::Setup,
        at: device_clock_seconds(),
        // A password the caller chose at this moment and posted over the
        // management API is not a bootstrap secret: it was never written onto
        // a medium and never left with a device (`docs/design/access.md` §4.4).
        rotation_required: false,
    };
    // A subtree that is not an object is a tree no `Settings` produced, so
    // there is nothing in it to preserve; an empty map is what this route
    // would have written into anyway.
    let mut subtree = match access {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    subtree.insert(
        "webAdmin".to_string(),
        serde_json::json!({ "password_hash": hash }),
    );
    // Infallible: both are structs of scalars with no map keys to collide.
    subtree.insert(
        "claim".to_string(),
        serde_json::to_value(claim).expect("the claim record serializes"),
    );
    subtree.insert(
        "apiTokens".to_string(),
        serde_json::to_value(&tokens).expect("api tokens serialize"),
    );
    if let Err(err) = state
        .api
        .set_settings(ACCESS_PATH, &Value::Object(subtree))
        .await
    {
        return bus_api_error(&err, Some(ACCESS_PATH));
    }
    // The device just left setup mode, and the gate must not keep believing
    // otherwise from a cached pre-write snapshot.
    state.access_cache.invalidate();
    // Recorded once the credential exists, which is the moment the device is
    // claimed. §6's trail says what happened to the device, not which surface
    // asked, so the event names the transition and not the route.
    state.audit.record(CLAIM_EVENT, "completed", &source);
    let session = state.sessions.create();
    (
        StatusCode::CREATED,
        [
            (
                CACHE_CONTROL,
                CacheClass::NoStore.header_value().to_string(),
            ),
            (SET_COOKIE, session::session_cookie(&session.cookie)),
        ],
        Json(SetupToken {
            token: minted.wire,
            csrf_token: session.csrf_token,
        }),
    )
        .into_response()
}

// The claim lifecycle

/// The §6 event name for the unclaimed → claimed transition.
///
/// Named for the transition rather than for the route, because both channels
/// that can cause it produce the same state and §6's trail says what happened
/// to the device. Its outcomes are `completed` and `refused`, which is the
/// grammar `login` and `custom-ui-upload` already use; the ROTATION that bounds
/// a bootstrap claim is a different action and carries its own name
/// ([`CLAIM_ROTATION_EVENT`]), the way `update-mark` and `update-rollback` are
/// two names rather than one with two outcomes.
const CLAIM_EVENT: &str = "claim";

/// The §6 event name for the rotation that discharges a bootstrap claim.
///
/// Outcomes: `completed` when the bootstrap credential is replaced, `refused`
/// when a mutation is turned away because it has not been.
const CLAIM_ROTATION_EVENT: &str = "claim-rotation";

/// Whether a mutation on `path` is one the forced rotation holds back.
///
/// Two exemptions, and they are the whole list. Both are mutations that write
/// nothing to the device:
///
/// - `POST /api/v1/actions/change-password` is the rotation itself, so holding
///   it back would make the bound unclearable;
/// - `/v1/session` is login and logout. A session lives in apid's memory and
///   is a fact about a browser, not about the appliance. An operator who
///   cannot sign in has no way to reach the exemption above, and one who
///   cannot sign OUT is being made to keep a live session by a rule that
///   exists to protect the credential behind it.
///
/// Stated as an exemption list rather than as an enforcement list on purpose:
/// a route added later is bound by default, which is the direction that fails
/// safe.
fn bound_by_rotation(path: &str) -> bool {
    path != V1_CHANGE_PASSWORD_PATH && path != V1_SESSION_PATH
}

/// The claim record stored under `access.claim`, when the subtree carries one
/// this build can read.
///
/// A record that does not parse is `None` and therefore reads as a claim by
/// provisioning document, which is the fail-safe direction: the consequence is
/// that a rotation is asked for, never that one is excused.
fn stored_claim(access: &Value) -> Option<ClaimSettings> {
    serde_json::from_value(access.get("claim")?.clone()).ok()
}

/// `GET /api/v1/claim` response body.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaimStatus {
    /// `unclaimed` or `claimed`.
    state: &'static str,
    /// Which channel minted the administrator credential: `setup` or
    /// `provisioning-document`. Absent while the device is unclaimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    via: Option<ClaimChannel>,
    /// The device clock's reading when the claim committed, seconds since the
    /// UNIX epoch. **A label, never a deadline** — the reading
    /// `access.apiTokens[].created` is. Absent while the device is unclaimed,
    /// and absent for a claim by document that predates any recorded import.
    #[serde(skip_serializing_if = "Option::is_none")]
    at: Option<u64>,
    /// Whether the claiming credential is still the bootstrap secret it
    /// arrived as. While this is true the device serves reads and refuses
    /// every authenticated mutation but the password change that clears it.
    rotation_required: bool,
}

/// The claim, projected from the settings tree.
///
/// **One truth, read two ways.** `access.webAdmin` is what makes a device
/// claimed and always was; this adds the part it cannot state. A claim through
/// `POST /api/v1/setup` writes `access.claim` in the same save as the
/// credential, so its record is read back verbatim. A claim by provisioning
/// document writes no record — mosd's importer is the one writer that does not,
/// deliberately — and is recognised by the absence:
///
/// - only two writers can create the FIRST `access.webAdmin` on a device that
///   has none, this route and the importer, because every other writer of that
///   path is authenticated and an unclaimed device has no credential to
///   authenticate with;
/// - the importer refuses to apply a document at all once `access.webAdmin`
///   exists (`docs/design/provisioning.md` §4.1.4), so if a document applied
///   AND a credential exists AND no record does, that document is what created
///   it.
///
/// The `provisioning` subtree is read only in that case, and it is exactly the
/// case whose mutations are about to be refused, so the ordinary claimed
/// device pays no second bus call. A `provisioning` read that FAILS answers
/// "not claimed by document": the failure direction here is open, for
/// `docs/design/access.md` §6's reason — the alternative to a wrong guess is
/// an appliance no operator can reach.
async fn claim_status(state: &AppState, access: &Value) -> ClaimStatus {
    if password_hash(access).is_none() {
        return ClaimStatus {
            state: "unclaimed",
            via: None,
            at: None,
            rotation_required: false,
        };
    }
    if let Some(claim) = stored_claim(access) {
        return ClaimStatus {
            state: "claimed",
            via: Some(claim.via),
            at: Some(claim.at),
            rotation_required: claim.rotation_required,
        };
    }
    let document = match state.api.get_settings("provisioning").await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "reading `provisioning` for the claim record failed");
            return ClaimStatus {
                state: "claimed",
                via: None,
                at: None,
                rotation_required: false,
            };
        }
    };
    let applied = document
        .get("document")
        .and_then(|record| record.get("appliedDigest"))
        .and_then(Value::as_str)
        .is_some();
    if !applied {
        // Claimed, with no record and no applied document. Nothing this
        // build writes produces that tree, so there is nothing to say about
        // the channel and nothing to demand a rotation of.
        return ClaimStatus {
            state: "claimed",
            via: None,
            at: None,
            rotation_required: false,
        };
    }
    ClaimStatus {
        state: "claimed",
        via: Some(ClaimChannel::ProvisioningDocument),
        // P1 already records WHEN, in the import record this reads; the claim
        // does not copy it into a second field that could disagree.
        at: document
            .get("document")
            .and_then(|record| record.get("lastImport"))
            .and_then(|import| import.get("at"))
            .and_then(Value::as_u64),
        rotation_required: true,
    }
}

/// Whether the credential authenticating this request is a bootstrap secret
/// that still has to be rotated.
async fn rotation_required(state: &AppState) -> bool {
    let access = match access_settings(state).await {
        Ok(value) => value,
        Err(err) => {
            tracing::warn!(error = %err, "reading `access` for the rotation gate failed");
            return false;
        }
    };
    claim_status(state, &access).await.rotation_required
}

/// Report how this device was claimed and whether its credential must still be
/// rotated.
///
/// **Authenticated, deliberately.** `GET /api/v1/session` already tells an
/// unauthenticated caller whether the device is in setup mode, and that is all
/// an unauthenticated caller learns here too — "this device was claimed from a
/// medium and is still holding the password that was on it" is a sentence an
/// attacker would act on, and the operator who needs to read it is signed in
/// by construction.
///
/// This is the surface that makes the bound observable BEFORE it bites: the
/// operator who claimed a device with a provisioning document sees
/// `rotationRequired` here, and can see it the moment they sign in rather than
/// on the first mutation the device turns away.
#[utoipa::path(
    get,
    path = V1_CLAIM_PATH,
    context_path = API,
    tag = "session",
    responses(
        (status = 200, description = "How the device was claimed and whether its credential must be rotated", body = ClaimStatus),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 500, description = "The access settings could not be read", body = ApiError),
        (status = 503, description = "mosd is unavailable (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_claim(
    _credential: ApiCredential,
    State(state): State<AppState>,
) -> Response {
    let access = match state.api.get_settings(ACCESS_PATH).await {
        Ok(value) => value,
        Err(err) => return bus_api_error(&err, Some(ACCESS_PATH)),
    };
    api_response(StatusCode::OK, claim_status(&state, &access).await)
}

// Physical presence, the reset tiers and credential recovery
// (`docs/design/recovery.md` §2, §4 and §5)

/// The named board capability that decides what a presence assertion IS.
///
/// **One seam, keyed by one capability.** `docs/design/recovery.md` §4.2
/// admits three kinds of mechanism — a physical control across a power cycle,
/// a local console the operator is attached to, and a file placed on the boot
/// medium with the medium out of the device — and both mos boards answer this
/// capability with [`PRESENCE_CONSOLE_ATTACH`] and nothing else. A board that
/// later qualifies a different mechanism answers it differently and reaches
/// the same gate; no flow reshapes, and nothing here anticipates one. In
/// particular there is no button code in this crate and no document claiming a
/// button flow: the cx3576 recovery button drops the board into rockusb loader
/// mode today and no software recovery flow reads it, which §4.2 and §8 record
/// as bench-dependent.
pub(crate) const PRESENCE_CAPABILITY: &str = "recovery.presence";

/// The capability's value on cx3576 and on x64: an operator attached to the
/// device's local console.
pub(crate) const PRESENCE_CONSOLE_ATTACH: &str = "console-attach";

/// The §5.3 event name for the console-attach flow, which is the mechanism in
/// the name rather than in a fourth member of the line.
///
/// §6's implemented line shape has exactly four members — timestamp, event,
/// outcome, source — so one enumerated event per mechanism keeps the trail's
/// grammar unchanged while making "which door was used" greppable. The
/// siblings §5.3 names (`credential-recovery-button`,
/// `credential-recovery-medium`, `credential-recovery-factory`) are NOT
/// declared here: a constant for a door this build cannot open would be a
/// claim the board table does not support.
const CREDENTIAL_RECOVERY_CONSOLE_EVENT: &str = "credential-recovery-console";

/// Where the console-attached asserter leaves its assertion.
///
/// On tmpfs and owned by root, which is what makes it presence rather than a
/// flag: `docs/design/recovery.md` §4.2's rule is that the action must be one
/// **no network client can perform**, and nothing reachable over the network
/// writes here. apid only ever READS it — there is no route, no settings path
/// and no code in this crate that creates it — so an API that could set it
/// would have to be written first, which is the change §4.2 forbids.
const PRESENCE_MARKER_PATH: &str = "/run/mos/presence";

/// A presence assertion made at the device.
pub(crate) struct Assertion {
    /// The mechanism, which is also what the audit event is named for.
    mechanism: &'static str,
    /// The channel that proved presence, and therefore the ONE channel a
    /// minted credential may be published on (§5.1 rule 2).
    channel: PathBuf,
}

impl Assertion {
    /// The assertion a test's presence seam hands back.
    ///
    /// Test-only, and the channel is deliberately a path nothing opens: a test
    /// seam captures what it was asked to publish rather than writing it, so
    /// there is no file anywhere for a minted credential to be left in.
    #[cfg(test)]
    pub(crate) fn console_for_test() -> Self {
        Self {
            mechanism: board_presence_mechanism(),
            channel: PathBuf::from("/dev/null"),
        }
    }
}

/// Why an assertion was not established. Named, because §5.3 audits a refusal
/// and an operator has to be able to tell "nobody is at the device" from "the
/// assertion has run out".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoPresence {
    /// No assertion has been made.
    Absent,
    /// One was made and its window has passed.
    Expired,
    /// The marker is there but this build cannot read it as an assertion.
    Malformed,
    /// The assertion names a mechanism this board does not answer the
    /// capability with.
    UnknownMechanism,
}

impl NoPresence {
    /// The sentence the refusal carries. It names no path and no value the
    /// marker held: a refusal is read by whoever asked, and what it may tell
    /// them is that presence was not established.
    fn message(self) -> String {
        match self {
            Self::Absent => {
                "this operation requires physical presence at the device, and none is asserted"
                    .to_string()
            }
            Self::Expired => {
                "the physical-presence assertion has expired; assert it again at the device"
                    .to_string()
            }
            Self::Malformed => {
                "the physical-presence assertion could not be read; assert it again at the device"
                    .to_string()
            }
            Self::UnknownMechanism => format!(
                "the physical-presence assertion names a mechanism this board does not offer; \
                 it answers `{PRESENCE_CAPABILITY}` with `{}`",
                board_presence_mechanism()
            ),
        }
    }
}

/// What this board answers [`PRESENCE_CAPABILITY`] with.
///
/// **The whole of the board-specific part of the gate**, and it is one
/// function so that a board qualifying a different mechanism changes this and
/// nothing else — no flow reshapes, no route moves, no new field appears. Both
/// mos boards answer [`PRESENCE_CONSOLE_ATTACH`] (`docs/design/recovery.md`
/// §4.2, §8), which is why there is no per-board branch here to test: a branch
/// with one arm reachable would be speculative code for a mechanism no board
/// has evidenced.
fn board_presence_mechanism() -> &'static str {
    PRESENCE_CONSOLE_ATTACH
}

/// The ONE seam every presence-gated operation passes through.
///
/// Two operations and not one, because §5.1 rule 2 binds them together: the
/// credential is returned "on the channel that proved presence", so whatever
/// decides presence is also what decides where a secret may be written. A
/// design that asserted here and published somewhere else could publish over
/// the network, which is the one thing §4.2 forbids outright.
pub(crate) trait Presence: Send + Sync {
    /// The assertion standing at this moment, or why there is none.
    fn assert(&self) -> Result<Assertion, NoPresence>;

    /// Write `secret` on the channel that proved presence, exactly once.
    ///
    /// # Errors
    ///
    /// Returns an error when the channel cannot be written, which is §5.3's
    /// `aborted`: presence was established and the flow did not complete.
    fn publish(&self, assertion: &Assertion, secret: &str) -> anyhow::Result<()>;
}

/// The shipped mechanism: an assertion left by an operator at the local
/// console, published back to the console they are attached to.
pub(crate) struct ConsolePresence {
    marker: PathBuf,
}

/// What [`PRESENCE_MARKER_PATH`] holds. Three members and no room for a
/// fourth: an assertion is a mechanism, a channel and a deadline.
#[derive(serde::Deserialize)]
struct PresenceMarker {
    /// The mechanism asserted, which must be the board capability's value.
    mechanism: String,
    /// The console device the operator is attached to.
    channel: PathBuf,
    /// UNIX seconds at which the assertion stops standing.
    ///
    /// **Required, and an assertion without one does not parse.** §5.4 gives
    /// the flow its own bound — "one rotation per presence assertion, and the
    /// assertion is re-performed physically for the next one" — and a marker
    /// with no deadline would turn one visit to the device into a standing
    /// permission, which is the permanent shell §4.3 refuses.
    expires: u64,
}

impl ConsolePresence {
    /// The shipped reader. A path and no syscall until something asserts.
    pub(crate) fn at_default() -> Self {
        Self {
            marker: PathBuf::from(PRESENCE_MARKER_PATH),
        }
    }
}

impl Presence for ConsolePresence {
    fn assert(&self) -> Result<Assertion, NoPresence> {
        let bytes = std::fs::read(&self.marker).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                NoPresence::Absent
            } else {
                NoPresence::Malformed
            }
        })?;
        let marker: PresenceMarker =
            serde_json::from_slice(&bytes).map_err(|_| NoPresence::Malformed)?;
        if marker.mechanism != board_presence_mechanism() {
            return Err(NoPresence::UnknownMechanism);
        }
        if marker.expires <= device_clock_seconds() {
            return Err(NoPresence::Expired);
        }
        Ok(Assertion {
            mechanism: board_presence_mechanism(),
            channel: marker.channel,
        })
    }

    fn publish(&self, assertion: &Assertion, secret: &str) -> anyhow::Result<()> {
        use std::io::Write;

        // A console is a character device. A REGULAR FILE is refused, and that
        // refusal is the whole of the check: publishing to a file would leave
        // the one copy of a minted credential on a filesystem, which is
        // §4.3's "reading, decrypting or exporting any stored secret" arrived
        // at from the other side.
        let metadata = std::fs::metadata(&assertion.channel)
            .with_context(|| format!("open {}", assertion.channel.display()))?;
        anyhow::ensure!(
            !metadata.is_file(),
            "{} is a regular file, not a console",
            assertion.channel.display()
        );
        let mut channel = std::fs::OpenOptions::new()
            .write(true)
            .open(&assertion.channel)
            .with_context(|| format!("open {}", assertion.channel.display()))?;
        writeln!(
            channel,
            "mos recovery: the new administrator password is {secret}"
        )?;
        channel.flush()?;
        Ok(())
    }
}

/// `POST /api/v1/reset` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub(crate) struct ResetRequest {
    /// Which tier. §2.2: "A tier names itself in the request and in the audit
    /// record. There is no parameterless reset."
    #[schema(value_type = String, example = "configuration")]
    tier: ResetTier,
}

/// `POST /api/v1/reset` response body.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResetStaged {
    /// The tier that is now staged.
    #[schema(value_type = String, example = "configuration")]
    tier: ResetTier,
    /// When it runs. Always `next-boot`: §2.2 requires a reset to be an intent
    /// record plus an idempotent apply, and mosd applies it before anything
    /// else on the next boot.
    applies: &'static str,
}

/// `POST /api/v1/recovery/credential` response body.
///
/// **It carries no secret and there is no member it could carry one in.**
/// §5.1 rule 2 returns the credential on the channel that proved presence,
/// never over the network, so the body says that a credential was minted and
/// where it went — the operator standing at the console reads it there.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CredentialRecovered {
    /// The mechanism that proved presence and published the credential.
    mechanism: &'static str,
    /// `access.device.generation` after the rotation (§5.1 rule 5).
    generation: u32,
}

/// The §6 audit event a staged tier is recorded under.
fn reset_event(tier: ResetTier) -> &'static str {
    match tier {
        ResetTier::Configuration => "reset-configuration",
        ResetTier::ApplicationData => "reset-application-data",
        ResetTier::FullFactory => "reset-full-factory",
    }
}

/// Whether a tier may only be reached with physical presence.
///
/// §2.2's line, and the reason it is drawn there: "a device whose identity and
/// credentials are gone cannot be handed back to its owner over the network".
/// Tiers 1 and 2 are authenticated management actions.
fn tier_needs_presence(tier: ResetTier) -> bool {
    matches!(tier, ResetTier::FullFactory)
}

/// §4's refusal, in §2.4's envelope.
fn presence_refusal(reason: NoPresence) -> Response {
    api_response(
        StatusCode::FORBIDDEN,
        ApiError::apid("presence_required", reason.message()),
    )
}

/// Stage a reset tier.
///
/// **This route stages; mosd applies.** §2.2 requires a reset to be an intent
/// record plus an idempotent apply, so the whole of this handler's write is
/// ONE `SetSettings("reset")` — one `Store::save` — after which a power loss
/// leaves the device either not asked or asked, never half-reset. mosd carries
/// the tier out before anything else on the next boot and clears the record.
///
/// **Authority, per §2.2.** Tiers 1 and 2 are authenticated management
/// actions. Tier 3 additionally requires §4 physical presence; the order it
/// sits at in §3's decision tree is after credential recovery, so an operator
/// who reaches it holds a credential as well as standing at the device. There
/// is no tier 4: `ResetTier` has three members, so `secure-wipe` is a body
/// this route cannot parse.
///
/// A tier staged over another replaces it. Both are audited, so nothing is
/// silent, and the alternative — refusing until something un-stages the first
/// — is a dead end for an operator who asked for the wrong one.
#[utoipa::path(
    post,
    path = V1_RESET_PATH,
    context_path = API,
    tag = "actions",
    request_body = ResetRequest,
    responses(
        (status = 202, description = "The tier is staged and runs on the next boot", body = ResetStaged),
        (status = 400, description = "The body is not JSON (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "The tier requires physical presence at the device and none is asserted (`presence_required`)", body = ApiError),
        (status = 409, description = "The device was claimed with a bootstrap credential that has not been rotated (`rotation_required`)", body = ApiError),
        (status = 422, description = "The body is not this shape, or names no tier this device implements (`validation_failed`)", body = ApiError),
        (status = 500, description = "mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_reset(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let request: ResetRequest = match json_body(body, Some(RESET_PATH)) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    let event = reset_event(request.tier);

    // Presence BEFORE the write and before anything else this handler does, so
    // a refused tier 3 is exactly a refused tier 3: nothing staged, nothing
    // cleared, one audit line.
    let presence = if tier_needs_presence(request.tier) {
        match state.presence.assert() {
            Ok(assertion) => Some(assertion.mechanism.to_string()),
            Err(reason) => {
                state.audit.record(event, "refused", &source);
                return presence_refusal(reason);
            }
        }
    } else {
        None
    };

    let intent = serde_json::json!({
        "tier": request.tier,
        "requested": device_clock_seconds(),
        "presence": presence,
    });
    if let Err(err) = state.api.set_settings(RESET_PATH, &intent).await {
        return bus_api_error(&err, Some(RESET_PATH));
    }
    state.audit.record(event, "staged", &source);
    api_response(
        StatusCode::ACCEPTED,
        ResetStaged {
            tier: request.tier,
            applies: "next-boot",
        },
    )
}

/// Recover the management credential: rotate, never reveal.
///
/// **Presence is the authority, and the only one.** §5.2: an authenticated
/// management session may NOT run this flow — a session that can rotate the
/// credential it authenticated with is a session-fixation lever, and an
/// operator holding a working credential needs
/// `POST /api/v1/actions/change-password` rather than recovery. So the route
/// takes no credential extractor and refuses a caller that presents one.
///
/// **What it does, in §5.1's order.** It MINTS a new password — it never
/// discloses, decrypts or derives the previous secret, and there is no code
/// path from here to a stored plaintext. It publishes the new one exactly
/// once, on the channel that proved presence. Only then does it commit, in ONE
/// write of `access`: the new hash, the emptied token list, the claim record
/// and the bumped generation together. Publishing first is §5.1 rule 3's
/// direction — a crash between the two must not be a self-inflicted lockout —
/// so the worst outcome here is a credential the operator saw and that never
/// worked, which the flow being cheap to re-run answers.
///
/// **The previous credential stops working at that commit**, and so does every
/// API token: §3's step 5 prices it, "any client or automation holding it must
/// be re-enrolled". Every session goes with it, for the reason a password
/// change drops them.
///
/// **A device-claimed by a provisioning document is the seam P2 named.** Its
/// bootstrap secret sat in plaintext on a medium, and losing it before the
/// forced rotation is a §5 case rather than an `docs/design/access.md` §4.4
/// one. A recovery writes the claim record with the channel that claimed the
/// device preserved and `rotationRequired` FALSE: the credential this flow
/// mints was drawn by the device from `OsRng` and shown once at the device, so
/// it is not a bootstrap secret, and demanding a rotation of a credential that
/// was just rotated under physical presence would be a bound with nothing left
/// to protect.
///
/// **An unclaimed device is refused**, pointing at `POST /api/v1/setup`. There
/// is nothing to recover on a device that has no credential, and minting one
/// here would be a third channel that can claim a device —
/// `mosd_settings::ClaimChannel` has exactly two members and says why.
#[utoipa::path(
    post,
    path = V1_RECOVERY_CREDENTIAL_PATH,
    context_path = API,
    tag = "actions",
    responses(
        (status = 200, description = "A new credential was minted and published on the channel that proved presence; the body carries no secret", body = CredentialRecovered),
        (status = 403, description = "Physical presence is not asserted (`presence_required`), or the caller is authenticated and must use `POST /api/v1/actions/change-password` instead (`authenticated_session`)", body = ApiError),
        (status = 409, description = "The device has no administrator credential to recover; claim it with `POST /api/v1/setup` (`not_claimed`)", body = ApiError),
        (status = 500, description = "Hashing the new password failed (`hash_failed`), it could not be published on the presence channel (`publish_failed`), or mosd failed to write (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_recovery_credential(
    State(state): State<AppState>,
    Source(source): Source,
    headers: HeaderMap,
) -> Response {
    let event = CREDENTIAL_RECOVERY_CONSOLE_EVENT;

    // §5.2's third authority does not exist, and the check that it does not is
    // here rather than in a comment. A caller holding a working credential is
    // turned away BEFORE presence is consulted: what they are told is which
    // route they should have used, and that answer does not depend on whether
    // anyone is standing at the device.
    if bearer_is_stored(&state, &headers).await
        || session::cookie_from_headers(&headers)
            .is_some_and(|cookie| state.sessions.verify(&cookie))
    {
        state.audit.record(event, "refused", &source);
        return api_response(
            StatusCode::FORBIDDEN,
            ApiError::apid(
                "authenticated_session",
                "credential recovery is authorized by physical presence and not by a session; \
                 an operator holding a working credential changes it with \
                 `POST /api/v1/actions/change-password`"
                    .to_string(),
            ),
        );
    }

    // §5.4's first rule: a presence-gated rotation is NOT throttled by the
    // login guard. The guard slows a remote guesser, presence is not
    // guessable, and a device whose operator is standing in front of it must
    // not be made to wait out a window an attacker armed. Nothing on this path
    // calls `begin_attempt`, and that absence is the rule.
    let assertion = match state.presence.assert() {
        Ok(assertion) => assertion,
        Err(reason) => {
            state.audit.record(event, "refused", &source);
            return presence_refusal(reason);
        }
    };

    let access = match state.api.get_settings(ACCESS_PATH).await {
        Ok(value) => value,
        Err(err) => return bus_api_error(&err, Some(ACCESS_PATH)),
    };
    if password_hash(&access).is_none() {
        state.audit.record(event, "refused", &source);
        return api_response(
            StatusCode::CONFLICT,
            ApiError::apid(
                "not_claimed",
                "this device has no administrator credential to recover; claim it with \
                 `POST /api/v1/setup`"
                    .to_string(),
            )
            .at("access.webAdmin"),
        );
    }
    let claim = claim_status(&state, &access).await;

    let secret = mint_recovery_password();
    let hashed = secret.clone();
    let hash = match tokio::task::spawn_blocking(move || auth::hash_password(&hashed))
        .await
        .unwrap_or_else(|err| Err(anyhow::anyhow!("password hashing task: {err}")))
    {
        Ok(hash) => hash,
        Err(err) => {
            // Logged and not returned, for the setup route's reason: the error
            // carries argon2's own text and the caller can do nothing with it.
            tracing::error!(error = %err, "recovery password hashing failed");
            state.audit.record(event, "aborted", &source);
            return api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid(
                    "hash_failed",
                    "the new credential could not be hashed; nothing was written".to_string(),
                ),
            );
        }
    };

    // Published BEFORE the commit, per §5.1 rule 3's direction. A failure here
    // is §5.3's `aborted`: presence was established and the flow did not
    // complete, the previous credential still works, and nothing was written.
    if let Err(err) = state.presence.publish(&assertion, &secret) {
        tracing::error!(error = %err, "publishing the recovered credential failed");
        state.audit.record(event, "aborted", &source);
        return api_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::apid(
                "publish_failed",
                "the new credential could not be published on the channel that proved \
                 presence; nothing was written and the previous credential still works"
                    .to_string(),
            ),
        );
    }

    // The commit: ONE write of the whole `access` subtree, so the new hash,
    // the revoked tokens, the claim record and the bumped generation land
    // together. Two writes could crash between them and leave a device whose
    // old credential was invalidated and whose new one was not stored.
    let generation = device_generation(&access).saturating_add(1);
    let mut subtree = match access {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    subtree.insert(
        "webAdmin".to_string(),
        serde_json::json!({ "password_hash": hash }),
    );
    // Every bearer token is a management credential too, and §3's step 5 says
    // in terms that the previous one stops working: a recovery that left them
    // authenticating would leave whoever holds one exactly the access the
    // operator came to the device to take back.
    subtree.insert("apiTokens".to_string(), Value::Array(Vec::new()));
    subtree.insert(
        "claim".to_string(),
        serde_json::to_value(ClaimSettings {
            // A device claimed by a document carries no record, and the channel
            // that claimed it is still that document — the rotation moves the
            // credential, not the history.
            via: claim.via.unwrap_or(ClaimChannel::ProvisioningDocument),
            // WHEN the device was claimed does not move because its credential
            // did; P2's precedent, and 0 is the reading an unset clock gives.
            at: claim.at.unwrap_or(0),
            rotation_required: false,
        })
        .expect("the claim record serializes"),
    );
    let mut device = subtree
        .get("device")
        .cloned()
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    device.insert("generation".to_string(), Value::from(generation));
    subtree.insert("device".to_string(), Value::Object(device));
    if let Err(err) = state
        .api
        .set_settings(ACCESS_PATH, &Value::Object(subtree))
        .await
    {
        state.audit.record(event, "aborted", &source);
        return bus_api_error(&err, Some(ACCESS_PATH));
    }

    // apid knows its own access write happened, so the gate's cache is dropped
    // here rather than waiting for the SettingsChanged round trip.
    state.access_cache.invalidate();
    // Every session was minted under a credential that no longer exists.
    state.sessions.remove_all_except("");
    // §5.4's second rule, and the release path §4.3 item 2 promises: a
    // SUCCESSFUL rotation clears the guard, counters and window both, which is
    // what would make a hard `lockoutThreshold` safe to ship. A refused or
    // aborted one clears nothing — every early return above leaves this line
    // unreached, which is the rule rather than a comment about it.
    state.guard.record_success();
    state.audit.record(event, "success", &source);

    api_response(
        StatusCode::OK,
        CredentialRecovered {
            mechanism: assertion.mechanism,
            generation,
        },
    )
}

/// `access.device.generation` as the subtree holds it, or 0.
///
/// A stored value this build cannot read is 0 and therefore rotates to 1,
/// which errs towards moving the counter rather than towards a rotation that
/// left "which credential is of record" unanswerable (§5.1 rule 5).
fn device_generation(access: &Value) -> u32 {
    access
        .get("device")
        .and_then(|device| device.get("generation"))
        .and_then(Value::as_u64)
        .and_then(|generation| u32::try_from(generation).ok())
        .unwrap_or(0)
}

/// A fresh administrator password: 128 bits of `OsRng`, hex.
///
/// Hex and not a denser alphabet because an operator reads this off a console
/// and types it into a browser, and 32 unambiguous characters beat 22 with a
/// case-and-symbol alphabet at the same entropy. Well over
/// `MIN_ADMIN_PASSWORD_LEN`, which is a floor for a password a human chose.
fn mint_recovery_password() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// Login / logout

// Password change

/// Why one password-change attempt failed, before either surface words it.
///
/// One outcome set for both surfaces: the HTML pane and the API route differ
/// in how they answer, not in what can happen.
enum PasswordChangeError {
    /// The current password did not verify; nothing was written.
    WrongCurrent,
    /// The new password is under the same floor the setup wizard enforces;
    /// nothing was written.
    TooShort,
    /// Hashing the new password failed.
    Hashing(anyhow::Error),
    /// A mosd call failed.
    Bus(anyhow::Error),
}

/// Verify the current admin password, write the new hash through the settings
/// tree, and drop every session except the acting one.
///
/// The current password is demanded even though the caller holds a session: a
/// session is a browser artifact that outlives the moment the password was
/// typed, and an unattended browser must not be enough to rotate the sole
/// credential on the management surface.
///
/// The invalidation and the write belong in one step. The gate's
/// short-circuit comment says an unset-password operation "has to clear the
/// session table in the same step", and replacing the hash is the same
/// reasoning: a session minted under the old credential proves possession of
/// nothing any more. The acting session is the one exception — it just proved
/// possession of the current password — or the operator would be signed out
/// by their own success.
async fn change_password(
    state: &AppState,
    source: &str,
    acting_session: Option<&str>,
    current: &str,
    new: &str,
) -> Result<(), PasswordChangeError> {
    if password_under_floor(new) {
        return Err(PasswordChangeError::TooShort);
    }
    let access = match state.api.get_settings("access").await {
        Ok(value) => value,
        Err(err) => return Err(PasswordChangeError::Bus(err)),
    };
    let Some(hash) = password_hash(&access) else {
        // Unreachable through either surface: both sit behind a verified
        // session, and no session can coexist with an unset password (see
        // `gate`). Refusing is still better than writing a first hash from a
        // route whose contract is rotation.
        return Err(PasswordChangeError::Bus(anyhow::anyhow!(
            "no admin password is configured"
        )));
    };
    // Off the async workers for the same reason login verification is:
    // argon2id costs real CPU per call, by design. A panic in the closure
    // surfaces as a failed verification: closed, never open.
    let hash = hash.to_string();
    let password = current.to_string();
    let verified = tokio::task::spawn_blocking(move || auth::verify_password(&hash, &password))
        .await
        .unwrap_or_else(|err| {
            tracing::error!(error = %err, "password verification task failed");
            false
        });
    if !verified {
        state.audit.record("password", "wrong-password", source);
        return Err(PasswordChangeError::WrongCurrent);
    }
    let password = new.to_string();
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .unwrap_or_else(|err| Err(anyhow::anyhow!("password hashing task: {err}")))
        .map_err(PasswordChangeError::Hashing)?;
    // Whether this change is the ROTATION that discharges a bootstrap claim,
    // decided from the tree as it was read above and not from the tree after
    // the write.
    let claim = claim_status(state, &access).await;
    let discharged = claim.rotation_required;
    // The new hash and the record that the bootstrap secret is gone commit
    // together, in ONE write of the whole `access` subtree and therefore one
    // `Store::save`. Two writes could crash between them, and both halves of
    // that crash are wrong: a device still demanding a rotation it has already
    // had, or -- with the writes the other way round -- one excused from a
    // rotation that never landed.
    let mut subtree = match access {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    subtree.insert(
        "webAdmin".to_string(),
        serde_json::json!({ "password_hash": hash }),
    );
    if let Some(via) = claim.via {
        subtree.insert(
            "claim".to_string(),
            // Infallible: a struct of scalars with no map keys to collide.
            serde_json::to_value(ClaimSettings {
                via,
                // WHEN the device was claimed does not move because its
                // credential did. 0 is the reading `ApiToken::created` gives an
                // unset clock, and it is what a derived record carries when the
                // import that claimed the device recorded none.
                at: claim.at.unwrap_or(0),
                rotation_required: false,
            })
            .expect("the claim record serializes"),
        );
    }
    if let Err(err) = state
        .api
        .set_settings(ACCESS_PATH, &Value::Object(subtree))
        .await
    {
        return Err(PasswordChangeError::Bus(err));
    }
    // apid knows its own access write happened, so the gate's cache is
    // dropped here rather than waiting for the SettingsChanged round trip:
    // the next unauthenticated request re-reads and cannot be answered from
    // a pre-change snapshot.
    state.access_cache.invalidate();
    // The write happened; every other session goes with the old credential.
    // No cookie on the request keeps nothing, which errs closed.
    state
        .sessions
        .remove_all_except(acting_session.unwrap_or(""));
    state.audit.record("password", "changed", source);
    if discharged {
        // A second line and not a replacement: the password change happened
        // and is recorded as one, and this says what it additionally did to
        // the device's claim. An operator reading the trail for "when did this
        // device stop holding the credential it shipped with" greps one name.
        state
            .audit
            .record(CLAIM_ROTATION_EVENT, "completed", source);
    }
    Ok(())
}

/// `POST /api/v1/actions/change-password` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChangePasswordRequest {
    /// The password being replaced, verified before anything is written.
    current_password: String,
    /// The replacement; at least 8 characters.
    new_password: String,
}

/// Change the admin password.
///
/// Requires the current password. Answers **204** on success; every session
/// but the acting one is dropped. A wrong current password is **401**, and a
/// new password below the length floor is **422**.
#[utoipa::path(
    post,
    path = V1_CHANGE_PASSWORD_PATH,
    context_path = API,
    tag = "actions",
    request_body = ChangePasswordRequest,
    responses(
        (status = 204, description = "The password was changed; every session except the calling one was dropped"),
        (status = 400, description = "The body is not JSON, or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "The browser CSRF token is invalid (`csrf_invalid`), or the current password does not verify (`wrong_password`)", body = ApiError),
        (status = 422, description = "The new password is shorter than 8 characters (`validation_failed`)", body = ApiError),
        (status = 500, description = "Hashing failed (`hashing_failed`), or mosd failed to answer (`settings_io`, `mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_change_password(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
    headers: HeaderMap,
    body: Result<Json<ChangePasswordRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        // §2.4's envelope rather than axum's plain-text rejection.
        Err(rejection) => {
            return api_response(
                StatusCode::BAD_REQUEST,
                ApiError::apid("request_invalid", rejection.body_text()),
            );
        }
    };
    let acting = session::cookie_from_headers(&headers);
    match change_password(
        &state,
        &source,
        acting.as_deref(),
        &request.current_password,
        &request.new_password,
    )
    .await
    {
        Ok(()) => (
            StatusCode::NO_CONTENT,
            [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
        )
            .into_response(),
        Err(PasswordChangeError::WrongCurrent) => api_response(
            StatusCode::FORBIDDEN,
            ApiError::apid(
                "wrong_password",
                "the current password does not verify".to_string(),
            ),
        ),
        Err(PasswordChangeError::TooShort) => api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid(
                "validation_failed",
                "the new password must be at least 8 characters".to_string(),
            ),
        ),
        Err(PasswordChangeError::Hashing(err)) => {
            tracing::error!(error = %err, "password hashing failed");
            api_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiError::apid("hashing_failed", format!("{err:#}")),
            )
        }
        Err(PasswordChangeError::Bus(err)) => bus_api_error(&err, Some("access.webAdmin")),
    }
}

// Network pane

/// The `network` settings subtree, as a map of typed entries.
///
/// Parsed into `mosd_settings` types rather than read out of the JSON by key,
/// so the pane renders exactly the schema mosd deserializes and a field this
/// file misspells is a compile error rather than a blank input.
type NetworkEntries = std::collections::BTreeMap<String, IfaceSettings>;

/// Parse readable entries from the configured network map.
fn parse_network(network: &Value) -> (NetworkEntries, Vec<String>) {
    let empty = serde_json::Map::new();
    let mut entries = NetworkEntries::new();
    let mut unreadable = Vec::new();
    for (name, body) in network.as_object().unwrap_or(&empty) {
        match serde_json::from_value::<IfaceSettings>(body.clone()) {
            Ok(cfg) => {
                entries.insert(name.clone(), cfg);
            }
            Err(_) => unreadable.push(name.clone()),
        }
    }
    (entries, unreadable)
}

fn peers_settings_path(iface: &str) -> String {
    format!("{}.wireguard.peers", iface_settings_path(iface))
}

// Power pane

/// A power action the pane can request of mosd.
#[derive(Clone, Copy)]
enum PowerAction {
    Reboot,
    PowerOff,
}

impl PowerAction {
    /// Stable action name used in the audit trail.
    fn confirm_token(self) -> &'static str {
        match self {
            Self::Reboot => "reboot",
            Self::PowerOff => "poweroff",
        }
    }
}

/// Audit the request and hand the action to mosd on a detached task.
///
/// Both surfaces call this and neither has its own copy, because the detached
/// spawn is the reason both answer **202**: the response is built and returned
/// without awaiting the D-Bus call, so whether the action completed is not
/// knowable over the connection that asked for it. A second copy here could
/// drift into awaiting the call on one surface and not the other, and the
/// status code would then be a lie on one of them.
///
/// Recorded before the request is dispatched, and the sink fsyncs each line:
/// the two audited actions here are the ones immediately followed by the
/// machine going down, so a line written after the call could be the line that
/// never reaches the disk.
fn dispatch_power_action(state: &AppState, action: PowerAction, source: &str) {
    state
        .audit
        .record(action.confirm_token(), "requested", source);
    let api = state.api.clone();
    tokio::spawn(async move {
        let result = match action {
            PowerAction::Reboot => api.reboot().await,
            PowerAction::PowerOff => api.power_off().await,
        };
        if let Err(err) = result {
            tracing::error!(action = action.confirm_token(), error = %err, "power action failed");
        }
    });
}

/// The 202 both power verbs answer, and the reason it is 202.
///
/// **Not 204.** The bus call is spawned on a detached task by
/// [`dispatch_power_action`], so the response leaves before the machine goes
/// down. 202 is the honest code: the request was accepted, and whether it
/// completed is not knowable over the connection that asked. A 204 would claim
/// the action had finished, which this route cannot know and, on a real
/// appliance, will usually be answering from a machine that is about to stop
/// existing. It is also what the form path already answers, measured rather
/// than assumed.
///
/// **No confirmation token.** The two form posts demand one, and this does
/// not. `TRANSIENT_CONFIRM_TOKEN` and [`PowerAction::confirm_token`] are
/// compile-time constants, not secrets and not per-session; they exist to stop
/// a mis-click on a rendered page. There is no mis-click on a `POST` a script
/// constructed, so the bearer token is the authorisation and the constant would
/// be friction that protects nothing. Ratified in the M1 design.
///
/// The body is empty: the outcome is the machine going down, and there is
/// nothing to say about it that the status does not.
fn power_accepted(state: &AppState, action: PowerAction, source: &str) -> Response {
    dispatch_power_action(state, action, source);
    (
        StatusCode::ACCEPTED,
        [(CACHE_CONTROL, CacheClass::NoStore.header_value())],
    )
        .into_response()
}

/// Reboot the appliance.
///
/// Answers **202**: the request is accepted and the reboot is dispatched, so
/// there may be no connection left to carry a later status.
#[utoipa::path(
    post,
    path = V1_REBOOT_PATH,
    context_path = API,
    tag = "actions",
    responses(
        (status = 202, description = "The reboot was accepted and dispatched; the call to mosd is not awaited, so completion is not reported over this connection"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_reboot(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    power_accepted(&state, PowerAction::Reboot, &source)
}

/// Power the appliance off.
///
/// Answers **202**: the request is accepted and the power-off is dispatched,
/// so there may be no connection left to carry a later status.
#[utoipa::path(
    post,
    path = V1_POWEROFF_PATH,
    context_path = API,
    tag = "actions",
    responses(
        (status = 202, description = "The power-off was accepted and dispatched; the call to mosd is not awaited, so completion is not reported over this connection"),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_poweroff(
    _credential: ApiCredential,
    State(state): State<AppState>,
    Source(source): Source,
) -> Response {
    power_accepted(&state, PowerAction::PowerOff, &source)
}

// Hostname submit

// SSH pane

/// Settings dot-path of the stored authorized-key list.
const SSH_KEYS_PATH: &str = "access.ssh.authorizedKeys";

/// The sentence the pane has to carry, verbatim.
///
/// `AuthorizedKeysFile` is `%u`-expanded over one shared key list, so a key
/// added here logs in as root. An operator who adds a colleague's key expecting
/// an unprivileged shell would be handing out root, and a pane that says
/// nothing manufactures exactly that misunderstanding. A test asserts the
/// sentence renders, so a later refactor cannot quietly drop it.
const ROOT_KEY_NOTICE: &str = "Every authorized key is a root key.";

/// Shortest transient password accepted, in bytes; mosd's own floor.
const MIN_TRANSIENT_PASSWORD_BYTES: usize = 8;

/// Longest transient password accepted, in bytes.
///
/// The 72 is not arbitrary, and it is deliberately tighter than mosd's own
/// bound: the transient password is hashed with bcrypt, and bcrypt reads only
/// the first 72 bytes of its input and silently ignores the rest. Accepting a
/// 100-character password would therefore mean the first 72 characters of it
/// also unlock the device — the operator would be running on a shorter secret
/// than the one they typed and believe in. Refusing the input is the only way
/// the pane avoids creating that surprise; truncating it silently would be the
/// same surprise with a different author.
const MAX_TRANSIENT_PASSWORD_BYTES: usize = 72;

/// OpenSSH fingerprint of a canonical `<type> <blob>` key line.
///
/// `SHA256:` followed by the unpadded base64 of the SHA-256 digest of the
/// decoded blob — the string `ssh-keygen -lf` prints, and the same value
/// mosd's sshd reconciler publishes. It is recomputed here rather than read
/// from the published state because the pane has to map the fingerprint an
/// operator clicks back onto the stored entry a removal rewrites, and the
/// published list carries no such handle. A test pins it against fingerprints
/// that came out of `ssh-keygen`, not against itself.
///
/// `None` when the line has no blob or the blob does not decode.
fn ssh_fingerprint(key: &str) -> Option<String> {
    let blob = key.split(' ').nth(1)?;
    let decoded = mosd_settings::decode_base64(blob)?;
    Some(format!(
        "SHA256:{}",
        mosd_settings::encode_base64_nopad(&Sha256::digest(&decoded))
    ))
}

/// The parser's own message, without the dot-path prefix its `Display` adds:
/// the operator is looking at a form field, not at a settings path.
fn key_error_message(err: &SettingsError) -> String {
    match err {
        SettingsError::Validation { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// Read the stored key list out of the `access.ssh` subtree.
///
/// An absent list is an empty list, but a list that is present and unreadable
/// is an error rather than an empty list: treating it as empty
/// would let an add or a remove overwrite keys the operator cannot see.
fn parse_key_list(ssh: &Value) -> anyhow::Result<Vec<AuthorizedKey>> {
    match ssh.get("authorizedKeys") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|err| anyhow::anyhow!("The stored authorized-key list is unreadable: {err}")),
    }
}

/// Bounds and forbidden bytes for a transient password.
///
/// No message echoes the password, and no branch here logs it: the only place
/// it goes is the D-Bus call.
fn validate_transient_password(password: &str) -> Result<(), String> {
    if password.len() < MIN_TRANSIENT_PASSWORD_BYTES {
        return Err(format!(
            "Password must be at least {MIN_TRANSIENT_PASSWORD_BYTES} bytes."
        ));
    }
    if password.len() > MAX_TRANSIENT_PASSWORD_BYTES {
        return Err(format!(
            "Password must be at most {MAX_TRANSIENT_PASSWORD_BYTES} bytes: it is hashed with bcrypt, which reads only the first {MAX_TRANSIENT_PASSWORD_BYTES} bytes, so a longer one would be silently shortened to that."
        ));
    }
    if password.contains(['\0', '\n', '\r']) {
        return Err("Password must not contain a NUL, newline or carriage return.".to_string());
    }
    Ok(())
}

/// `POST /api/v1/actions/transient-root-password` request body.
#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TransientRootPasswordRequest {
    /// The password to open the channel with: 8 to 72 bytes, and no NUL,
    /// newline or carriage return.
    password: String,
}

// Bounds are checked by the same function the form path calls, not a second
// copy, and none of its three messages interpolates the password.
/// Set a transient root password.
///
/// The password must be 8 to 72 bytes and contain no NUL, newline or carriage
/// return. 72 is bcrypt's limit — a longer password would be silently
/// truncated, so it is refused instead.
///
/// The password is written into no setting, is never logged, and lasts until
/// the next reboot. A **422** states which bound was broken and never repeats
/// the password back.
///
/// Answers **202** once the password hash is written and its scoped apply is queued.
#[utoipa::path(
    post,
    path = V1_TRANSIENT_PASSWORD_PATH,
    context_path = API,
    tag = "actions",
    request_body = TransientRootPasswordRequest,
    responses(
        (status = 202, description = "The transient root password hash was written and its scoped apply was queued", body = TaskAccepted),
        (status = 400, description = "The body is not JSON, or not this shape (`request_invalid`)", body = ApiError),
        (status = 401, description = "No stored bearer token or authenticated browser session (`not_authenticated`)", body = ApiError),
        (status = 403, description = "A browser session mutation omitted or supplied the wrong CSRF token (`csrf_invalid`)", body = ApiError),
        (status = 422, description = "The password is shorter than 8 bytes, longer than 72, or contains a NUL, newline or carriage return (`validation_failed`); the message states the bound and never the password", body = ApiError),
        (status = 500, description = "mosd failed to set it (`mosd_failed`)", body = ApiError),
        (status = 503, description = "The call to mosd could not be made (`mosd_unreachable`); carries `Retry-After`", body = ApiError),
        (status = 504, description = "The bounded call to mosd timed out (`mosd_timeout`); the operation may still be running", body = ApiError),
        (status = 405, description = "A method this route does not serve (`method_not_allowed`); carries `Allow`", body = ApiError),
    ),
)]
pub(crate) async fn api_v1_transient_root_password(
    _credential: ApiCredential,
    State(app): State<AppState>,
    Source(source): Source,
    body: Result<Json<TransientRootPasswordRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(request) = match body {
        Ok(body) => body,
        // §2.4's envelope rather than axum's plain-text rejection. The
        // rejection text describes the shape, never the value, so a malformed
        // body carrying a password does not put it in the response either.
        Err(rejection) => {
            return api_response(
                StatusCode::BAD_REQUEST,
                ApiError::apid("request_invalid", rejection.body_text()),
            );
        }
    };
    if let Err(message) = validate_transient_password(&request.password) {
        return api_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::apid("validation_failed", message),
        );
    }
    let task_id = match app.api.set_transient_root_password(&request.password).await {
        Ok(task_id) => task_id,
        Err(err) => {
            // No dot-path: this writes no setting, so §2.4's optional member is
            // absent rather than naming something that was not at fault.
            return bus_api_error(&err, None);
        }
    };
    // The event carries who opened a password channel and from where — and
    // deliberately nothing about the password itself.
    app.audit.record("transient-password", "set", &source);
    api_response(StatusCode::ACCEPTED, TaskAccepted { task_id })
}
