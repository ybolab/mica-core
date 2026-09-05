//! The reset tiers and credential recovery: the authority each one needs, the
//! one write each one makes, and the secret that appears on exactly one
//! channel.
//!
//! What a tier DOES to the device is mosd's contract and is tested in
//! `mosd/src/reset.rs`, against §2.1's table cell for cell. What is here is
//! apid's half: who may ask, what gets committed, and what the trail records.

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;

use super::*;
use crate::routes::{Assertion, MarkerPresence, NoPresence, Presence};
use crate::settings_api::TaskNotFound;

const RESET_PATH: &str = "/api/v1/reset";
const RECOVERY_PATH: &str = "/api/v1/recovery/credential";

/// The mechanism the fixtures assert presence under.
///
/// A mechanism is a BOARD fact and neither shipped board declares one
/// (`docs/design/recovery.md` §4), so this is a fixture's name for a door no
/// mos device has — never a value read out of the tree, which would be a claim
/// the board table does not support.
const FIXTURE_MECHANISM: &str = "boot-menu";

/// What both shipped boards declare.
const DECLARES_NONE: &str = "BOARD_RECOVERY_ACTIONS=\"\"\n";

/// A board declaring one action under [`FIXTURE_MECHANISM`].
const DECLARES_ONE: &str = "\
BOARD_RECOVERY_ACTIONS=\"BOOT_MENU\"
RECOVERY_BOOT_MENU_INTENT=recovery
RECOVERY_BOOT_MENU_MECHANISM=boot-menu
RECOVERY_BOOT_MENU_CHANNEL=/dev/tty0
RECOVERY_BOOT_MENU_TIER=none
";

/// The password the fixtures claim the device with. A sentinel, so every
/// assertion that it reached nowhere is about a string nothing else produces.
const PW_SENTINEL: &str = "PW-SENTINEL-recovery-previous";

/// A presence seam a test can drive, which is the only way one is ever
/// established: an assertion is an action at the DEVICE, so no request and no
/// fixture tree can produce one.
struct FakePresence {
    /// Cleared by [`Presence::spend`], as the shipped reader's marker is
    /// unlinked: an assertion this seam has spent establishes nothing further.
    established: std::sync::atomic::AtomicBool,
    /// Everything [`Presence::publish`] was given, in order. The one place a
    /// minted credential is allowed to appear.
    published: std::sync::Mutex<Vec<String>>,
    /// When true, publication fails — §5.3's `aborted`.
    publish_fails: bool,
}

impl FakePresence {
    fn present() -> Arc<Self> {
        Arc::new(Self {
            established: std::sync::atomic::AtomicBool::new(true),
            published: std::sync::Mutex::new(Vec::new()),
            publish_fails: false,
        })
    }

    fn absent() -> Arc<Self> {
        Arc::new(Self {
            established: std::sync::atomic::AtomicBool::new(false),
            published: std::sync::Mutex::new(Vec::new()),
            publish_fails: false,
        })
    }

    fn present_but_unwritable() -> Arc<Self> {
        Arc::new(Self {
            established: std::sync::atomic::AtomicBool::new(true),
            published: std::sync::Mutex::new(Vec::new()),
            publish_fails: true,
        })
    }

    fn published(&self) -> Vec<String> {
        self.published.lock().unwrap().clone()
    }

    /// The one secret this flow published, which is the credential the
    /// operator standing at the console reads.
    fn secret(&self) -> String {
        let published = self.published();
        assert_eq!(published.len(), 1, "the credential was not published once");
        published[0].clone()
    }
}

