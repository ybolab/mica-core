//! The claim lifecycle: the one-write commit, the retry after an interrupted
//! attempt, the already-claimed refusal and the forced rotation that bounds a
//! bootstrap credential.
//!
//! What a provisioning document MEANS — validation, idempotence, its own claim
//! gate — is micad's contract and is tested in `micad/src/provisioning_doc.rs`.
//! What is here is apid's half: the state a claim leaves behind, and the
//! bound apid enforces from it.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use super::*;
use crate::settings_api::TaskNotFound;

const CLAIM_PATH: &str = "/api/v1/claim";
const CHANGE_PASSWORD_PATH: &str = "/api/v1/actions/change-password";

/// The password a claim body carries. A sentinel, so every assertion that it
/// reached nowhere is an assertion about a string nothing else could produce.
const PW_SENTINEL: &str = "PW-SENTINEL-claim-bootstrap";

fn claim_body() -> String {
    json!({ "password": PW_SENTINEL }).to_string()
}

/// A device Layer 1 has provisioned and nothing has claimed.
fn seeded_tree() -> serde_json::Value {
    let mut tree = unconfigured_tree();
    tree["provisioning"] = json!({
        "state": "complete",
        "deviceId": "0123456789abcdef0123456789abcdef",
        "seededGeneration": 1,
    });
    tree
}

/// A device claimed by a provisioning document: an administrator credential,
/// an applied document, and NO claim record — which is the shape apid reads as
/// "claimed from a medium, still holding the password that was on it".
fn document_claimed_tree(password: &str) -> serde_json::Value {
    let mut tree = configured_tree(password);
    tree["provisioning"] = json!({
        "state": "complete",
        "deviceId": "0123456789abcdef0123456789abcdef",
        "seededGeneration": 1,
        "document": {
            "appliedVersion": 1,
            "appliedDigest": "9f2c1d0e5a7b4c3d2e1f00112233445566778899aabbccddeeff001122334455",
            "lastImport": {
                "source": "media",
                "outcome": "applied",
                "at": 1_700_000_000,
            },
        },
    });
    tree
}

/// The `access` subtree as the fake holds it.
async fn access_of(fake: &FakeSettings) -> serde_json::Value {
    fake.get_settings("access").await.unwrap()
}

// --- The one-write commit --------------------------------------------------

/// The credential, the record of how the device was claimed and the minted
/// token reach the bus as ONE write of ONE subtree.
///
/// The assertion is the write LIST and not the resulting tree, because the
/// resulting tree looks identical whether it took one write or three. micad
/// turns one `SetSettings` into one `Store::save`, so "one write here" is
/// exactly "one commit point on the device".
#[tokio::test]
async fn a_route_claim_commits_the_credential_the_record_and_the_token_in_one_write() {
    let (router, fake) = test_app(seeded_tree());

    let response = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    assert_eq!(
        fake.set_paths(),
        vec!["access".to_string()],
        "the claim must be one write of one subtree"
    );
    let access = access_of(&fake).await;
    assert!(access["webAdmin"]["password_hash"].as_str().is_some());
    assert_eq!(access["claim"]["via"], json!("setup"));
    assert_eq!(access["claim"]["rotationRequired"], json!(false));
    assert_eq!(access["apiTokens"].as_array().unwrap().len(), 1);
}

/// Everything the route has no opinion about survives the whole-subtree write.
///
/// The claim writes `access` and not `access.webAdmin`, so the keys it does
/// not name — the SSH policy, the console switch, the first-boot device
/// credential — have to be written back exactly as they were read. A
/// whole-subtree write that rebuilt the object instead would silently reset
/// the appliance's access policy at the moment it was claimed.
#[tokio::test]
async fn the_claim_write_preserves_every_access_key_it_does_not_name() {
    let mut tree = seeded_tree();
    tree["access"] = json!({
        "ssh": {
            "enabled": true,
            "port": 2222,
            "permitRootLogin": false,
            "passwordAuthentication": false,
            "listenAddresses": ["10.0.0.7"],
            "authorizedKeys": [],
        },
        "console": { "shellEnabled": true },
        "device": { "generation": 1 },
    });
    let (router, fake) = test_app(tree);

    let response = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(response.status(), StatusCode::CREATED);

    let access = access_of(&fake).await;
    assert_eq!(access["ssh"]["port"], json!(2222));
    assert_eq!(access["ssh"]["enabled"], json!(true));
    assert_eq!(access["console"]["shellEnabled"], json!(true));
    assert_eq!(access["device"]["generation"], json!(1));
}

