//! Reset tier execution: the applier behind `docs/design/recovery.md` §2.
//!
//! **This module is the second half of a two-writer operation.** apid checks
//! the authority §2.2 gives each tier and commits ONE record — `reset` in the
//! settings tree — and this module carries that record out on the next boot
//! and clears it. §2.2 requires exactly that shape: "a reset is therefore
//! staged as an intent record plus an idempotent apply, never as a sequence
//! whose interruption is a third state". A power loss leaves the device either
//! pre-reset (no record, nothing happened) or mid-reset (record still staged),
//! and the next boot replays the same tier until the record is gone.
//!
//! **Three tiers, and the effects are §2.1's table cell for cell.** What each
//! tier PRESERVES is as much the contract as what it clears — a reset that
//! took more than its row is this module's failure mode, so the tests below
//! assert survival and not only removal.
//!
//! Two stores this module cannot reach, by construction rather than by
//! promise:
//!
//! - **META.** §2.1 marks it `unaffected` for tiers 1 and 2 and `preserved`
//!   for tier 3, and `docs/design/access.md` §5.2 is why: the lockdown bit is
//!   one-way, and a tier that re-seeded META would be the exact software
//!   action the bit exists to exclude. There is no META path in this file.
//! - **Both system slots.** §2.1 footnote `[^slots]`: a reset resets state,
//!   not the software version. Slot vocabulary lives in one place —
//!   `validate_mark` and `rollback_eligibility` in [`crate::rauc`] — and this
//!   module names no slot, calls neither, and installs, activates or condemns
//!   nothing.
//!
//! **Identity and calibration survive every tier**, §2.1 footnote
//! `[^identity]`: `provisioning.deviceId`, the per-device secrets under the
//! state directory and `access.device` are drawn once and never re-issued, so
//! re-minting them on a serviceable device would sever every fleet-side record
//! naming it. The operation that replaces identity is the whole-disk reflash
//! (`docs/design/access.md` §9.2), which keeps ONE writer for that fact.
//!
//! **There is no tier 4.** Secure wipe is not implemented here, behind a flag
//! or at all: `mosd_settings::ResetTier` has three members, so an intent
//! naming it does not parse, and §2 footnote `[^wipe]` and §7 say why — until
//! a board evidences a device-level erase primitive, the honest operator
//! instruction is to destroy the medium.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mosd_settings::{ProvisioningState, ResetTier, Settings, Store};

/// The DATA pool root, and the variable that relocates it.
///
/// `MOS_DATA_ROOT` is `rootfs/overlay/usr/lib/mos/mos-data-layout`'s own
/// variable, read here under the same name so a test that relocates the pool
/// relocates it for the layout script and for the applier together, and so a
/// device can never have the two disagree about where `/mos` is.
pub const DATA_ROOT_ENV: &str = "MOS_DATA_ROOT";
/// Where the pool is mounted when nothing relocates it (PLAN-063 / RFCT-292:
/// `/mos` and `/srv` are binds of this ONE pool).
pub const DEFAULT_DATA_ROOT: &str = "/mnt/data";

/// The STATE partition root, and the variable that relocates it for tests.
pub const STATE_ROOT_ENV: &str = "MOSD_STATE_ROOT";
/// Where STATE is mounted when nothing relocates it (`docs/design/ro-root.md`
/// §4's bind table).
pub const DEFAULT_STATE_ROOT: &str = "/mnt/state";

/// The system-owned DATA namespace, relative to the pool root.
const SYSTEM_DIR: &str = "mos";
/// The user-owned DATA namespace, relative to the pool root.
const USER_DIR: &str = "srv";

/// The `/mos` skeleton `mos-data-layout` establishes on a virgin device, in
/// its own order.
///
/// **Read as the definition of `re-seeded`**: tier 3 empties every one of
/// these and removes anything under `/mos` that is not one of them, which
/// leaves exactly the tree a virgin device has. The directories themselves are
/// kept rather than deleted and recreated, so their declared modes are never
/// this module's to restate — `mos-data-layout` owns them and chmods them on
/// every boot.
const SYSTEM_SKELETON: &[&str] = &[
    "ui",
    "containers",
    "home",
    "root",
    "apps",
    "updates",
    "updates/downloads",
    "updates/verified",
    "updates/staging",
];

