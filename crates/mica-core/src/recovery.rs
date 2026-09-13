//! The system side of a board-declared physical recovery action
//! (`docs/design/recovery.md` §4): read the intent at boot, map it through the
//! board's declaration, and turn it into the presence assertion and the tier
//! the existing flows already consume.
//!
//! **This module knows nothing about which mechanism produced the intent.** A
//! firmware recovery entry, a U-Boot menu selection, a button pattern, a USB event —
//! the board's BSP implements one and the board declares it; what arrives here
//! is a string on the kernel command line. Everything per-board is in the
//! declaration ([`micad_settings::Declaration`]), and nothing in this file
//! names a board, a bootloader or a console device.
//!
//! **Both shipped boards declare NONE**, so on a fielded mica device today
//! every path through here refuses and writes nothing. That is the honest
//! state rather than a stub: the flows become reachable when a board declares
//! an action AND its BSP implements it, and not before.
//!
//! **Three writes, in this order, and the order is the contract.** The
//! assertion is written FIRST, so an operator whose tier is refused later
//! still has the presence they physically established and can run credential
//! recovery with it. The tier is staged SECOND, as the same one-record intent
//! `POST /api/v1/reset` stages — [`crate::reset::apply_pending`] runs
//! immediately after this module and carries it out on this same boot, so a
//! power loss in between leaves the device asked-but-not-reset, which the next
//! boot replays. The audit line is written LAST and names the DECLARED ACTION:
//! an entry that does not name it cannot answer "how did this device get
//! reset".
//!
//! **What this module does not touch.** It does not change what any tier DOES
//! — that is [`crate::reset`], cell for cell against §2.1's table — only how
//! the intent to run one arrives. It owns no console, reconfigures no getty
//! and reads no serial device: the board's DEBUG console is not a product
//! surface, and on cx3576 displacing its `ttyFIQ0` getty was measured on
//! hardware to wedge the FIQ tty and block systemd uninterruptibly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use micad_settings::{
    ACTOR_DEVICE, Declaration, INTENT_SOURCE, NoAction, PRESENCE_WINDOW_SECS, PresenceMarker,
    RECOVERY_ACTION_EVENT, REFUSED_MALFORMED_INTENT, RecoveryAction, ResetSettings, ResetTier,
    Settings, Store, append_audit_line, audit_line, audit_ring_dir, cmdline_path, declaration_path,
    intent_from_cmdline, presence_marker_path, recovery_action_event, refusal_outcome, reset_event,
};

/// The four paths this module reads or writes, so a test can drive it without
/// a `/proc`, a `/run` or a STATE partition.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The kernel command line the recovery intent arrives on.
    pub cmdline: PathBuf,
    /// The board's declaration of its physical recovery actions.
    pub declaration: PathBuf,
    /// Where the presence assertion is left for apid to read.
    pub marker: PathBuf,
    /// The device's audit ring.
    pub audit_dir: PathBuf,
}

impl Paths {
    /// The paths this device uses, honouring every test hook.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            cmdline: cmdline_path(),
            declaration: declaration_path(),
            marker: presence_marker_path(),
            audit_dir: audit_ring_dir(),
        }
    }
}

/// What [`apply_boot_intent`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The command line carries no recovery intent. The ordinary boot.
    NoIntent,
    /// An intent arrived and mapped to nothing. Nothing was written.
    Refused(&'static str),
    /// An intent mapped to a declared action: presence is asserted, and the
    /// tier the action declares — if it declares one — is staged.
    Asserted {
        /// The board's own name for the action, which is what the audit line
        /// records.
        action: String,
        /// The mechanism the assertion carries.
        mechanism: String,
        /// The tier staged, or `None` for an action that only asserts
        /// presence.
        tier: Option<ResetTier>,
    },
}