impl Presence for FakePresence {
    fn assert(&self) -> Result<Assertion, NoPresence> {
        if self.established.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(Assertion::for_test(FIXTURE_MECHANISM))
        } else {
            Err(NoPresence::Absent)
        }
    }

    fn publish(&self, _assertion: &Assertion, secret: &str) -> anyhow::Result<()> {
        if self.publish_fails {
            return Err(anyhow::anyhow!("the console could not be written"));
        }
        self.published.lock().unwrap().push(secret.to_string());
        Ok(())
    }

    fn spend(&self) -> anyhow::Result<()> {
        self.established
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// A claimed device carrying an API token, an audit ring and a presence seam.
fn device(presence: Arc<dyn Presence>) -> (Router, Arc<FakeSettings>, TempDir, String) {
    let dir = TempDir::new().unwrap();
    let (tree, token) = with_token(claimed_tree());
    let fake = Arc::new(FakeSettings::new(tree));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY)
        .with_persistence(dir.path())
        .with_presence(presence));
    (router, fake, dir, token)
}

/// A device claimed through `POST /api/v1/setup`, with a generation to bump.
fn claimed_tree() -> serde_json::Value {
    let mut tree = configured_tree(PW_SENTINEL);
    tree["access"]["claim"] = json!({
        "via": "setup",
        "at": 1_700_000_000,
        "rotationRequired": false,
    });
    tree["access"]["device"] = json!({ "generation": 4 });
    tree["provisioning"] = json!({
        "state": "complete",
        "deviceId": "0123456789abcdef0123456789abcdef",
        "seededGeneration": 1,
    });
    tree
}

async fn access_of(fake: &FakeSettings) -> serde_json::Value {
    fake.get_settings("access").await.unwrap()
}

// --- The reset tiers -------------------------------------------------------