// --- Power loss, and the retry ---------------------------------------------

/// A [`SettingsApi`] that fails the FIRST write of a chosen dot-path and then
/// behaves normally.
///
/// This is the power loss, modelled where it is observable: the claim commits
/// through one `Store::save`, and the only two outcomes that save has are
/// "landed" and "did not". A write that does not land is a write the caller
/// sees fail, so failing it here drives the same tree state a power cut
/// before the rename would leave.
struct InterruptOnce {
    inner: Arc<FakeSettings>,
    path: &'static str,
    fired: std::sync::Mutex<bool>,
}

impl InterruptOnce {
    fn new(tree: serde_json::Value, path: &'static str) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(FakeSettings::new(tree)),
            path,
            fired: std::sync::Mutex::new(false),
        })
    }
}

#[async_trait::async_trait]
impl SettingsApi for InterruptOnce {
    async fn get_settings(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        self.inner.get_settings(path).await
    }

    async fn set_settings(&self, path: &str, value: &serde_json::Value) -> anyhow::Result<String> {
        if path == self.path {
            let mut fired = self.fired.lock().unwrap();
            if !*fired {
                *fired = true;
                // Nothing is written: the save never reached its rename.
                return Err(anyhow::anyhow!("interrupted before the commit"));
            }
        }
        self.inner.set_settings(path, value).await
    }

    async fn get_task(&self, id: &str) -> anyhow::Result<crate::task_registry::TaskRecord> {
        self.inner
            .get_task(id)
            .await
            .map_err(|_| TaskNotFound(id.to_string()).into())
    }

    async fn get_state(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        self.inner.get_state(path).await
    }

    async fn get_time_status(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_time_status().await
    }

    async fn get_storage_status(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_storage_status().await
    }

    async fn get_system_info(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_system_info().await
    }

    async fn get_telemetry(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_telemetry().await
    }

    async fn get_observed_network(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_observed_network().await
    }

    async fn get_failure_evidence(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_failure_evidence().await
    }

    async fn reboot(&self) -> anyhow::Result<()> {
        self.inner.reboot().await
    }

    async fn power_off(&self) -> anyhow::Result<()> {
        self.inner.power_off().await
    }

    async fn set_transient_root_password(&self, password: &str) -> anyhow::Result<String> {
        self.inner.set_transient_root_password(password).await
    }

    async fn rotate_wireguard_key(&self, iface: &str) -> anyhow::Result<String> {
        self.inner.rotate_wireguard_key(iface).await
    }

    async fn get_update_state(&self) -> anyhow::Result<serde_json::Value> {
        self.inner.get_update_state().await
    }

    async fn check_update(&self) -> anyhow::Result<()> {
        self.inner.check_update().await
    }

    async fn fetch_update(&self) -> anyhow::Result<()> {
        self.inner.fetch_update().await
    }

    async fn install_update(&self, bundle: &str) -> anyhow::Result<()> {
        self.inner.install_update(bundle).await
    }

    async fn confirm_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        self.inner.confirm_deployment(deployment_id).await
    }

    async fn reject_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        self.inner.reject_deployment(deployment_id).await
    }

    async fn rollback_deployment(&self, deployment_id: &str) -> anyhow::Result<()> {
        self.inner.rollback_deployment(deployment_id).await
    }

    async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<serde_json::Value> {
        self.inner.set_reboot_override(seconds).await
    }

    async fn set_update_config(
        &self,
        patch: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.inner.set_update_config(patch).await
    }
}