/// The `/mos` subtrees the application layer owns, which tier 2 clears.
///
/// §2.1 footnote `[^apps-mos]`: `/mos` is the system-owned namespace and tier
/// 2 does not empty it. `ui/`, `updates/` — a verified bundle is not
/// application data — and the `home/`/`root/` backing directories are not
/// opened.
const APPLICATION_DIRS: &[&str] = &["apps", "containers"];

/// The STATE directories holding application enrolment records.
///
/// §2.1 footnote `[^apps-state]`: tier 2's only write to STATE is the removal
/// of these. `quadlet` is bound over `/etc/containers/systemd`
/// (`docs/design/containers.md`) and `systemd-units` over
/// `/usr/local/lib/systemd/system` (`docs/design/ro-root.md` §4), so a unit
/// surviving the payload it starts — a boot-time failure loop — is what
/// clearing them prevents. Settings, identity, credentials and the audit trail
/// are not opened: they are not in this list.
const STATE_APPLICATION_DIRS: &[&str] = &["quadlet", "systemd-units"];

/// The roots a tier is allowed to reach. Nothing outside them is opened.
///
/// A struct and not three arguments so that the set of reachable stores is one
/// declaration a reader can check against §2.1's columns. There is no META
/// member and no slot member, which is what makes "no code path of the tier
/// opens this store" a property of the type rather than a claim about the
/// code.
#[derive(Debug, Clone)]
pub struct Roots {
    /// The DATA pool root; `/mos` and `/srv` are its two subdirectories.
    pub data: PathBuf,
    /// The STATE partition root.
    pub state: PathBuf,
}

impl Roots {
    /// The roots this device uses, honouring the two test hooks.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            data: std::env::var_os(DATA_ROOT_ENV)
                .map_or_else(|| PathBuf::from(DEFAULT_DATA_ROOT), PathBuf::from),
            state: std::env::var_os(STATE_ROOT_ENV)
                .map_or_else(|| PathBuf::from(DEFAULT_STATE_ROOT), PathBuf::from),
        }
    }

    fn system(&self) -> PathBuf {
        self.data.join(SYSTEM_DIR)
    }

    fn user(&self) -> PathBuf {
        self.data.join(USER_DIR)
    }
}

/// What [`apply_pending`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// No intent was staged; the boot carries on untouched.
    NoIntent,
    /// The staged tier was applied and the record cleared.
    Applied(ResetTier),
}

/// Apply the staged reset intent, if there is one, and clear it.
///
/// **The order is the transaction.** The filesystem work runs first and is
/// idempotent — clearing an already-cleared directory is a no-op — and the
/// settings save that clears the record runs last. A power loss before that
/// save leaves the record staged, so the next boot replays a tier that has
/// already done part of its work and completes it; a power loss after it
/// leaves a device that is fully reset. There is no ordering in which a
/// partly-applied tier looks finished.
///
/// **The save is ONE `Store::save`.** The re-seeded tree, and the absence of
/// the record that asked for it, commit together by
/// `Store::save`'s temp-write-and-rename, exactly as
/// `docs/design/provisioning.md` §4.1.3 argues for the claim.
///
/// Tiers 1 and 3 hand the seeded values back to first-boot provisioning by
/// putting `provisioning.state` back to `Pending`: `provisioning::ensure_provisioned`
/// runs later in the same boot and re-derives the hostname from the PRESERVED
/// device identity and the SSH default from the image profile. That is §2.1's
/// `re-seeded` read literally — "recreated by the code that creates them on a
/// virgin device" — rather than a second copy of the seeding rules here. Its
/// save is first-boot provisioning's own, and losing power between the two
/// leaves a reset device that provisions itself on the next boot, which is
/// what a virgin device does anyway.
///
/// # Errors
///
/// Returns an error when the tier cannot complete its own scope — §2.2's "a
/// tier never widens under failure": the record stays staged, nothing is
/// re-seeded, and the next boot retries the same tier. It never falls back to
/// a different one.
pub fn apply_pending(store: &Store, settings: &mut Settings, roots: &Roots) -> Result<Outcome> {
    let Some(intent) = settings.reset.clone() else {
        return Ok(Outcome::NoIntent);
    };
    tracing::info!(
        tier = ?intent.tier,
        requested = intent.requested,
        presence = intent.presence.as_deref().unwrap_or("none"),
        "applying a staged reset"
    );

    match intent.tier {
        ResetTier::Configuration => {}
        ResetTier::ApplicationData => clear_application_state(roots)?,
        ResetTier::FullFactory => {
            clear_application_state(roots)?;
            reseed_tree(&roots.system(), SYSTEM_SKELETON)
                .with_context(|| format!("re-seed {}", roots.system().display()))?;
        }
    }

    // The commit. Built from the tree in hand and saved once; `*settings` is
    // replaced only after the save has returned, so no caller observes a tree
    // that is not on STATE.
    let mut applied = match intent.tier {
        ResetTier::Configuration | ResetTier::FullFactory => {
            reseeded_settings(settings, intent.tier)
        }
        ResetTier::ApplicationData => settings.clone(),
    };
    applied.reset = None;
    store
        .save(&applied)
        .context("persist the tree the reset produced")?;
    *settings = applied;

    tracing::info!(tier = ?intent.tier, "reset applied");
    Ok(Outcome::Applied(intent.tier))
}