/// Tiers 1 and 2 are authenticated management actions, and staging one is ONE
/// write of ONE subtree.
///
/// The assertion is the write LIST and not the resulting tree: §2.2 makes the
/// intent record the whole of the commit, so "one write here" is exactly "one
/// `Store::save` on the device", and a device that lost power mid-request is
/// either not asked or asked.
#[tokio::test]
async fn an_authenticated_operator_stages_tiers_one_and_two_in_one_write() {
    for (tier, event) in [
        ("configuration", "reset-configuration"),
        ("application-data", "reset-application-data"),
    ] {
        let (router, fake, dir, token) = device(FakePresence::absent());

        let response = bearer_json(
            &router,
            "POST",
            RESET_PATH,
            &token,
            &json!({ "tier": tier }).to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{tier}");
        assert_api_headers(&response, RESET_PATH);
        let body = body_json(response).await;
        assert_eq!(body["tier"], json!(tier));
        assert_eq!(body["applies"], json!("next-boot"));

        assert_eq!(
            fake.set_paths(),
            vec!["reset".to_string()],
            "staging {tier} wrote more than the intent"
        );
        let staged = fake.get_settings("reset").await.unwrap();
        assert_eq!(staged["tier"], json!(tier));
        // Tiers 1 and 2 carry no presence: they are not presence-gated, and a
        // record claiming otherwise would put a mechanism in the trail that
        // nobody performed.
        assert_eq!(staged["presence"], json!(null));

        assert_eq!(
            audit_events(&audit_lines(dir.path())),
            [(event.to_string(), "staged".to_string())]
        );
    }
}

/// Tier 1 does not touch the management credential — not here and not on the
/// device. apid's half of that is that staging writes `reset` and nothing
/// else, so `access` is exactly as it was.
///
/// A configuration reset that dropped the credential would be a lockout
/// dressed as a settings action, and a remote credential-clearing primitive
/// (§2.1 footnote `[^cfg]`).
#[tokio::test]
async fn staging_a_configuration_reset_leaves_the_management_credential_alone() {
    let (router, fake, _dir, token) = device(FakePresence::absent());
    let before = access_of(&fake).await;

    let response = bearer_json(
        &router,
        "POST",
        RESET_PATH,
        &token,
        &json!({ "tier": "configuration" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    assert_eq!(access_of(&fake).await, before, "staging touched `access`");
    // And the credential it did not touch still authenticates.
    let probe = bearer(&router, "GET", "/api/v1/meta", &token).await;
    assert_eq!(probe.status(), StatusCode::OK);
}

/// Tier 3 is refused without presence, and the refusal writes nothing.
///
/// A credential alone is not enough: §2.2 draws the line where the operation
/// stops being self-serviceable, and a device whose identity and credentials
/// are gone cannot be handed back to its owner over the network.
#[tokio::test]
async fn a_full_factory_reset_is_refused_without_presence() {
    let (router, fake, dir, token) = device(FakePresence::absent());

    let response = bearer_json(
        &router,
        "POST",
        RESET_PATH,
        &token,
        &json!({ "tier": "full-factory" }).to_string(),
    )
    .await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(envelope(response).await["code"], "presence_required");
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [("reset-full-factory".to_string(), "refused".to_string())]
    );
}

/// With presence asserted it is staged, and the record names the mechanism
/// that authorized it.
#[tokio::test]
async fn a_full_factory_reset_is_staged_when_presence_is_asserted() {
    let (router, fake, dir, token) = device(FakePresence::present());

    let response = bearer_json(
        &router,
        "POST",
        RESET_PATH,
        &token,
        &json!({ "tier": "full-factory" }).to_string(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    assert_eq!(fake.set_paths(), vec!["reset".to_string()]);
    let staged = fake.get_settings("reset").await.unwrap();
    assert_eq!(staged["tier"], json!("full-factory"));
    assert_eq!(staged["presence"], json!(FIXTURE_MECHANISM));
    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [("reset-full-factory".to_string(), "staged".to_string())]
    );
}

/// **Tier 4 does not exist in any form.** No spelling of secure wipe is a tier
/// this device accepts, and the refusal happens before anything is written.
///
/// Asserted rather than assumed because the absence is the position: §2
/// footnote `[^wipe]` makes every cell of that row bench-dependent on a
/// device-level erase primitive no board has evidenced, and §7's answer until
/// then is to destroy the medium — not to offer a tier that cannot keep its
/// promise.
#[tokio::test]
async fn no_spelling_of_a_fourth_tier_is_accepted() {
    let (router, fake, dir, token) = device(FakePresence::present());

    for spelling in ["secure-wipe", "secureWipe", "secure_wipe", "wipe", "4"] {
        let response = bearer_json(
            &router,
            "POST",
            RESET_PATH,
            &token,
            &json!({ "tier": spelling }).to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "`{spelling}` was accepted"
        );
        assert_eq!(envelope(response).await["code"], "validation_failed");
    }
    // A parameterless reset is not a request either (§2.2).
    let bare = bearer_json(&router, "POST", RESET_PATH, &token, &json!({}).to_string()).await;
    assert_eq!(bare.status(), StatusCode::UNPROCESSABLE_ENTITY);

    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    // Nothing to audit: a body this route cannot parse names no tier, so there
    // is no event to record it under.
    assert!(!dir.path().join("audit.log").exists());
}

/// The reset route needs a credential, presence or not.
#[tokio::test]
async fn the_reset_route_refuses_an_unauthenticated_caller() {
    let (router, fake, _dir, _token) = device(FakePresence::present());

    for tier in ["configuration", "application-data", "full-factory"] {
        let response = post_json(
            &router,
            RESET_PATH,
            &json!({ "tier": tier }).to_string(),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{tier}");
        assert_eq!(envelope(response).await["code"], "not_authenticated");
    }
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

// --- Credential recovery ---------------------------------------------------

/// The flow mints, publishes once, invalidates at the same commit and bumps
/// the generation — and the commit is ONE write.
///
/// Every clause of §5.1 is one assertion below. The write count is the one
/// that makes the rest hold: the new hash, the emptied token list, the claim
/// record and the generation are four keys of one subtree, so no crash between
/// two writes can leave a device whose old credential was invalidated and
/// whose new one was not stored.
#[tokio::test]
async fn recovery_mints_publishes_once_and_invalidates_at_the_same_commit() {
    let presence = FakePresence::present();
    let (router, fake, dir, token) = device(presence.clone());
    let before = access_of(&fake).await;
    let previous_hash = before["webAdmin"]["password_hash"]
        .as_str()
        .unwrap()
        .to_string();

    let response = post_json(&router, RECOVERY_PATH, "", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_api_headers(&response, RECOVERY_PATH);
    let body = body_json(response).await;
    assert_eq!(body["mechanism"], json!(FIXTURE_MECHANISM));
    assert_eq!(body["generation"], json!(5), "the generation did not move");

    assert_eq!(
        fake.set_paths(),
        vec!["access".to_string()],
        "the rotation must be one write of one subtree"
    );
    let access = access_of(&fake).await;

    // MINTED, and the previous secret is invalidated at that same commit.
    let hash = access["webAdmin"]["password_hash"].as_str().unwrap();
    assert_ne!(hash, previous_hash, "the credential was not replaced");
    // Every API token goes with it: §3's step 5 prices exactly that.
    assert_eq!(access["apiTokens"], json!([]));
    let stale = bearer(&router, "GET", "/api/v1/meta", &token).await;
    assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);

    // The claim record says the credential of record is no longer a bootstrap
    // secret, and WHEN the device was claimed has not moved.
    assert_eq!(access["claim"]["via"], json!("setup"));
    assert_eq!(access["claim"]["rotationRequired"], json!(false));
    assert_eq!(access["claim"]["at"], json!(1_700_000_000_u64));

    // PUBLISHED exactly once, on the channel that proved presence, and it is
    // the credential that now works.
    let secret = presence.secret();
    let session = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": secret }),
        None,
        None,
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    // And the previous one does not.
    let refused = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": PW_SENTINEL }),
        None,
        None,
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    assert!(
        audit_events(&audit_lines(dir.path())).contains(&(
            "credential-recovery-boot-menu".to_string(),
            "success".to_string()
        )),
        "{:?}",
        audit_events(&audit_lines(dir.path()))
    );
}

/// Presence is the authority. Without it the flow is refused, nothing is
/// published, nothing is written, and the refusal is audited under the same
/// event name — §5.1 rule 4's "a refused attempt is the more interesting
/// record".
#[tokio::test]
async fn recovery_is_refused_without_presence_and_publishes_nothing() {
    let presence = FakePresence::absent();
    let (router, fake, dir, _token) = device(presence.clone());

    let response = post_json(&router, RECOVERY_PATH, "", None).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(envelope(response).await["code"], "presence_required");
    assert!(presence.published().is_empty());
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [("credential-recovery".to_string(), "refused".to_string())]
    );
}

/// **An authenticated session may not run it**, and holding a credential does
/// not become an authority by standing at the device: presence is asserted
/// here and the caller is still refused.
///
/// §5.2's argument, asserted: a session that can rotate the credential it
/// authenticated with is a session-fixation lever, and the operator holding a
/// working credential needs the ordinary change-password path.
#[tokio::test]
async fn an_authenticated_session_cannot_run_credential_recovery() {
    let presence = FakePresence::present();
    let (router, fake, dir, token) = device(presence.clone());
    let cookie = login(&router, PW_SENTINEL).await;

    // The bearer client.
    let by_token = bearer_json(&router, "POST", RECOVERY_PATH, &token, "").await;
    assert_eq!(by_token.status(), StatusCode::FORBIDDEN);
    let refusal = envelope(by_token).await;
    assert_eq!(refusal["code"], "authenticated_session");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|message| message.contains("change-password")),
        "{refusal}"
    );

    // The browser session.
    let by_session = post_json(&router, RECOVERY_PATH, "", Some(&cookie)).await;
    assert_eq!(by_session.status(), StatusCode::FORBIDDEN);
    assert_eq!(envelope(by_session).await["code"], "authenticated_session");

    assert!(presence.published().is_empty());
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    let events = audit_events(&audit_lines(dir.path()));
    assert_eq!(
        events
            .iter()
            .filter(|(event, outcome)| event == "credential-recovery" && outcome == "refused")
            .count(),
        2,
        "{events:?}"
    );
}

/// A device with no credential has nothing to recover, and this flow does not
/// become a third channel that can claim one.
#[tokio::test]
async fn recovery_refuses_an_unclaimed_device_and_points_at_setup() {
    let dir = TempDir::new().unwrap();
    let fake = Arc::new(FakeSettings::new(unconfigured_tree()));
    let router = app(AppState::new(fake.clone(), SIGNING_KEY)
        .with_persistence(dir.path())
        .with_presence(FakePresence::present()));

    let response = post_json(&router, RECOVERY_PATH, "", None).await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let refusal = envelope(response).await;
    assert_eq!(refusal["code"], "not_claimed");
    assert!(
        refusal["message"]
            .as_str()
            .is_some_and(|message| message.contains("/api/v1/setup")),
        "{refusal}"
    );
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
}

/// §5.4, both halves, asserted through the behaviour the guard has: a
/// SUCCESSFUL rotation clears the counters and the window, and a REFUSED one
/// clears nothing.
///
/// The observation is the login route's own answer. A guard with an armed
/// window turns a login away with 429 before it looks at the password, so
/// "the window is gone" is exactly "a correct password is admitted again" —
/// which is the release path §4.3 item 2 promises and what would make a hard
/// `lockoutThreshold` survivable.
#[tokio::test]
async fn a_successful_rotation_clears_the_login_guard_and_a_refused_one_does_not() {
    // The refused rotation first, on its own device: it must leave the armed
    // window exactly as it found it.
    let (router, _fake, dir, _token) = device(FakePresence::absent());
    let armed = arm_the_guard(&router, dir.path()).await;
    let refused = post_json(&router, RECOVERY_PATH, "", None).await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        guard_state(dir.path()),
        armed,
        "a refused rotation moved the guard: it must touch neither the failure run nor the \
         armed window"
    );

    // The successful one, on a device whose guard is armed the same way.
    let presence = FakePresence::present();
    let (router, _fake, dir, _token) = device(presence.clone());
    arm_the_guard(&router, dir.path()).await;
    let recovered = post_json(&router, RECOVERY_PATH, "", None).await;
    assert_eq!(
        recovered.status(),
        StatusCode::OK,
        "the rotation was throttled by the guard it is not subject to"
    );
    assert_eq!(
        guard_state(dir.path()),
        json!({ "failures": 0, "locked_until_unix": 0 }),
        "a successful rotation left the guard armed"
    );
    let session = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": presence.secret() }),
        None,
        None,
    )
    .await;
    assert_eq!(
        session.status(),
        StatusCode::CREATED,
        "the rotated credential was refused"
    );
}

/// The login-guard state `GuardStore` persisted, as JSON.
///
/// The guard is read here rather than probed with a second login request.
/// A probe asserts "still throttled", which is only true while the window is
/// armed — `BACKOFF_BASE` is one second, so every request that had to land
/// inside it was a race against the machine's load rather than an assertion
/// about the rotation. The file says what the guard holds, whenever it is
/// read.
fn guard_state(state_dir: &std::path::Path) -> serde_json::Value {
    let path = state_dir.join("login_guard.json");
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("the guard state at {} is unreadable: {err}", path.display()));
    serde_json::from_slice(&bytes).expect("the guard state is JSON")
}