/// The interrupted path, then the retry: nothing half-lands, and the retry
/// mints no second identity and no second credential.
///
/// The identity assertion is `provisioning.deviceId`, which is what
/// "without cloning identity" is about: it is drawn once by
/// `micad/src/identity.rs` and the claim never touches it, so a claim driven
/// twice must leave the same one. The credential assertion is the token list,
/// because a claim that retried by appending would leave TWO tokens that both
/// authenticate and only one of which the caller was ever told about.
#[tokio::test]
async fn an_interrupted_claim_writes_nothing_and_the_retry_mints_one_identity() {
    let api = InterruptOnce::new(seeded_tree(), "access");
    let router = app(AppState::new(api.clone(), SIGNING_KEY));

    // The attempt that does not commit.
    let interrupted = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(interrupted.status(), StatusCode::SERVICE_UNAVAILABLE);
    let access = access_of(&api.inner).await;
    assert!(
        access.get("webAdmin").is_none(),
        "an interrupted claim left a credential: {access}"
    );
    assert!(access.get("claim").is_none(), "{access}");
    assert!(access.get("apiTokens").is_none(), "{access}");
    let device_id = api
        .inner
        .get_settings("provisioning.deviceId")
        .await
        .unwrap();

    // The retry, on a device the interruption left exactly as it found it.
    let response = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let token = body_json(response).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let access = access_of(&api.inner).await;
    assert_eq!(
        access["apiTokens"].as_array().unwrap().len(),
        1,
        "the retry minted a second working credential: {access}"
    );
    assert_eq!(access["claim"]["via"], json!("setup"));
    assert_eq!(
        api.inner
            .get_settings("provisioning.deviceId")
            .await
            .unwrap(),
        device_id,
        "the retry moved the device identity"
    );

    // And the one credential the caller was handed is the one that works.
    let probe = bearer(&router, "GET", "/api/v1/meta", &token).await;
    assert_eq!(probe.status(), StatusCode::OK);
}

/// The other half of the same power loss: the save LANDED and the answer never
/// reached the caller, so the caller retries a claim that already happened.
///
/// It is refused, and the refusal is what keeps the device single-credentialled
/// — the first token still authenticates, the claim record still says what it
/// said, and the identity has not moved.
#[tokio::test]
async fn a_claim_that_committed_but_was_never_answered_is_refused_on_retry() {
    let (router, fake) = test_app(seeded_tree());

    let first = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let token = body_json(first).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let committed = access_of(&fake).await;

    let retry = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(retry.status(), StatusCode::CONFLICT);
    assert_eq!(envelope(retry).await["code"], "already_configured");

    assert_eq!(
        access_of(&fake).await,
        committed,
        "the refused retry changed the claim"
    );
    assert_eq!(
        fake.set_paths(),
        vec!["access".to_string()],
        "the refused retry wrote something"
    );
    let probe = bearer(&router, "GET", "/api/v1/meta", &token).await;
    assert_eq!(probe.status(), StatusCode::OK);
}

// --- The already-claimed contract ------------------------------------------