/// The tree a settings-clearing tier leaves behind.
///
/// **Every field this function names is a §2.1 cell.** The defaults come from
/// `Settings::default()`, so a subtree added to the schema later is re-seeded
/// by tier 1 without an edit here; what survives has to be named, which is the
/// direction that fails safe — a new credential-bearing subtree is cleared by
/// default rather than silently kept.
fn reseeded_settings(before: &Settings, tier: ResetTier) -> Settings {
    let mut after = Settings {
        // The identity record, PRESERVED by both tiers (§2.1 footnote
        // `[^identity]`), with `state` put back so first-boot provisioning
        // re-seeds the hostname from it and the SSH default from the profile.
        provisioning: mosd_settings::ProvisioningSettings {
            state: ProvisioningState::Pending,
            ..before.provisioning.clone()
        },
        ..Settings::default()
    };
    // The per-device credential and its generation: a first-boot secret, not a
    // management credential, and `identity::ensure_identity` would re-mint it
    // if it were dropped. §2.1's `identity: preserved` covers it on both tiers.
    after.access.device = before.access.device.clone();

    if tier == ResetTier::Configuration {
        // §2.1 footnote `[^cfg]`: tier 1 keeps the apid management credential.
        // A configuration reset that dropped it would be a lockout dressed as
        // a settings action, and a remote credential-clearing primitive —
        // `docs/design/recovery.md` §5 is where a credential is deliberately
        // replaced, under §4's authority and nowhere else.
        after.access.web_admin = before.access.web_admin.clone();
        after.access.claim = before.access.claim;
        after.access.api_tokens = before.access.api_tokens.clone();
    }
    after
}

/// Tier 2's whole scope, and tier 3's share of it: the application layer's
/// enrolment records on STATE, its subtrees under `/mos`, and `/srv`.
fn clear_application_state(roots: &Roots) -> Result<()> {
    for dir in STATE_APPLICATION_DIRS {
        let path = roots.state.join(dir);
        clear_contents(&path).with_context(|| format!("clear {}", path.display()))?;
    }
    for dir in APPLICATION_DIRS {
        let path = roots.system().join(dir);
        clear_contents(&path).with_context(|| format!("clear {}", path.display()))?;
    }
    // §2.1 marks `/srv` `cleared` and not `re-seeded`, footnote `[^apps-mos]`:
    // the product gives that namespace to the operator, so mos recreates the
    // mount point and never its contents. Clearing the contents and keeping
    // the directory is exactly that.
    let user = roots.user();
    clear_contents(&user).with_context(|| format!("clear {}", user.display()))
}

/// Remove every entry inside `dir`, keeping `dir` itself.
///
/// Idempotent, which is what makes a replayed tier safe: an absent directory
/// and an already-empty one are both success. Entries are classified by
/// [`fs::DirEntry::file_type`], which does NOT follow symlinks, so a symlink
/// planted in a cleared tree is unlinked rather than followed into a store the
/// tier has no business opening.
fn clear_contents(dir: &Path) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
        remove(&entry.path(), &entry.file_type()?)?;
    }
    Ok(())
}