/// Arm the login guard with a wrong password, and return the state it armed
/// so a caller can require that state to be exactly what it finds later.
async fn arm_the_guard(router: &Router, state_dir: &std::path::Path) -> serde_json::Value {
    let wrong = json_request(
        router,
        "POST",
        "/api/v1/session",
        json!({ "password": "not-the-password" }),
        None,
        None,
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let armed = guard_state(state_dir);
    assert_eq!(
        armed["failures"], 1,
        "the wrong password charged no failure, so the guard is not armed and the caller \
         asserts nothing"
    );
    assert_ne!(
        armed["locked_until_unix"], 0,
        "the charged failure armed no window, so the guard is not armed and the caller \
         asserts nothing"
    );
    armed
}

/// The publication is attempted BEFORE the commit, so a console that cannot be
/// written leaves the previous credential working and nothing on STATE.
///
/// §5.1 rule 3's direction: the alternative ordering — invalidate, then fail
/// to publish — is a self-inflicted lockout with no way back except a factory
/// reset.
#[tokio::test]
async fn a_rotation_that_cannot_publish_is_aborted_and_writes_nothing() {
    let presence = FakePresence::present_but_unwritable();
    let (router, fake, dir, token) = device(presence.clone());
    let before = access_of(&fake).await;

    let response = post_json(&router, RECOVERY_PATH, "", None).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(envelope(response).await["code"], "publish_failed");
    assert!(fake.set_paths().is_empty(), "{:?}", fake.set_paths());
    assert_eq!(access_of(&fake).await, before);
    // The previous credential still works, which is the whole point of the
    // ordering.
    let probe = bearer(&router, "GET", "/api/v1/meta", &token).await;
    assert_eq!(probe.status(), StatusCode::OK);
    assert_eq!(
        audit_events(&audit_lines(dir.path())),
        [(
            "credential-recovery-boot-menu".to_string(),
            "aborted".to_string()
        )]
    );
}

// --- Power loss, and the retry ---------------------------------------------

/// A [`SettingsApi`] that fails the FIRST write of a chosen dot-path and then
/// behaves normally — P2's `InterruptOnce`, for the same reason and modelled
/// at the same place: the commit is one `Store::save`, whose only two outcomes
/// are "landed" and "did not", and a write that does not land is one the
/// caller sees fail.
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

    async fn mark_update(&self, state: &str, slot: &str) -> anyhow::Result<(String, String)> {
        self.inner.mark_update(state, slot).await
    }

    async fn set_reboot_override(&self, seconds: u32) -> anyhow::Result<serde_json::Value> {
        self.inner.set_reboot_override(seconds).await
    }
}