/// The refusal is audited, and it discloses nothing an unauthenticated caller
/// could not already read off `GET /api/v1/session`.
///
/// **The inference, stated.** An unauthenticated caller learns exactly one
/// bit: whether this device has an administrator credential. It learns it from
/// `GET /api/v1/session` already — that route answers `setup` or
/// `unauthenticated` without a credential, because a first-run wizard cannot
/// ask for one — so the 409 here adds nothing to what a device on a network
/// tells anyone who asks. What is NOT disclosed is anything about the
/// credential: not its hash, not its length, not when it was set, not which
/// channel set it. That distinction is the whole of the argument, and it is
/// asserted below rather than promised.
#[tokio::test]
async fn a_refused_claim_is_audited_and_discloses_only_what_the_session_route_does() {
    let dir = TempDir::new().unwrap();
    let fake = Arc::new(FakeSettings::new(configured_tree(PW_SENTINEL)));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY).with_persistence(dir.path()));

    let response = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let headers = format!("{:?}", response.headers());
    let body = body_string(response).await;

    // The one bit, and it is the bit `GET /api/v1/session` already serves.
    let session = get(&router, "/api/v1/session", None).await;
    assert_eq!(session.status(), StatusCode::OK);
    assert_eq!(body_json(session).await["state"], json!("unauthenticated"));

    // And nothing about the credential itself.
    let hash = fake
        .get_settings("access.webAdmin.password_hash")
        .await
        .unwrap();
    let hash = hash.as_str().unwrap();
    for haystack in [&headers, &body] {
        assert!(!haystack.contains(PW_SENTINEL), "{haystack}");
        assert!(!haystack.contains(hash), "{haystack}");
        assert!(!haystack.contains("argon2"), "{haystack}");
        // Not the channel and not the moment either: a refusal says the device
        // is claimed, never how or when.
        assert!(!haystack.contains("provisioning-document"), "{haystack}");
    }

    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [("claim".to_string(), "refused".to_string())]
    );
    let raw = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
    assert!(!raw.contains(PW_SENTINEL), "{raw}");
    assert!(!raw.contains(hash), "{raw}");
}

/// Nothing the claim path emits carries the password, on the success path or
/// on either refusal.
///
/// The sentinel is driven through every surface a secret could leak into: the
/// audit trail, the claim status, the settings tree's own readable fields, and
/// the bodies and headers of both failures. The minted token is deliberately
/// NOT checked out of the success body — that body is the one place it may
/// appear, and it appears there once.
#[tokio::test]
async fn no_claim_surface_emits_the_password() {
    let dir = TempDir::new().unwrap();
    let fake = Arc::new(FakeSettings::new(seeded_tree()));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY).with_persistence(dir.path()));

    // The refusal that happens before anything is written.
    let short = post_json(
        &router,
        "/api/v1/setup",
        &json!({ "password": "PW-SEN" }).to_string(),
        None,
    )
    .await;
    assert_eq!(short.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!body_string(short).await.contains("PW-SEN"));

    let created = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let token = body_json(created).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let status = bearer(&router, "GET", CLAIM_PATH, &token).await;
    assert_eq!(status.status(), StatusCode::OK);
    let status = body_string(status).await;
    assert!(!status.contains(PW_SENTINEL), "{status}");
    // Nor the hash the password became: the claim status is a shape, not a
    // credential read-back.
    assert!(!status.contains("argon2"), "{status}");

    let trail = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
    assert!(!trail.contains(PW_SENTINEL), "{trail}");
    assert!(!trail.contains(&token), "{trail}");

    // The stored claim record itself holds nothing the caller typed.
    let record = access_of(&fake).await["claim"].clone();
    let record = record.to_string();
    assert!(!record.contains(PW_SENTINEL), "{record}");
}

// --- One claim state, two channels -----------------------------------------

/// A claim by route and a claim by document answer the SAME projection, with
/// the channel and the moment each of them can honestly name.
///
/// There is one route that answers "how was this device claimed", and it
/// answers for both. The document channel writes no record of its own — micad's
/// importer is the one writer that does not — and is read off the evidence P1
/// already persists, so there is no second field that could disagree with
/// `access.webAdmin` about whether the device is claimed.
#[tokio::test]
async fn a_document_claim_and_a_route_claim_answer_one_projection() {
    // Claimed by the route.
    let (router, fake) = test_app(seeded_tree());
    let created = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let token = body_json(created).await["token"]
        .as_str()
        .unwrap()
        .to_string();
    let by_route = body_json(bearer(&router, "GET", CLAIM_PATH, &token).await).await;

    assert_eq!(by_route["state"], json!("claimed"));
    assert_eq!(by_route["via"], json!("setup"));
    assert_eq!(by_route["rotationRequired"], json!(false));
    assert_eq!(
        by_route["at"],
        access_of(&fake).await["claim"]["at"],
        "the route's answer must be the stored record and not a second reading"
    );

    // Claimed by a document, on a tree that carries no claim record at all.
    let (tree, token) = with_token(document_claimed_tree("hunter2secret"));
    let (router, _fake) = test_app(tree);
    let by_document = body_json(bearer(&router, "GET", CLAIM_PATH, &token).await).await;

    assert_eq!(by_document["state"], json!("claimed"));
    assert_eq!(by_document["via"], json!("provisioning-document"));
    assert_eq!(by_document["rotationRequired"], json!(true));
    // WHEN comes from the import record P1 already writes, not from a copy.
    assert_eq!(by_document["at"], json!(1_700_000_000_u64));
}