/// Read the boot-time recovery intent and act on it.
///
/// Called before every reconciler and before [`crate::reset::apply_pending`],
/// so a tier staged here is carried out on this boot rather than the next one.
///
/// # Errors
///
/// Returns an error only when an action MAPPED and its assertion or its staged
/// tier could not be written; the caller logs it and carries on booting. A
/// refusal is a recorded outcome, not an error — a device whose operator
/// mistyped a bootloader entry must still come up.
pub fn apply_boot_intent(store: &Store, settings: &mut Settings, paths: &Paths) -> Result<Outcome> {
    let cmdline = match std::fs::read_to_string(&paths.cmdline) {
        Ok(cmdline) => cmdline,
        // No command line to read is not a refusal: it is a host that does not
        // have one, which is every test process and no device.
        Err(err) => {
            tracing::debug!(path = %paths.cmdline.display(), error = %err, "no kernel command line");
            return Ok(Outcome::NoIntent);
        }
    };

    let intent = match intent_from_cmdline(&cmdline) {
        Ok(None) => return Ok(Outcome::NoIntent),
        Ok(Some(intent)) => intent,
        Err(reason) => {
            tracing::warn!(%reason, "the recovery intent is malformed; nothing is asserted");
            record(paths, RECOVERY_ACTION_EVENT, REFUSED_MALFORMED_INTENT);
            return Ok(Outcome::Refused(REFUSED_MALFORMED_INTENT));
        }
    };

    let declaration = Declaration::read(&paths.declaration);
    let action = match declaration.map_intent(intent) {
        Ok(action) => action,
        Err(reason) => {
            let outcome = refusal_outcome(&reason);
            // The intent's own text goes to the journal and never to the
            // trail: the trail carries the enumerated reason and the surface
            // it arrived on, which is what an operator has to be able to grep.
            tracing::warn!(
                intent,
                declaration = %paths.declaration.display(),
                reason = %refusal_sentence(&reason),
                "the recovery intent mapped to no board-declared action"
            );
            record(paths, RECOVERY_ACTION_EVENT, outcome);
            return Ok(Outcome::Refused(outcome));
        }
    };

    write_assertion(&paths.marker, action).with_context(|| {
        format!(
            "write the presence assertion for `{}` to {}",
            action.name,
            paths.marker.display()
        )
    })?;

    if let Some(tier) = action.tier {
        stage_tier(store, settings, action, tier)
            .with_context(|| format!("stage {tier:?} for `{}`", action.name))?;
    }

    // LAST, and both lines name the action rather than the mechanism alone:
    // the mechanism says which kind of door, the action says which door.
    record_source(
        paths,
        &recovery_action_event(&action.mechanism),
        "success",
        &action.name,
    );
    if let Some(tier) = action.tier {
        record_source(paths, reset_event(tier), "staged", &action.name);
    }

    Ok(Outcome::Asserted {
        action: action.name.clone(),
        mechanism: action.mechanism.clone(),
        tier: action.tier,
    })
}

/// The sentence behind a refusal, for the journal.
fn refusal_sentence(reason: &NoAction) -> String {
    match reason {
        NoAction::BoardDeclaresNone => {
            "this board declares no physical recovery action".to_string()
        }
        NoAction::Unreadable(why) => why.clone(),
        NoAction::UnknownIntent => "the intent names no action this board declares".to_string(),
    }
}