/// The interrupted rotation, then the retry: nothing half-lands, the previous
/// credential still works after the interruption, and the retry leaves ONE
/// credential — the one the retry published.
#[tokio::test]
async fn an_interrupted_rotation_writes_nothing_and_the_retry_leaves_one_credential() {
    let presence = FakePresence::present();
    let api = InterruptOnce::new(claimed_tree(), "access");
    let router = app(AppState::new(api.clone(), SIGNING_KEY).with_presence(presence.clone()));
    let before = api.inner.get_settings("access").await.unwrap();

    // The attempt that does not commit.
    let interrupted = post_json(&router, RECOVERY_PATH, "", None).await;
    assert_eq!(interrupted.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        api.inner.get_settings("access").await.unwrap(),
        before,
        "an interrupted rotation left a credential behind"
    );
    // The previous credential still works, because nothing invalidated it.
    let still_works = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": PW_SENTINEL }),
        None,
        None,
    )
    .await;
    assert_eq!(still_works.status(), StatusCode::CREATED);

    // The retry, on a device the interruption left exactly as it found it.
    let response = post_json(&router, RECOVERY_PATH, "", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["generation"], json!(5));

    // Two credentials were published, and only the last one authenticates:
    // the interrupted attempt's was never stored, which is what makes the
    // ordering safe rather than merely lucky.
    let published = presence.published();
    assert_eq!(published.len(), 2, "{published:?}");
    let latest = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": published[1] }),
        None,
        None,
    )
    .await;
    assert_eq!(latest.status(), StatusCode::CREATED);
    let orphan = json_request(
        &router,
        "POST",
        "/api/v1/session",
        json!({ "password": published[0] }),
        None,
        None,
    )
    .await;
    assert_eq!(
        orphan.status(),
        StatusCode::UNAUTHORIZED,
        "the interrupted attempt's credential authenticates"
    );
}