/// The route needs a credential, and a device carrying a token but no
/// administrator credential is `unclaimed` rather than a claim with no channel.
#[tokio::test]
async fn the_claim_route_needs_a_credential_and_can_report_an_unclaimed_device() {
    let (tree, token) = with_token(seeded_tree());
    let (router, _fake) = test_app(tree);

    let anonymous = get(&router, CLAIM_PATH, None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(envelope(anonymous).await["code"], "not_authenticated");

    let response = bearer(&router, "GET", CLAIM_PATH, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, CLAIM_PATH);
    let status = body_json(response).await;
    assert_eq!(status["state"], json!("unclaimed"));
    assert_eq!(status["rotationRequired"], json!(false));
    assert!(status.get("via").is_none(), "{status}");
}

// --- The bound: forced rotation --------------------------------------------

/// The bound bites on every authenticated mutation, and on exactly two things
/// it must not: reads, and the password change that clears it.
///
/// That pair of exemptions is what makes the bound unable to brick a device.
/// The operator holding the bootstrap credential can always sign in, can
/// always read WHY they were refused, and can always do the one thing that
/// discharges it.
#[tokio::test]
async fn a_bootstrap_claim_refuses_every_mutation_but_the_rotation() {
    let (tree, token) = with_token(document_claimed_tree("hunter2secret"));
    let (router, _fake) = test_app(tree);

    // Reads are open, including the one that says why.
    let status = bearer(&router, "GET", CLAIM_PATH, &token).await;
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(body_json(status).await["rotationRequired"], json!(true));
    assert_eq!(
        bearer(&router, "GET", "/api/v1/settings/hostname", &token)
            .await
            .status(),
        StatusCode::OK
    );

    // Signing in and out are open: a bound that locked the operator out of
    // their own device would be the brick this design exists to avoid, and a
    // logout the device refuses would be a rule about the credential forcing a
    // live session to stay open.
    let session = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": "hunter2secret" }),
        None,
        None,
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let cookie = session_cookie_value(&session);
    let csrf = body_json(session).await["csrfToken"]
        .as_str()
        .unwrap()
        .to_string();
    let logout = json_request(
        &router,
        "DELETE",
        "/api/v1/session",
        json!({}),
        Some(&cookie),
        Some(&csrf),
    )
    .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);

    // Every mutation is refused, with the same stable code and no write.
    for (method, path, body) in [
        ("POST", "/api/v1/tokens", json!({ "name": "ci" })),
        ("PUT", "/api/v1/settings/hostname", json!("renamed")),
        ("POST", "/api/v1/actions/reboot", json!({})),
        ("PUT", "/api/v1/network", json!({})),
    ] {
        let response = bearer_json(&router, method, path, &token, &body.to_string()).await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{method} {path}");
        let envelope = envelope(response).await;
        assert_eq!(envelope["code"], "rotation_required", "{method} {path}");
        assert_eq!(envelope["path"], json!("access.claim"), "{method} {path}");
    }
}