/// Leave the assertion the presence reader consumes: a mechanism, a channel
/// and a deadline.
///
/// The deadline is a system constant ([`PRESENCE_WINDOW_SECS`]) measured from
/// now, not a board fact: it measures the operator's session, and how long a
/// person stands at a device is not a property of the hardware.
///
/// Mode 0600 and, being on tmpfs, gone at the next boot — so an assertion is
/// spent by the boot it was made on even if nothing consumes it.
fn write_assertion(marker: &Path, action: &RecoveryAction) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let assertion = PresenceMarker {
        mechanism: action.mechanism.clone(),
        channel: action.channel.clone(),
        expires: now_seconds().saturating_add(PRESENCE_WINDOW_SECS),
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(marker)
        .with_context(|| format!("open {}", marker.display()))?;
    file.write_all(serde_json::to_string(&assertion)?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
        .with_context(|| format!("flush {}", marker.display()))?;
    Ok(())
}

/// Stage the tier the action declares, as ONE settings record.
///
/// Deliberately the same record `POST /api/v1/reset` writes, down to the
/// `presence` member naming the mechanism that authorized it: the applier has
/// one shape to read, and a tier that arrived physically is replayable across
/// a power loss for exactly the reason an API-staged one is.
fn stage_tier(
    store: &Store,
    settings: &mut Settings,
    action: &RecoveryAction,
    tier: ResetTier,
) -> Result<()> {
    settings.reset = Some(ResetSettings {
        tier,
        requested: now_seconds(),
        presence: Some(action.mechanism.clone()),
    });
    store.save(settings).context("save the staged tier")?;
    Ok(())
}

/// One audit line whose source is the surface the intent arrived on.
fn record(paths: &Paths, event: &str, outcome: &str) {
    record_source(paths, event, outcome, INTENT_SOURCE);
}

/// One audit line.
///
/// The actor is [`ACTOR_DEVICE`]: a recovery action is asserted at the console
/// by somebody standing at the board, and what this module can honestly say is
/// that the device acted on what it found at boot — not that an authenticated
/// operator asked, which is a different claim and the one the API side makes.
///
/// A failed write is logged and swallowed, as it is on apid's side: refusing
/// to boot a device because its audit ring is unwritable is a lockdown
/// decision this design does not take (`docs/design/access.md` §6).
fn record_source(paths: &Paths, event: &str, outcome: &str, source: &str) {
    tracing::info!(target: "audit", event, outcome, source, actor = ACTOR_DEVICE, "audit event");
    if let Err(err) = std::fs::create_dir_all(&paths.audit_dir).and_then(|()| {
        append_audit_line(
            &paths.audit_dir,
            &audit_line(event, outcome, source, ACTOR_DEVICE),
        )
    }) {
        tracing::warn!(
            error = %err,
            dir = %paths.audit_dir.display(),
            "the recovery audit line could not be written"
        );
    }
}

/// The device clock in UNIX seconds, saturating at 0 before the epoch.
fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use micad_settings::{AUDIT_LOG, ProvisioningState};
    use tempfile::TempDir;

    use super::*;

    /// A board declaring both shapes an action can take: one that only asserts
    /// presence, and one that stages a tier.
    const DECLARED: &str = "\
BOARD_RECOVERY_ACTIONS=\"BOOT_MENU_RECOVERY BOOT_MENU_FACTORY\"
RECOVERY_BOOT_MENU_RECOVERY_INTENT=recovery
RECOVERY_BOOT_MENU_RECOVERY_MECHANISM=boot-menu
RECOVERY_BOOT_MENU_RECOVERY_CHANNEL=/dev/tty0
RECOVERY_BOOT_MENU_RECOVERY_TIER=none
RECOVERY_BOOT_MENU_FACTORY_INTENT=factory
RECOVERY_BOOT_MENU_FACTORY_MECHANISM=boot-menu-factory
RECOVERY_BOOT_MENU_FACTORY_CHANNEL=/dev/tty0
RECOVERY_BOOT_MENU_FACTORY_TIER=full-factory
";

    /// What both shipped mica boards declare.
    const DECLARES_NONE: &str = "BOARD_RECOVERY_ACTIONS=\"\"\n";

    struct Device {
        dir: TempDir,
        store: Store,
        settings: Settings,
        paths: Paths,
    }

    impl Device {
        /// A device whose command line carries `cmdline` and whose board
        /// declares `declaration`, or nothing at all when it is `None`.
        fn new(cmdline: &str, declaration: Option<&str>) -> Self {
            let dir = TempDir::new().unwrap();
            let root = dir.path();
            std::fs::write(root.join("cmdline"), cmdline).unwrap();
            if let Some(text) = declaration {
                std::fs::write(root.join("recovery-actions.conf"), text).unwrap();
            }
            let config = root.join("config");
            std::fs::create_dir_all(&config).unwrap();
            let store = Store::new(root.join("settings.toml"), config);
            let mut settings = Settings::default();
            settings.provisioning.state = ProvisioningState::Complete;
            store.save(&settings).unwrap();
            let paths = Paths {
                cmdline: root.join("cmdline"),
                declaration: root.join("recovery-actions.conf"),
                marker: root.join("run/presence"),
                audit_dir: root.join("audit"),
            };
            Self {
                dir,
                store,
                settings,
                paths,
            }
        }

        fn run(&mut self) -> Outcome {
            apply_boot_intent(&self.store, &mut self.settings, &self.paths).unwrap()
        }

        fn assertion(&self) -> Option<PresenceMarker> {
            let bytes = std::fs::read(&self.paths.marker).ok()?;
            Some(serde_json::from_slice(&bytes).expect("the marker is the shared shape"))
        }

        /// Every audit line, as `(event, outcome, source)`.
        fn trail(&self) -> Vec<(String, String, String)> {
            let path = self.paths.audit_dir.join(AUDIT_LOG);
            let Ok(text) = std::fs::read_to_string(path) else {
                return Vec::new();
            };
            text.lines()
                .map(|line| {
                    let value: serde_json::Value = serde_json::from_str(line).unwrap();
                    (
                        value["event"].as_str().unwrap().to_string(),
                        value["outcome"].as_str().unwrap().to_string(),
                        value["source"].as_str().unwrap().to_string(),
                    )
                })
                .collect()
        }

        /// The tier staged in the persisted tree, read back off disk rather
        /// than out of the in-memory copy.
        fn staged(&self) -> Option<ResetSettings> {
            self.store.load().unwrap().reset
        }
    }

    #[test]
    fn a_declared_action_asserts_presence_and_the_audit_names_it() {
        let mut device = Device::new("root=/dev/sda2 mica.recovery=recovery", Some(DECLARED));
        assert_eq!(
            device.run(),
            Outcome::Asserted {
                action: "BOOT_MENU_RECOVERY".to_string(),
                mechanism: "boot-menu".to_string(),
                tier: None,
            }
        );

        let assertion = device.assertion().expect("presence is asserted");
        assert_eq!(assertion.mechanism, "boot-menu");
        assert_eq!(assertion.channel, PathBuf::from("/dev/tty0"));
        assert!(
            assertion.expires > now_seconds(),
            "the assertion carries a deadline that has not passed"
        );
        assert!(
            assertion.expires <= now_seconds() + PRESENCE_WINDOW_SECS,
            "the deadline is the system window and not longer"
        );

        // An action that declares no tier stages none: presence is what it
        // produced, and nothing about the device's state changed.
        assert_eq!(device.staged(), None);
        assert_eq!(
            device.trail(),
            vec![(
                "recovery-action-boot-menu".to_string(),
                "success".to_string(),
                "BOOT_MENU_RECOVERY".to_string(),
            )],
            "the audit line names the declared action"
        );
    }

    #[test]
    fn an_action_that_declares_a_tier_stages_it_and_the_audit_names_both() {
        let mut device = Device::new("mica.recovery=factory", Some(DECLARED));
        assert_eq!(
            device.run(),
            Outcome::Asserted {
                action: "BOOT_MENU_FACTORY".to_string(),
                mechanism: "boot-menu-factory".to_string(),
                tier: Some(ResetTier::FullFactory),
            }
        );

        let staged = device.staged().expect("the tier is staged");
        assert_eq!(staged.tier, ResetTier::FullFactory);
        assert_eq!(
            staged.presence.as_deref(),
            Some("boot-menu-factory"),
            "the staged record names the mechanism that authorized it"
        );
        assert_eq!(
            device.trail(),
            vec![
                (
                    "recovery-action-boot-menu-factory".to_string(),
                    "success".to_string(),
                    "BOOT_MENU_FACTORY".to_string(),
                ),
                (
                    "reset-full-factory".to_string(),
                    "staged".to_string(),
                    "BOOT_MENU_FACTORY".to_string(),
                ),
            ],
            "the reset is recorded under the same event the API route uses, sourced to the action"
        );
    }

    /// Every way an intent can fail to produce an action, enumerated: each one
    /// writes NO assertion, stages NO tier, and leaves one audit line whose
    /// outcome says which door was closed.
    ///
    /// The refusals must all be reached and must all be distinct — a reason
    /// that stopped being reachable would otherwise be deletable with this
    /// test still green.
    #[test]
    fn every_refusal_writes_nothing_and_is_recorded_under_its_own_outcome() {
        let cases: &[(&str, &str, Option<&str>, &str)] = &[
            (
                "a board that declares none, which is both shipped boards",
                "mica.recovery=recovery",
                Some(DECLARES_NONE),
                "refused-board-declares-none",
            ),
            (
                "a board with no declaration file at all",
                "mica.recovery=recovery",
                None,
                "refused-board-declares-none",
            ),
            (
                "an intent no declared action names",
                "mica.recovery=wipe-everything",
                Some(DECLARED),
                "refused-unknown-intent",
            ),
            (
                "a declaration this build cannot read",
                "mica.recovery=recovery",
                Some("BOARD_RECOVERY_ACTIONS=\"A\"\nRECOVERY_A_INTENT=recovery\n"),
                "refused-declaration-unreadable",
            ),
            (
                "a command line naming the parameter twice",
                "mica.recovery=recovery mica.recovery=factory",
                Some(DECLARED),
                REFUSED_MALFORMED_INTENT,
            ),
            (
                "a command line naming the parameter with no value",
                "mica.recovery=",
                Some(DECLARED),
                REFUSED_MALFORMED_INTENT,
            ),
        ];

        let mut outcomes = BTreeSet::new();
        for (label, cmdline, declaration, want) in cases {
            let mut device = Device::new(cmdline, *declaration);
            assert_eq!(device.run(), Outcome::Refused(want), "{label}");
            assert!(device.assertion().is_none(), "{label} wrote an assertion");
            assert_eq!(device.staged(), None, "{label} staged a tier");
            assert_eq!(
                device.trail(),
                vec![(
                    RECOVERY_ACTION_EVENT.to_string(),
                    (*want).to_string(),
                    INTENT_SOURCE.to_string(),
                )],
                "{label}",
            );
            outcomes.insert(*want);
            drop(device.dir);
        }
        assert_eq!(
            outcomes.len(),
            4,
            "every closed door this input space can reach must be reached"
        );
    }

    #[test]
    fn a_command_line_with_no_intent_touches_nothing() {
        let mut device = Device::new("root=/dev/sda2 quiet", Some(DECLARED));
        assert_eq!(device.run(), Outcome::NoIntent);
        assert!(device.assertion().is_none());
        assert_eq!(device.staged(), None);
        assert!(
            device.trail().is_empty(),
            "an ordinary boot leaves no recovery line in the trail"
        );
    }

    #[test]
    fn a_host_with_no_kernel_command_line_is_an_ordinary_boot() {
        let mut device = Device::new("unused", Some(DECLARED));
        std::fs::remove_file(&device.paths.cmdline).unwrap();
        assert_eq!(device.run(), Outcome::NoIntent);
        assert!(device.assertion().is_none());
        assert!(device.trail().is_empty());
    }
}