// --- No secret anywhere but the presence channel ---------------------------

/// The minted credential appears on the presence channel and NOWHERE else.
///
/// The sentinel is the previous password, driven through every surface a
/// secret could leak into; the minted one is read back off the channel and
/// searched for in the same places. §5.1 rule 2 is what makes the response
/// body one of those places rather than the permitted appearance: the
/// credential is returned on the channel that proved presence, never over the
/// network.
#[tokio::test]
async fn no_recovery_surface_emits_a_credential() {
    let presence = FakePresence::present();
    let (router, fake, dir, _token) = device(presence.clone());

    let response = post_json(&router, RECOVERY_PATH, "", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = format!("{:?}", response.headers());
    let body = body_string(response).await;
    let minted = presence.secret();
    let hash = access_of(&fake).await["webAdmin"]["password_hash"]
        .as_str()
        .unwrap()
        .to_string();

    for haystack in [&headers, &body] {
        assert!(
            !haystack.contains(&minted),
            "the new credential: {haystack}"
        );
        assert!(!haystack.contains(PW_SENTINEL), "{haystack}");
        assert!(!haystack.contains(&hash), "{haystack}");
        assert!(!haystack.contains("argon2"), "{haystack}");
    }

    let trail = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
    assert!(!trail.contains(&minted), "{trail}");
    assert!(!trail.contains(PW_SENTINEL), "{trail}");
    assert!(!trail.contains(&hash), "{trail}");

    // Nor in the settings tree beyond the hash it became, and the claim record
    // holds nothing about the credential at all.
    let record = access_of(&fake).await["claim"].to_string();
    assert!(!record.contains(&minted), "{record}");
}

/// The refusals say nothing about the credential either, on any of the three
/// paths that can turn the flow away.
#[tokio::test]
async fn a_refused_recovery_discloses_nothing_about_the_credential() {
    let (router, fake, _dir, token) = device(FakePresence::absent());
    let hash = access_of(&fake).await["webAdmin"]["password_hash"]
        .as_str()
        .unwrap()
        .to_string();

    let no_presence = body_string(post_json(&router, RECOVERY_PATH, "", None).await).await;
    let authenticated =
        body_string(bearer_json(&router, "POST", RECOVERY_PATH, &token, "").await).await;

    for haystack in [&no_presence, &authenticated] {
        assert!(!haystack.contains(PW_SENTINEL), "{haystack}");
        assert!(!haystack.contains(&hash), "{haystack}");
        assert!(!haystack.contains("argon2"), "{haystack}");
    }
}

// --- The shipped reader, against a board's own declaration ------------------

/// A reader over a board that declares `declaration`, or over one that ships
/// none at all, with `marker` as its assertion.
fn shipped(declaration: Option<&str>, marker: Option<&str>) -> (TempDir, MarkerPresence) {
    let dir = TempDir::new().unwrap();
    let declaration_path = dir.path().join("recovery-actions.conf");
    let marker_path = dir.path().join("presence");
    if let Some(text) = declaration {
        std::fs::write(&declaration_path, text).unwrap();
    }
    if let Some(text) = marker {
        std::fs::write(&marker_path, text).unwrap();
    }
    (dir, MarkerPresence::at(marker_path, declaration_path))
}

/// A marker standing for `seconds` more.
fn marker(mechanism: &str, seconds: u64) -> String {
    let expires = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + seconds;
    json!({ "mechanism": mechanism, "channel": "/dev/tty0", "expires": expires }).to_string()
}

/// **The shipped state of both mos boards.** A board that declares no physical
/// recovery action refuses presence, and the refusal says so — an operator
/// told only "none is asserted" would go looking for a door that does not
/// exist on this hardware.
#[test]
fn a_board_that_declares_no_action_refuses_and_says_which() {
    for declaration in [Some(DECLARES_NONE), None] {
        let (_dir, presence) = shipped(declaration, None);
        let refusal = presence
            .assert()
            .err()
            .expect("presence is not established");
        assert_eq!(refusal, NoPresence::BoardDeclaresNone);
        let message = format!("{refusal:?}");
        assert_eq!(message, "BoardDeclaresNone");
    }

    // And a marker cannot buy presence on such a board: the declaration is
    // read first, so a file left in /run by anything at all reaches nothing.
    let (_dir, presence) = shipped(Some(DECLARES_NONE), Some(&marker(FIXTURE_MECHANISM, 600)));
    assert_eq!(presence.assert().err(), Some(NoPresence::BoardDeclaresNone));
}

/// Every state the shipped reader can be in, enumerated: exactly one of them
/// establishes presence, and each refusal is reached.
#[test]
fn the_shipped_reader_establishes_presence_only_for_a_declared_live_assertion() {
    /// One reader state: what it is called, what the board declares, what the
    /// marker holds, and the refusal it must produce (`None` = presence).
    struct Case {
        label: &'static str,
        declaration: Option<&'static str>,
        assertion: Option<String>,
        want: Option<NoPresence>,
    }
    fn case(
        label: &'static str,
        declaration: Option<&'static str>,
        assertion: Option<String>,
        want: Option<NoPresence>,
    ) -> Case {
        Case {
            label,
            declaration,
            assertion,
            want,
        }
    }

    let cases = [
        case(
            "a declared mechanism, still standing",
            Some(DECLARES_ONE),
            Some(marker(FIXTURE_MECHANISM, 600)),
            None,
        ),
        case(
            "no assertion at all",
            Some(DECLARES_ONE),
            None,
            Some(NoPresence::Absent),
        ),
        case(
            "an assertion whose window has passed",
            Some(DECLARES_ONE),
            Some(
                json!({
                    "mechanism": FIXTURE_MECHANISM,
                    "channel": "/dev/tty0",
                    "expires": 1_u64,
                })
                .to_string(),
            ),
            Some(NoPresence::Expired),
        ),
        case(
            "an assertion with no deadline",
            Some(DECLARES_ONE),
            Some(json!({ "mechanism": FIXTURE_MECHANISM, "channel": "/dev/tty0" }).to_string()),
            Some(NoPresence::Malformed),
        ),
        case(
            "an assertion that is not JSON",
            Some(DECLARES_ONE),
            Some("not an assertion".to_string()),
            Some(NoPresence::Malformed),
        ),
        case(
            "a mechanism no declared action uses",
            Some(DECLARES_ONE),
            Some(marker("some-other-door", 600)),
            Some(NoPresence::UnknownMechanism(vec![
                FIXTURE_MECHANISM.to_string(),
            ])),
        ),
        case(
            "a declaration this build cannot read",
            Some("BOARD_RECOVERY_ACTIONS=\"A\"\nRECOVERY_A_INTENT=recovery\n"),
            Some(marker(FIXTURE_MECHANISM, 600)),
            Some(NoPresence::DeclarationUnreadable),
        ),
        case(
            "a board that declares none",
            Some(DECLARES_NONE),
            None,
            Some(NoPresence::BoardDeclaresNone),
        ),
    ];

    let mut established = 0_usize;
    let mut refusals = std::collections::BTreeSet::new();
    for Case {
        label,
        declaration,
        assertion,
        want,
    } in cases
    {
        let (_dir, presence) = shipped(declaration, assertion.as_deref());
        match (presence.assert(), &want) {
            (Ok(_), None) => established += 1,
            (Err(got), Some(expected)) => {
                assert_eq!(&got, expected, "{label}");
                refusals.insert(format!("{got:?}"));
            }
            (got, want) => panic!("{label}: got ok={}, wanted {want:?}", got.is_ok()),
        }
    }
    assert_eq!(established, 1, "exactly one input establishes presence");
    assert_eq!(
        refusals.len(),
        6,
        "every refusal the reader can produce must be reached: {refusals:?}"
    );
}