/// The rotation discharges the bound, in ONE write, and is audited under its
/// own event name.
///
/// The write count is the assertion that matters: the new hash and the record
/// that the bootstrap secret is gone commit together, so no crash between two
/// writes can leave a device excused from a rotation that never landed.
#[tokio::test]
async fn the_rotation_discharges_the_bound_in_one_write_and_is_audited() {
    let dir = TempDir::new().unwrap();
    let (tree, token) = with_token(document_claimed_tree("hunter2secret"));
    let fake = Arc::new(FakeSettings::new(tree));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY).with_persistence(dir.path()));

    let response = bearer_json(
        &router,
        "POST",
        CHANGE_PASSWORD_PATH,
        &token,
        &json!({
            "currentPassword": "hunter2secret",
            "newPassword": PW_SENTINEL,
        })
        .to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    assert_eq!(
        fake.set_paths(),
        vec!["access".to_string()],
        "the rotation must be one write of one subtree"
    );
    let access = access_of(&fake).await;
    assert_eq!(access["claim"]["via"], json!("provisioning-document"));
    assert_eq!(access["claim"]["rotationRequired"], json!(false));
    assert_eq!(
        access["claim"]["at"],
        json!(1_700_000_000_u64),
        "rotating a credential does not move WHEN the device was claimed"
    );

    // The bound is gone: the mutation that was refused now lands.
    let mint = bearer_json(
        &router,
        "POST",
        "/api/v1/tokens",
        &token,
        &json!({ "name": "ci" }).to_string(),
    )
    .await;
    assert_eq!(mint.status(), StatusCode::CREATED);

    let events = audit_events(&audit_lines(dir.path()));
    assert!(
        events.contains(&("claim-rotation".to_string(), "completed".to_string())),
        "{events:?}"
    );
    let raw = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
    assert!(!raw.contains(PW_SENTINEL), "{raw}");
}