/// Reduce `root` to exactly `skeleton`, with every directory in it empty.
///
/// The declared paths are kept and descended into; everything else is removed.
/// Descending rather than deleting the whole tree keeps the modes
/// `mos-data-layout` established, so this module never restates them.
fn reseed_tree(root: &Path, skeleton: &[&str]) -> Result<()> {
    let declared: BTreeSet<PathBuf> = skeleton.iter().map(PathBuf::from).collect();
    reseed_dir(root, Path::new(""), &declared)
}

fn reseed_dir(dir: &Path, relative: &Path, declared: &BTreeSet<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
        let file_type = entry.file_type()?;
        let child = relative.join(entry.file_name());
        // A declared path is kept only when it is a real directory: a file or
        // a symlink standing where a skeleton directory belongs is not that
        // directory, and leaving it would leave the pretence of a re-seeded
        // tree.
        if file_type.is_dir() && declared.contains(&child) {
            reseed_dir(&entry.path(), &child, declared)?;
        } else {
            remove(&entry.path(), &file_type)?;
        }
    }
    Ok(())
}

fn remove(path: &Path, file_type: &fs::FileType) -> Result<()> {
    let removed = if file_type.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use mosd_settings::{
        ApiToken, ClaimChannel, ClaimSettings, DeviceCredentialSettings, ProvisioningSettings,
        ResetSettings, WebAdminSettings,
    };
    use tempfile::TempDir;

    use super::*;

    /// The sentinel credential. Nothing else in this file can produce it, so
    /// an assertion that it survived is an assertion about this value and not
    /// about a shape that happens to match.
    const ADMIN_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$UkVTRVQ$UkVTRVRIQVNI";
    const DEVICE_ID: &str = "0123456789abcdef0123456789abcdef";

    /// A device as a fielded one is: claimed, named after its identity, with
    /// operator settings in every subtree the schema has.
    fn fielded_settings() -> Settings {
        let mut settings = Settings {
            hostname: "edge-42".to_string(),
            provisioning: ProvisioningSettings {
                state: ProvisioningState::Complete,
                device_id: Some(DEVICE_ID.to_string()),
                seeded_generation: 1,
                document: None,
            },
            ..Settings::default()
        };
        settings.access.web_admin = Some(WebAdminSettings {
            password_hash: ADMIN_HASH.to_string(),
        });
        settings.access.claim = Some(ClaimSettings {
            via: ClaimChannel::Setup,
            at: 1_700_000_000,
            rotation_required: false,
        });
        settings.access.api_tokens = vec![ApiToken {
            id: "3f2a9c41".to_string(),
            name: "ci".to_string(),
            hash: "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            created: 1_700_000_000,
        }];
        settings.access.device = DeviceCredentialSettings {
            password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2aGFzaA".to_string()),
            generation: 3,
        };
        settings.access.ssh.enabled = true;
        settings.access.ssh.port = 2222;
        settings.container.enabled = true;
        settings.time.timezone = "Europe/Berlin".to_string();
        settings
    }

    /// A pool and a STATE partition populated the way a running device
    /// populates them: an application payload, a verified update bundle, a
    /// custom UI, operator data in `/srv`, and the enrolment records on STATE.
    fn populated_roots() -> (TempDir, Roots) {
        let dir = TempDir::new().unwrap();
        let roots = Roots {
            data: dir.path().join("data"),
            state: dir.path().join("state"),
        };
        for relative in SYSTEM_SKELETON {
            fs::create_dir_all(roots.system().join(relative)).unwrap();
        }
        fs::create_dir_all(roots.user()).unwrap();
        for dir in STATE_APPLICATION_DIRS {
            fs::create_dir_all(roots.state.join(dir)).unwrap();
        }
        // The settings store and the per-device secrets live on STATE too, and
        // no tier may reach them.
        fs::create_dir_all(roots.state.join("mos/secrets")).unwrap();

        write(&roots.system().join("apps/inventory/db.sqlite"), "app data");
        write(&roots.system().join("containers/overlay/layer"), "layer");
        write(&roots.system().join("ui/active/index.html"), "custom ui");
        write(&roots.system().join("updates/verified/os.raucb"), "bundle");
        write(&roots.system().join("home/operator/.profile"), "profile");
        write(&roots.system().join("root/.ssh/known_hosts"), "hosts");
        write(&roots.user().join("operator/report.csv"), "operator data");
        write(&roots.state.join("quadlet/web.container"), "unit");
        write(&roots.state.join("systemd-units/vendor.service"), "unit");
        write(&roots.state.join("mos/secrets/device-password"), "secret");
        (dir, roots)
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn exists(path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }

    /// Every path a tier may NOT have opened, with what it must still hold.
    ///
    /// Called by all three tier tests: META and the slots are outside these
    /// roots entirely, and these are the stores inside them that survive every
    /// row of §2.1.
    fn assert_identity_survived(roots: &Roots) {
        assert_eq!(
            fs::read_to_string(roots.state.join("mos/secrets/device-password")).unwrap(),
            "secret",
            "a tier reached the per-device secrets"
        );
    }

    fn store_at(dir: &TempDir) -> Store {
        Store::new(dir.path().join("settings.toml"))
    }

    fn stage(settings: &mut Settings, tier: ResetTier) {
        settings.reset = Some(ResetSettings {
            tier,
            requested: 1_700_000_000,
            presence: (tier == ResetTier::FullFactory).then(|| "console-attach".to_string()),
        });
    }

    /// No record, no reset: a boot with nothing staged writes nothing at all.
    ///
    /// The positive control for every test below — without it, an applier that
    /// never ran would pass the survival assertions vacuously.
    #[test]
    fn a_boot_with_no_intent_staged_changes_nothing() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let mut settings = fielded_settings();
        let before = settings.clone();

        assert_eq!(
            apply_pending(&store, &mut settings, &roots).unwrap(),
            Outcome::NoIntent
        );

        assert_eq!(settings, before);
        assert!(!dir.path().join("settings.toml").exists(), "it saved");
        assert!(exists(&roots.system().join("apps/inventory/db.sqlite")));
        assert!(exists(&roots.user().join("operator/report.csv")));
    }

    /// Tier 1, §2.1 row 1: STATE re-seeded, everything else in reach
    /// preserved, and the management credential above all.
    #[test]
    fn tier_one_reseeds_the_settings_and_keeps_the_management_credential() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let mut settings = fielded_settings();
        stage(&mut settings, ResetTier::Configuration);

        assert_eq!(
            apply_pending(&store, &mut settings, &roots).unwrap(),
            Outcome::Applied(ResetTier::Configuration)
        );

        // CLEARED: every modelled setting, at once. There is no per-subtree
        // reset, so the assertion is over subtrees the operator touched.
        assert_eq!(settings.hostname, Settings::default().hostname);
        assert_eq!(settings.access.ssh, Default::default());
        assert_eq!(settings.container, Default::default());
        assert_eq!(settings.time, Default::default());

        // PRESERVED, and this is the row that a reset clearing more than its
        // scope would break: the credential, the claim record, the tokens, the
        // per-device secret and the identity.
        assert_eq!(
            settings.access.web_admin.as_ref().unwrap().password_hash,
            ADMIN_HASH
        );
        assert_eq!(settings.access.claim.unwrap().via, ClaimChannel::Setup);
        assert_eq!(settings.access.api_tokens.len(), 1);
        assert_eq!(settings.access.device.generation, 3);
        assert_eq!(settings.provisioning.device_id.as_deref(), Some(DEVICE_ID));
        // Handed back to first-boot provisioning, which re-derives the
        // hostname from the identity above.
        assert_eq!(settings.provisioning.state, ProvisioningState::Pending);

        // UNAFFECTED: tier 1 opens no DATA store and no STATE directory but
        // the settings file.
        assert!(exists(&roots.system().join("apps/inventory/db.sqlite")));
        assert!(exists(&roots.system().join("containers/overlay/layer")));
        assert!(exists(&roots.user().join("operator/report.csv")));
        assert!(exists(&roots.state.join("quadlet/web.container")));
        assert_identity_survived(&roots);

        // The record is gone, and the tree on STATE is the tree in hand.
        assert_eq!(settings.reset, None);
        assert_eq!(store.load().unwrap(), settings);
    }

    /// Tier 2, §2.1 row 2: the application layer goes and nothing else does —
    /// not the platform's settings, not its credentials, not `/mos/ui`, not a
    /// verified bundle.
    #[test]
    fn tier_two_clears_the_application_layer_and_keeps_the_platform() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let mut settings = fielded_settings();
        let before_settings = Settings {
            reset: None,
            ..settings.clone()
        };
        stage(&mut settings, ResetTier::ApplicationData);

        assert_eq!(
            apply_pending(&store, &mut settings, &roots).unwrap(),
            Outcome::Applied(ResetTier::ApplicationData)
        );

        // CLEARED: the application subtrees of `/mos`, all of `/srv`, and the
        // enrolment records on STATE.
        assert!(!exists(&roots.system().join("apps/inventory")));
        assert!(!exists(&roots.system().join("containers/overlay")));
        assert!(!exists(&roots.user().join("operator")));
        assert!(!exists(&roots.state.join("quadlet/web.container")));
        assert!(!exists(&roots.state.join("systemd-units/vendor.service")));

        // RE-SEEDED and not deleted: the directories are still there at the
        // modes `mos-data-layout` gave them, so nothing that writes into them
        // has to wait for the next boot.
        for relative in APPLICATION_DIRS {
            assert!(roots.system().join(relative).is_dir(), "{relative}");
        }
        assert!(roots.user().is_dir(), "the /srv mount point was removed");

        // PRESERVED: `/mos` is the system-owned namespace and tier 2 does not
        // empty it — a verified bundle is not application data.
        assert_eq!(
            fs::read_to_string(roots.system().join("updates/verified/os.raucb")).unwrap(),
            "bundle"
        );
        assert!(exists(&roots.system().join("ui/active/index.html")));
        assert!(exists(&roots.system().join("home/operator/.profile")));
        assert!(exists(&roots.system().join("root/.ssh/known_hosts")));
        assert_identity_survived(&roots);

        // PRESERVED: the whole settings tree, credential included. The only
        // write is the record's removal.
        assert_eq!(settings, before_settings);
        assert_eq!(store.load().unwrap(), before_settings);
    }

    /// Tier 3, §2.1 row 3: the whole mutable state goes; identity,
    /// calibration and the per-device secrets do not.
    #[test]
    fn tier_three_returns_the_device_to_first_boot_and_keeps_its_identity() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let mut settings = fielded_settings();
        stage(&mut settings, ResetTier::FullFactory);

        assert_eq!(
            apply_pending(&store, &mut settings, &roots).unwrap(),
            Outcome::Applied(ResetTier::FullFactory)
        );

        // CLEARED: settings, management credentials, applications and all
        // operator data, together.
        assert_eq!(settings.hostname, Settings::default().hostname);
        assert_eq!(settings.access.web_admin, None);
        assert_eq!(settings.access.claim, None);
        assert!(settings.access.api_tokens.is_empty());
        assert_eq!(settings.access.ssh, Default::default());
        assert!(!exists(&roots.system().join("apps/inventory")));
        assert!(!exists(&roots.system().join("ui/active")));
        assert!(!exists(&roots.system().join("updates/verified/os.raucb")));
        assert!(!exists(&roots.system().join("home/operator")));
        assert!(!exists(&roots.user().join("operator")));
        assert!(!exists(&roots.state.join("quadlet/web.container")));

        // RE-SEEDED: `/mos` is exactly the skeleton a virgin device has, every
        // directory present and every one of them empty.
        for relative in SYSTEM_SKELETON {
            let path = roots.system().join(relative);
            assert!(path.is_dir(), "{relative} is not a directory");
        }
        assert_eq!(
            fs::read_dir(roots.system().join("apps")).unwrap().count(),
            0
        );
        assert_eq!(
            fs::read_dir(roots.system().join("updates/verified"))
                .unwrap()
                .count(),
            0
        );

        // PRESERVED: identity, the per-device credential, and the secrets on
        // STATE. A full factory reset does not re-mint identity — the
        // operation that does is the whole-disk reflash.
        assert_eq!(settings.provisioning.device_id.as_deref(), Some(DEVICE_ID));
        assert_eq!(settings.access.device.generation, 3);
        assert_eq!(
            settings.access.device.password_hash,
            fielded_settings().access.device.password_hash
        );
        assert_identity_survived(&roots);
        assert_eq!(settings.provisioning.state, ProvisioningState::Pending);

        assert_eq!(settings.reset, None);
        assert_eq!(store.load().unwrap(), settings);
    }

    /// The tier that has to survive an interruption, driven the way the record
    /// makes it survivable: the filesystem work is done and the commit has not
    /// landed, so the record is still staged and a replay finishes the job.
    ///
    /// The second run is the assertion: it must be a no-op over an
    /// already-cleared tree rather than an error, and it must leave the same
    /// device the uninterrupted path leaves.
    #[test]
    fn an_interrupted_tier_replays_to_the_same_device() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);

        // The interruption, modelled where it is observable: the tier's
        // filesystem work ran, the save did not, so the record is untouched on
        // the tree the next boot loads.
        let mut interrupted = fielded_settings();
        stage(&mut interrupted, ResetTier::FullFactory);
        clear_application_state(&roots).unwrap();
        reseed_tree(&roots.system(), SYSTEM_SKELETON).unwrap();
        assert!(
            interrupted.reset.is_some(),
            "the record must survive the interruption"
        );

        // The replay, over a tree half of the work has already been done to.
        assert_eq!(
            apply_pending(&store, &mut interrupted, &roots).unwrap(),
            Outcome::Applied(ResetTier::FullFactory)
        );

        // And it is the device the uninterrupted path produces.
        let (other_dir, other_roots) = populated_roots();
        let other_store = store_at(&other_dir);
        let mut uninterrupted = fielded_settings();
        stage(&mut uninterrupted, ResetTier::FullFactory);
        apply_pending(&other_store, &mut uninterrupted, &other_roots).unwrap();
        assert_eq!(interrupted, uninterrupted);
    }

    /// Running the same tier twice over its own output changes nothing the
    /// second time. Idempotence is what makes "re-run the tier" a safe
    /// instruction after a power loss.
    #[test]
    fn a_tier_applied_over_its_own_output_is_a_no_op() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let mut settings = fielded_settings();
        stage(&mut settings, ResetTier::ApplicationData);
        apply_pending(&store, &mut settings, &roots).unwrap();
        let once = settings.clone();

        stage(&mut settings, ResetTier::ApplicationData);
        apply_pending(&store, &mut settings, &roots).unwrap();

        assert_eq!(settings, once);
    }

    /// A symlink planted in a cleared tree is unlinked, never followed: the
    /// target survives, so a tier cannot be steered into a store §2.1 marks
    /// `unaffected`.
    #[test]
    fn a_symlink_in_a_cleared_tree_is_unlinked_rather_than_followed() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("meta"), "not this tier's").unwrap();
        std::os::unix::fs::symlink(&outside, roots.user().join("escape")).unwrap();

        let mut settings = fielded_settings();
        stage(&mut settings, ResetTier::ApplicationData);
        apply_pending(&store, &mut settings, &roots).unwrap();

        assert!(!exists(&roots.user().join("escape")));
        assert_eq!(
            fs::read_to_string(outside.join("meta")).unwrap(),
            "not this tier's",
            "the tier followed a symlink out of its own roots"
        );
    }

    /// A tier that cannot complete its own scope fails and leaves the record
    /// staged; it does not widen to the next tier and it does not re-seed.
    ///
    /// Driven by putting a regular file where `/srv` belongs — the shape a
    /// pool that did not mount leaves, and the one `mos-data-layout` refuses
    /// to work with for the same reason. The clear cannot be enumerated, so
    /// the tier has not done its work and must say so.
    #[test]
    fn a_tier_that_cannot_finish_leaves_the_intent_staged() {
        let (dir, roots) = populated_roots();
        let store = store_at(&dir);
        let mut settings = fielded_settings();
        stage(&mut settings, ResetTier::ApplicationData);
        let before = settings.clone();
        fs::remove_dir_all(roots.user()).unwrap();
        fs::write(roots.user(), "not a directory").unwrap();

        let err = apply_pending(&store, &mut settings, &roots).unwrap_err();

        assert!(format!("{err:#}").contains("srv"), "{err:#}");
        assert_eq!(settings, before, "a failed tier changed the tree");
        assert!(settings.reset.is_some(), "the retry has nothing to replay");
        assert!(
            !dir.path().join("settings.toml").exists(),
            "a failed tier committed"
        );
        // And it did not widen: the STATE and `/mos` work its own row calls
        // for ran, but nothing outside that row was touched.
        assert!(exists(&roots.system().join("updates/verified/os.raucb")));
        assert_identity_survived(&roots);
    }
}