/// A refused mutation is audited too, under the rotation's own event name.
#[tokio::test]
async fn a_mutation_refused_for_want_of_a_rotation_is_audited() {
    let dir = TempDir::new().unwrap();
    let (tree, token) = with_token(document_claimed_tree("hunter2secret"));
    let fake = Arc::new(FakeSettings::new(tree));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY).with_persistence(dir.path()));

    let response = bearer_json(
        &router,
        "POST",
        "/api/v1/tokens",
        &token,
        &json!({ "name": "ci" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [("claim-rotation".to_string(), "refused".to_string())]
    );
    assert!(
        fake.set_paths().is_empty(),
        "a refused mutation wrote: {:?}",
        fake.set_paths()
    );
}

/// A claim by route needs no rotation, so the bound never fires on the
/// channel whose credential the caller chose.
#[tokio::test]
async fn a_route_claim_is_not_bound_by_the_rotation() {
    let (router, _fake) = test_app(seeded_tree());
    let created = post_json(&router, "/api/v1/setup", &claim_body(), None).await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let token = body_json(created).await["token"]
        .as_str()
        .unwrap()
        .to_string();

    let mint = bearer_json(
        &router,
        "POST",
        "/api/v1/tokens",
        &token,
        &json!({ "name": "ci" }).to_string(),
    )
    .await;
    assert_eq!(mint.status(), StatusCode::CREATED);
}

// --- One device, one owner --------------------------------------------------

/// **A factory-fresh device must not issue two administrator sessions to
/// concurrent claimants** — and the claimant that loses must be told so.
///
/// The claim is a check-then-act. `POST /api/v1/setup` reads `access` to
/// decide the device is still unclaimed, and writes `access` to claim it;
/// between the two sit the validators, the argon2id hash and the optional
/// hostname and network writes. micad serialises each `SetSettings` under its
/// own write lock but offers no compare-and-set, so nothing below apid made
/// that pair one step: two requests that both read an unclaimed tree both
/// wrote one, the second silently replacing the first administrator's
/// credential while apid handed each of them a session.
///
/// **Measured on the shipped path before it was fixed**, not inferred from the
/// seam below: real micad over a real private session bus, real apid over TLS,
/// two concurrent `POST /api/v1/setup` requests per iteration and no
/// instrumentation anywhere in the handler — 200 of 200 iterations issued two
/// 201s, and every session so issued read protected settings.
///
/// [`FakeSettings::hold_access_reads`] makes that interleaving deterministic
/// here by holding the first two `access` reads until both have been taken.
/// **It widens the window; it does not create one** — the window is the work
/// the route does between its own read and its own write, and it is wide
/// enough to lose without any help at all. The hold is time-bounded because an
/// atomic claim makes the second read unreachable until the first request has
/// finished: reaching that bound is the property holding, not a hung test.
#[tokio::test]
async fn a_factory_fresh_device_issues_one_administrator_session_to_concurrent_claimants() {
    const FIRST_PASSWORD: &str = "first-claimant-password";
    const SECOND_PASSWORD: &str = "second-claimant-password";

    let (router, fake) = test_app(seeded_tree());
    fake.hold_access_reads(2, std::time::Duration::from_secs(1));

    let first_body = json!({ "password": FIRST_PASSWORD }).to_string();
    let second_body = json!({ "password": SECOND_PASSWORD }).to_string();
    let (first, second) = tokio::join!(
        post_json(&router, "/api/v1/setup", &first_body, None),
        post_json(&router, "/api/v1/setup", &second_body, None),
    );

    let statuses = [first.status(), second.status()];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1,
        "A factory-fresh device must not issue two administrator sessions to \
         concurrent claimants; the two claims answered {statuses:?}"
    );

    // Which request won is the scheduler's business and not this test's; which
    // password the device ends up holding is not.
    let first_won = first.status() == StatusCode::CREATED;
    let (winner, loser) = if first_won {
        (first, second)
    } else {
        (second, first)
    };
    let (winning_password, losing_password) = if first_won {
        (FIRST_PASSWORD, SECOND_PASSWORD)
    } else {
        (SECOND_PASSWORD, FIRST_PASSWORD)
    };

    // The loser's outcome, which is the half a count of 201s does not state.
    // A status an operator can act on -- the same refusal a later claim gets,
    // naming the path and the route that changes a password -- and no session:
    // a claim that was not granted must not leave a cookie behind that reads
    // the appliance.
    assert_eq!(
        loser.status(),
        StatusCode::CONFLICT,
        "the losing claimant must be refused, not left to guess"
    );
    assert!(
        loser.headers().get(SET_COOKIE).is_none(),
        "the losing claim issued a session cookie: {:?}",
        loser.headers().get(SET_COOKIE)
    );
    let refusal = envelope(loser).await;
    assert_eq!(refusal["code"], "already_configured");
    assert_eq!(refusal["path"], "access.webAdmin");

    // One claim reached the device, and it is the winner's. The write list and
    // not the tree, for `a_route_claim_commits_...`'s reason: a tree written
    // twice looks exactly like a tree written once.
    assert_eq!(
        fake.set_paths(),
        vec!["access".to_string()],
        "a second claim reached the device"
    );
    let access = access_of(&fake).await;
    assert_eq!(
        access["apiTokens"].as_array().unwrap().len(),
        1,
        "one claim minted more than one first-run token: {access}"
    );
    assert_eq!(access["claim"]["via"], json!("setup"));

    // The winner owns the device: its session reads protected settings.
    let cookie = session_cookie_value(&winner);
    let read = get(&router, "/api/v1/settings/hostname", Some(&cookie)).await;
    assert_eq!(
        read.status(),
        StatusCode::OK,
        "the granted claim's session cannot read the device it claimed"
    );

    // And the loser owns nothing. It holds no session, so the only thing it
    // can present is none -- and the password it posted never became the
    // device's credential, which is the assertion a last-write-wins claim
    // fails even when it hands out a single cookie. The winning login goes
    // first because a failed one arms §3.3's backoff.
    let anonymous = get(&router, "/api/v1/settings/hostname", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let granted = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": winning_password }),
        None,
        None,
    )
    .await;
    assert_eq!(
        granted.status(),
        StatusCode::CREATED,
        "the credential the device kept is not the one the granted claim set"
    );
    let refused = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": losing_password }),
        None,
        None,
    )
    .await;
    assert_eq!(
        refused.status(),
        StatusCode::UNAUTHORIZED,
        "the refused claimant's password became a credential on the device"
    );
}
