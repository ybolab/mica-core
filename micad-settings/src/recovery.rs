//! Board-declared physical recovery actions: the ONE interface the system
//! layer reserves for them (`docs/design/recovery.md` §4).
//!
//! **The physical action is a BOARD fact and this module knows nothing about
//! it.** A GRUB menu entry, a U-Boot menu selection, a button pattern held
//! across a power cycle, a USB event — whichever a board has, its BSP
//! implements it and the board DECLARES it. What crosses into the system layer
//! is one string, the *recovery intent*, placed on the kernel command line by
//! whatever ran before Linux. This module reads that intent, maps it through
//! the board's declaration, and hands back the action the board named. It
//! never learns which mechanism produced the intent, and nothing here is
//! per-board.
//!
//! **Both shipped boards declare NONE**, which is the honest state and not a
//! placeholder: neither has an implemented action ([`Declaration::None`]), so
//! every mapping refuses and the presence-gated flows refuse with it. A
//! refusal on such a board says the board declares none rather than that
//! presence is merely absent — the two send an operator to different places.
//!
//! **Fail closed, everywhere.** An intent naming no declared action, a
//! declaration this build cannot read, a command line carrying the parameter
//! twice: each of them maps to nothing. The failure mode this ordering exists
//! to exclude is a malformed declaration that opens a door instead of closing
//! one.
//!
//! This crate is the one both binaries link, which is why the declaration
//! lives here: micad MAPS an intent and writes the presence assertion, apid
//! READS the assertion and has to be able to say what the board declares when
//! it refuses. Two copies of this grammar would be two answers to one
//! question.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::model::ResetTier;

/// Where a board's declaration is read from on the device.
///
/// **Absent means the board declares none.** A board with no implemented
/// physical action ships no declaration, which is the state both mos boards
/// are in; a board whose BSP implements one renders exactly the
/// `BOARD_RECOVERY_ACTIONS` / `RECOVERY_*` lines of its `boards/<board>/board.env`
/// here, so the file on the device is a subset of the board definition the
/// layout lint already holds to this schema rather than a second statement of
/// it.
pub const DEFAULT_DECLARATION_PATH: &str = "/usr/lib/mica/recovery-actions.conf";

/// Test hook relocating [`DEFAULT_DECLARATION_PATH`].
pub const DECLARATION_PATH_ENV: &str = "MOS_RECOVERY_DECLARATION_PATH";

/// Where the recovery intent is read from.
pub const DEFAULT_CMDLINE_PATH: &str = "/proc/cmdline";

/// Test hook relocating [`DEFAULT_CMDLINE_PATH`].
pub const CMDLINE_PATH_ENV: &str = "MOS_RECOVERY_CMDLINE_PATH";

/// The kernel command-line parameter a board's mechanism sets.
///
/// `mos.recovery=<intent>`. A bootloader menu entry that appends it is the
/// industry-standard shape this interface expects; nothing bespoke is invented
/// at the system layer, and a board free to choose its own parameter would
/// make the system layer per-board.
pub const INTENT_PARAMETER: &str = "mos.recovery";

/// The board key listing the actions a board implements.
pub const ACTIONS_KEY: &str = "BOARD_RECOVERY_ACTIONS";

/// How long a presence assertion produced from a boot-time action stands.
///
/// The reader requires a deadline and refuses an assertion without one
/// (`docs/design/recovery.md` §4), because a visit to the device must not
/// become a standing permission. Fifteen minutes is a system constant and not
/// a board fact: it measures the operator's session — long enough to reach the
/// device's API from a laptop on the same bench and re-run a flow that failed,
/// short enough that a device left booted in recovery is not indefinitely
/// open. A board that wanted its own window would be declaring how long its
/// operator stands still, which is not a property of the hardware.
pub const PRESENCE_WINDOW_SECS: u64 = 15 * 60;

/// The tier value a declared action uses when it authorizes presence and
/// stages no reset.
pub const TIER_NONE: &str = "none";

/// Where the presence assertion this interface produces is left.
///
/// **One constant, two daemons.** micad WRITES it after mapping an intent
/// through the board's declaration; apid only ever READS it, and there is no
/// route, no settings path and no line in apid that creates it. That asymmetry
/// is what keeps `docs/design/recovery.md` §4's rule literal — the action must
/// be one no network client can perform — so an API that could set it would
/// have to be written first.
///
/// On tmpfs and owned by root, so the assertion does not survive the boot the
/// operator made it on.
pub const DEFAULT_PRESENCE_MARKER_PATH: &str = "/run/mica/presence";

/// Test hook relocating [`DEFAULT_PRESENCE_MARKER_PATH`].
pub const PRESENCE_MARKER_PATH_ENV: &str = "MOS_PRESENCE_MARKER_PATH";

/// The audit event a mapped recovery action is recorded under, before the
/// mechanism is appended: see [`recovery_action_event`].
///
/// Used bare for a REFUSAL, where there is no mechanism to name because
/// nothing mapped.
pub const RECOVERY_ACTION_EVENT: &str = "recovery-action";

/// The outcome recorded when a recovery intent mapped to nothing.
///
/// One enumerated outcome per closed door, so "why did this device not enter
/// recovery" is greppable without widening the four-member line shape
/// (`docs/design/recovery.md` §5.3).
#[must_use]
pub fn refusal_outcome(reason: &NoAction) -> &'static str {
    match reason {
        NoAction::BoardDeclaresNone => "refused-board-declares-none",
        NoAction::Unreadable(_) => "refused-declaration-unreadable",
        NoAction::UnknownIntent => "refused-unknown-intent",
    }
}

/// The outcome recorded when the command line itself was malformed, so no
/// intent was read at all.
pub const REFUSED_MALFORMED_INTENT: &str = "refused-malformed-intent";

/// The source recorded for a refused intent: the local surface it arrived on,
/// never the intent's own text, which is not this trail's to carry.
pub const INTENT_SOURCE: &str = "cmdline";

/// The audit event naming a mechanism that asserted presence at boot.
#[must_use]
pub fn recovery_action_event(mechanism: &str) -> String {
    format!("{RECOVERY_ACTION_EVENT}-{mechanism}")
}

/// The audit event a credential recovery is recorded under
/// (`docs/design/recovery.md` §5.3): the flow and the presence mechanism, so
/// "which door was used" is greppable.
///
/// Derived from the board's declaration rather than enumerated in code. The
/// enumeration used to be the honest shape — a constant for a door no board
/// could open would have been a claim the board table did not support — and
/// now the board table is what names the doors, so the event follows it.
#[must_use]
pub fn credential_recovery_event(mechanism: &str) -> String {
    format!("credential-recovery-{mechanism}")
}

/// The audit event a staged reset tier is recorded under, whichever half of
/// the system staged it.
///
/// One name per tier, shared by the API route that stages one and by the
/// boot-time action that stages one, so an operator greps a device's history
/// for `reset-full-factory` and finds both.
#[must_use]
pub fn reset_event(tier: ResetTier) -> &'static str {
    match tier {
        ResetTier::Configuration => "reset-configuration",
        ResetTier::ApplicationData => "reset-application-data",
        ResetTier::FullFactory => "reset-full-factory",
    }
}

/// What [`DEFAULT_PRESENCE_MARKER_PATH`] holds.
///
/// Three members and no room for a fourth: an assertion is a mechanism, a
/// channel and a deadline. Declared once and serialized by micad, deserialized
/// by apid, so the writer and the reader cannot come to disagree about the
/// shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PresenceMarker {
    /// The mechanism asserted, which must be one the board declares.
    pub mechanism: String,
    /// The device the operator is attached to, and therefore the ONE channel a
    /// minted credential may be published on.
    pub channel: PathBuf,
    /// UNIX seconds at which the assertion stops standing.
    ///
    /// **Required, and an assertion without one does not parse.** A marker
    /// with no deadline would turn one visit to the device into a standing
    /// permission, which is the permanent shell `docs/design/recovery.md` §4.3
    /// refuses.
    pub expires: u64,
}

/// The presence marker path this device uses, honouring the test hook.
#[must_use]
pub fn presence_marker_path() -> PathBuf {
    std::env::var_os(PRESENCE_MARKER_PATH_ENV).map_or_else(
        || PathBuf::from(DEFAULT_PRESENCE_MARKER_PATH),
        PathBuf::from,
    )
}

/// One physical recovery action a board declares.
///
/// Four facts and no fifth: what the mechanism puts on the command line, what
/// the assertion is called, where a minted credential may be published, and
/// which reset tier the action stages. Everything the system does with an
/// action is one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryAction {
    /// The board's own name for the action, as it appears in
    /// [`ACTIONS_KEY`] — `GRUB_RECOVERY_ENTRY`, `UBOOT_MENU`. This is what the
    /// audit trail records, because "which declared action produced it" is the
    /// question an audit of a reset has to answer.
    pub name: String,
    /// The `mos.recovery=` value the board's mechanism sets.
    pub intent: String,
    /// What the presence assertion is called: the mechanism the reader
    /// validates and the suffix the audit event carries.
    pub mechanism: String,
    /// The device the operator is attached to, and therefore the ONE channel a
    /// minted credential may be published on.
    ///
    /// A board must NOT declare its debug console here. That console is not a
    /// product surface: on cx3576 displacing the `ttyFIQ0` getty was measured
    /// on hardware to wedge the FIQ tty and block systemd uninterruptibly, so a
    /// board that declared it would be building a product flow on a surface
    /// nothing may own.
    pub channel: PathBuf,
    /// The tier this action stages, or `None` when it only asserts presence.
    ///
    /// `None` is the credential-recovery shape: the operator proves presence
    /// at boot, rotates the management credential, and decides afterwards
    /// whether anything destructive is wanted.
    pub tier: Option<ResetTier>,
}

/// What a board declares, read as a total function of the file.
///
/// Three states and not two: a board that declares nothing and a declaration
/// this build cannot read both map no intent, and telling them apart is the
/// difference between "this board has no recovery action" and "this image is
/// broken".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declaration {
    /// The board declares no physical recovery action. Both mos boards.
    None,
    /// The board declares these, in file order.
    Actions(Vec<RecoveryAction>),
    /// A declaration is present and is not one this build can read. Fails
    /// closed: no intent maps through it, and the reason is carried so the
    /// refusal can name it.
    Unreadable(String),
}

/// Why no action was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoAction {
    /// The board declares no physical recovery action at all.
    BoardDeclaresNone,
    /// The board's declaration could not be read.
    Unreadable(String),
    /// The intent names no action this board declares.
    UnknownIntent,
}

impl Declaration {
    /// Read the declaration at `path`. A file that is not there is
    /// [`Declaration::None`] — see [`DEFAULT_DECLARATION_PATH`].
    #[must_use]
    pub fn read(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::None,
            Err(err) => Self::Unreadable(format!("{} could not be read: {err}", path.display())),
        }
    }

    /// Read the declaration this device ships, honouring the test hook.
    #[must_use]
    pub fn from_env() -> Self {
        Self::read(&declaration_path())
    }

    /// Parse a declaration's text.
    ///
    /// The grammar is `boards/<board>/board.env`'s, narrowed to what a
    /// declaration needs: `KEY=value` one per line, the value optionally in
    /// double or single quotes, `#` comments and blank lines. Every other
    /// shell construct is refused rather than skipped — a line this parser did
    /// not understand and stepped over would silently drop a key and change
    /// what the board declares.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let values = match parse_assignments(text) {
            Ok(values) => values,
            Err(reason) => return Self::Unreadable(reason),
        };
        let Some(list) = values.iter().find(|(key, _)| key == ACTIONS_KEY) else {
            return Self::Unreadable(format!(
                "the declaration does not declare {ACTIONS_KEY}, so it says nothing about which \
                 physical actions this board has; a board with none declares it empty"
            ));
        };
        let names: Vec<&str> = list.1.split_whitespace().collect();
        if names.is_empty() {
            return Self::None;
        }

        let mut actions = Vec::with_capacity(names.len());
        for name in names {
            match action_from(&values, name) {
                Ok(action) => actions.push(action),
                Err(reason) => return Self::Unreadable(reason),
            }
        }
        if let Err(reason) = refuse_collisions(&actions) {
            return Self::Unreadable(reason);
        }
        Self::Actions(actions)
    }

    /// The declared actions; empty for both of the other states.
    #[must_use]
    pub fn actions(&self) -> &[RecoveryAction] {
        match self {
            Self::Actions(actions) => actions,
            Self::None | Self::Unreadable(_) => &[],
        }
    }

    /// Whether some declared action asserts presence under `mechanism`.
    #[must_use]
    pub fn declares_mechanism(&self, mechanism: &str) -> bool {
        self.actions().iter().any(|a| a.mechanism == mechanism)
    }

    /// The mechanisms this board answers `recovery.presence` with, in
    /// declaration order, for a refusal that has to say what the board offers.
    #[must_use]
    pub fn mechanisms(&self) -> Vec<&str> {
        self.actions()
            .iter()
            .map(|a| a.mechanism.as_str())
            .collect()
    }

    /// The action a recovery intent names.
    ///
    /// **THE PREMISE, and it is stated here because this is the site that
    /// stands on it.** The intent arrives on the kernel command line, which is
    /// written by the bootloader before Linux runs. That is not proof against
    /// an attacker who already has root on the running system: root can
    /// rewrite the GRUB configuration or the U-Boot environment and reboot
    /// into whatever intent it likes. It is proof against an API-level
    /// attacker, which is the threat this gate exists for — no route, no
    /// settings path and no operator-reachable surface in this tree writes a
    /// bootloader configuration or a kernel command line, so nothing an API
    /// client can do produces an intent.
    ///
    /// *This holds because the boot-time intent is unreachable from the API
    /// surface; if that stops holding — if any route, task or settings path
    /// ever edits GRUB's configuration, the U-Boot environment, or a kernel
    /// command line — it does not.* There is deliberately no check for it
    /// here: a check on this side could only observe the intent it was handed,
    /// which is exactly the thing that would have been forged.
    ///
    /// # Errors
    ///
    /// Returns [`NoAction`] naming which of the three closed doors was hit.
    pub fn map_intent(&self, intent: &str) -> Result<&RecoveryAction, NoAction> {
        match self {
            Self::None => Err(NoAction::BoardDeclaresNone),
            Self::Unreadable(reason) => Err(NoAction::Unreadable(reason.clone())),
            Self::Actions(actions) => actions
                .iter()
                .find(|a| a.intent == intent)
                .ok_or(NoAction::UnknownIntent),
        }
    }
}

/// The declaration path this device uses, honouring the test hook.
#[must_use]
pub fn declaration_path() -> PathBuf {
    std::env::var_os(DECLARATION_PATH_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_DECLARATION_PATH), PathBuf::from)
}

/// The kernel command-line path this device uses, honouring the test hook.
#[must_use]
pub fn cmdline_path() -> PathBuf {
    std::env::var_os(CMDLINE_PATH_ENV)
        .map_or_else(|| PathBuf::from(DEFAULT_CMDLINE_PATH), PathBuf::from)
}

/// The recovery intent a kernel command line carries, if any.
///
/// Fails closed on a command line carrying [`INTENT_PARAMETER`] more than once
/// or carrying it with an empty value: both are a mechanism that did not do
/// what it meant to, and guessing which occurrence was meant is how a
/// mechanism ends up selecting a tier nobody asked for.
///
/// # Errors
///
/// Returns the sentence describing the malformation.
pub fn intent_from_cmdline(cmdline: &str) -> Result<Option<&str>, String> {
    let prefix = format!("{INTENT_PARAMETER}=");
    let mut found: Option<&str> = None;
    for token in cmdline.split_whitespace() {
        if token == INTENT_PARAMETER {
            return Err(format!(
                "the kernel command line carries `{INTENT_PARAMETER}` with no value; a recovery \
                 intent names the action that produced it"
            ));
        }
        let Some(value) = token.strip_prefix(&prefix) else {
            continue;
        };
        if value.is_empty() {
            return Err(format!(
                "the kernel command line carries `{INTENT_PARAMETER}=` with an empty value"
            ));
        }
        if found.is_some() {
            return Err(format!(
                "the kernel command line carries `{INTENT_PARAMETER}` more than once; which \
                 action was meant cannot be decided here"
            ));
        }
        found = Some(value);
    }
    Ok(found)
}

/// The tier a declaration's `_TIER` value names.
fn tier_from(value: &str) -> Option<Option<ResetTier>> {
    match value {
        TIER_NONE => Some(None),
        "configuration" => Some(Some(ResetTier::Configuration)),
        "application-data" => Some(Some(ResetTier::ApplicationData)),
        "full-factory" => Some(Some(ResetTier::FullFactory)),
        _ => None,
    }
}

/// Build one action out of the parsed assignments, or say why it cannot be.
fn action_from(values: &[(String, String)], name: &str) -> Result<RecoveryAction, String> {
    if !is_action_name(name) {
        return Err(format!(
            "{ACTIONS_KEY} names `{name}`, which is not an action name: a name is uppercase \
             letters, digits and underscores, starting with a letter, because it is the middle of \
             the keys that describe it"
        ));
    }
    let get = |suffix: &str| -> Result<String, String> {
        let key = format!("RECOVERY_{name}_{suffix}");
        match values.iter().find(|(k, _)| *k == key) {
            None => Err(format!(
                "{ACTIONS_KEY} names `{name}` and the declaration has no {key}"
            )),
            Some((_, value)) if value.is_empty() => Err(format!(
                "{key} is declared empty; an empty declaration is not a value"
            )),
            Some((_, value)) => Ok(value.clone()),
        }
    };

    let intent = get("INTENT")?;
    if !is_intent_token(&intent) {
        return Err(format!(
            "RECOVERY_{name}_INTENT is `{intent}`, which is not a kernel command-line token: \
             lowercase letters, digits and dashes"
        ));
    }
    let mechanism = get("MECHANISM")?;
    if !is_mechanism_token(&mechanism) {
        return Err(format!(
            "RECOVERY_{name}_MECHANISM is `{mechanism}`, which is not a mechanism name: \
             lowercase letters, digits and dashes, starting with a letter"
        ));
    }
    let channel = get("CHANNEL")?;
    if !is_channel_path(&channel) {
        return Err(format!(
            "RECOVERY_{name}_CHANNEL is `{channel}`; a channel is a device under /dev, because a \
             minted credential is written to it and never to a file"
        ));
    }
    let tier_value = get("TIER")?;
    let Some(tier) = tier_from(&tier_value) else {
        return Err(format!(
            "RECOVERY_{name}_TIER is `{tier_value}`; it is one of {TIER_NONE}, configuration, \
             application-data, full-factory"
        ));
    };

    Ok(RecoveryAction {
        name: name.to_string(),
        intent,
        mechanism,
        channel: PathBuf::from(channel),
        tier,
    })
}

/// Two actions must not share an intent or a mechanism.
///
/// A shared intent is a mapping with two answers; a shared mechanism is an
/// audit trail that cannot say which door was used, which is the whole reason
/// the mechanism rides in the event name.
fn refuse_collisions(actions: &[RecoveryAction]) -> Result<(), String> {
    let mut intents = BTreeSet::new();
    let mut mechanisms = BTreeSet::new();
    for action in actions {
        if !intents.insert(action.intent.as_str()) {
            return Err(format!(
                "two declared actions share the intent `{}`; one command line would name both",
                action.intent
            ));
        }
        if !mechanisms.insert(action.mechanism.as_str()) {
            return Err(format!(
                "two declared actions share the mechanism `{}`; the audit trail could not say \
                 which one was used",
                action.mechanism
            ));
        }
    }
    Ok(())
}

fn is_action_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn is_intent_token(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_mechanism_token(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_channel_path(value: &str) -> bool {
    value.starts_with("/dev/") && value.len() > "/dev/".len() && !value.contains("..")
}

/// `KEY=value` lines, in file order, or the sentence refusing the file.
fn parse_assignments(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut values = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {number} is not a `KEY=value` assignment: {line}"
            ));
        };
        if key.is_empty() || !is_key(key) {
            return Err(format!(
                "line {number} assigns to `{key}`, which is not a declaration key"
            ));
        }
        let value = unquote(value)
            .ok_or_else(|| format!("line {number} is not a value this reader accepts: {value}"))?;
        values.push((key.to_string(), value));
    }
    Ok(values)
}

fn is_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A value, with one layer of matching quotes removed.
///
/// A quoted value may hold anything but its own quote; an unquoted one may
/// hold no whitespace and none of the shell metacharacters that would mean
/// something to the `source` this reader deliberately never performs.
fn unquote(value: &str) -> Option<String> {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return (!inner.contains(quote)).then(|| inner.to_string());
        }
    }
    if value.contains(['"', '\'', '$', '`', '\\', ' ', '\t']) {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A board that declares one action, spelled the way a BSP renders it.
    const ONE_ACTION: &str = "\
# rendered from boards/example/board.env
BOARD_RECOVERY_ACTIONS=\"GRUB_RECOVERY_ENTRY\"
RECOVERY_GRUB_RECOVERY_ENTRY_INTENT=recovery
RECOVERY_GRUB_RECOVERY_ENTRY_MECHANISM=grub-entry
RECOVERY_GRUB_RECOVERY_ENTRY_CHANNEL=/dev/tty0
RECOVERY_GRUB_RECOVERY_ENTRY_TIER=none
";

    fn one_action() -> Declaration {
        Declaration::parse(ONE_ACTION)
    }

    #[test]
    fn a_declared_action_maps_from_its_intent() {
        let declaration = one_action();
        let action = declaration.map_intent("recovery").expect("it maps");
        assert_eq!(action.name, "GRUB_RECOVERY_ENTRY");
        assert_eq!(action.mechanism, "grub-entry");
        assert_eq!(action.channel, PathBuf::from("/dev/tty0"));
        assert_eq!(action.tier, None);
        assert!(declaration.declares_mechanism("grub-entry"));
        assert_eq!(declaration.mechanisms(), vec!["grub-entry"]);
    }

    #[test]
    fn every_tier_a_board_may_name_maps_to_the_tier_the_flows_run() {
        for (declared, want) in [
            (TIER_NONE, None),
            ("configuration", Some(ResetTier::Configuration)),
            ("application-data", Some(ResetTier::ApplicationData)),
            ("full-factory", Some(ResetTier::FullFactory)),
        ] {
            let text = ONE_ACTION.replace("_TIER=none", &format!("_TIER={declared}"));
            let declaration = Declaration::parse(&text);
            assert_eq!(
                declaration.map_intent("recovery").expect("it maps").tier,
                want,
                "declared tier {declared}"
            );
        }
    }

    #[test]
    fn an_absent_declaration_is_a_board_that_declares_none() {
        let dir = tempfile::tempdir().unwrap();
        let declaration = Declaration::read(&dir.path().join("nothing-here.conf"));
        assert_eq!(declaration, Declaration::None);
        assert_eq!(
            declaration.map_intent("recovery"),
            Err(NoAction::BoardDeclaresNone)
        );
    }

    #[test]
    fn an_empty_list_is_a_board_that_declares_none() {
        // The shipped spelling on both mos boards: the key is declared, and
        // what it declares is that there is no action.
        let declaration = Declaration::parse("BOARD_RECOVERY_ACTIONS=\"\"\n");
        assert_eq!(declaration, Declaration::None);
        assert_eq!(
            declaration.map_intent("anything"),
            Err(NoAction::BoardDeclaresNone)
        );
        assert!(declaration.mechanisms().is_empty());
    }

    #[test]
    fn an_intent_no_action_declares_maps_to_nothing() {
        assert_eq!(
            one_action().map_intent("factory-reset"),
            Err(NoAction::UnknownIntent)
        );
        assert!(!one_action().declares_mechanism("uboot-menu"));
    }

    /// Every way a declaration can be malformed fails CLOSED, and each is
    /// reached: a case that stopped being malformed would stop being tested
    /// here rather than start passing somewhere else.
    #[test]
    fn every_malformed_declaration_maps_no_intent() {
        let cases: &[(&str, &str)] = &[
            ("no list at all", "RECOVERY_X_INTENT=recovery\n"),
            (
                "a name that is not a name",
                "BOARD_RECOVERY_ACTIONS=\"grub entry\"\n",
            ),
            (
                "a missing key",
                "BOARD_RECOVERY_ACTIONS=\"A\"\nRECOVERY_A_INTENT=recovery\n",
            ),
            (
                "a key declared empty",
                &ONE_ACTION.replace("_MECHANISM=grub-entry", "_MECHANISM=\"\""),
            ),
            (
                "an intent that is not a command-line token",
                &ONE_ACTION.replace("_INTENT=recovery", "_INTENT=Recovery!"),
            ),
            (
                "a mechanism that is not a mechanism name",
                &ONE_ACTION.replace("_MECHANISM=grub-entry", "_MECHANISM=GRUB_ENTRY"),
            ),
            (
                "a channel that is not a device",
                &ONE_ACTION.replace("_CHANNEL=/dev/tty0", "_CHANNEL=/var/lib/mica/console"),
            ),
            (
                "a tier no flow implements",
                &ONE_ACTION.replace("_TIER=none", "_TIER=secure-wipe"),
            ),
            (
                "a line that is not an assignment",
                &format!("{ONE_ACTION}source /etc/passwd\n"),
            ),
            (
                "a value a shell would expand",
                &ONE_ACTION.replace("_INTENT=recovery", "_INTENT=$(id)"),
            ),
            (
                "two actions sharing an intent",
                &format!(
                    "{}{}",
                    ONE_ACTION.replace(
                        "BOARD_RECOVERY_ACTIONS=\"GRUB_RECOVERY_ENTRY\"",
                        "BOARD_RECOVERY_ACTIONS=\"GRUB_RECOVERY_ENTRY BUTTON\"",
                    ),
                    "RECOVERY_BUTTON_INTENT=recovery\nRECOVERY_BUTTON_MECHANISM=button\n\
                     RECOVERY_BUTTON_CHANNEL=/dev/tty0\nRECOVERY_BUTTON_TIER=none\n",
                ),
            ),
            (
                "two actions sharing a mechanism",
                &format!(
                    "{}{}",
                    ONE_ACTION.replace(
                        "BOARD_RECOVERY_ACTIONS=\"GRUB_RECOVERY_ENTRY\"",
                        "BOARD_RECOVERY_ACTIONS=\"GRUB_RECOVERY_ENTRY BUTTON\"",
                    ),
                    "RECOVERY_BUTTON_INTENT=factory\nRECOVERY_BUTTON_MECHANISM=grub-entry\n\
                     RECOVERY_BUTTON_CHANNEL=/dev/tty0\nRECOVERY_BUTTON_TIER=none\n",
                ),
            ),
        ];
        for (label, text) in cases {
            let declaration = Declaration::parse(text);
            let Declaration::Unreadable(reason) = &declaration else {
                panic!("{label} was accepted: {declaration:?}");
            };
            assert!(!reason.is_empty(), "{label} refused without a reason");
            assert_eq!(
                declaration.map_intent("recovery"),
                Err(NoAction::Unreadable(reason.clone())),
                "{label}",
            );
            assert!(
                declaration.actions().is_empty() && declaration.mechanisms().is_empty(),
                "{label} left an action reachable",
            );
        }
    }

    #[test]
    fn an_intent_is_read_off_the_command_line_and_only_from_the_parameter() {
        assert_eq!(
            intent_from_cmdline("root=/dev/mmcblk0p6 mos.recovery=recovery quiet"),
            Ok(Some("recovery"))
        );
        assert_eq!(intent_from_cmdline("root=/dev/mmcblk0p6 quiet"), Ok(None));
        // A parameter that merely contains the name is not the parameter.
        assert_eq!(intent_from_cmdline("not.mos.recovery=recovery"), Ok(None));
        assert_eq!(intent_from_cmdline("mos.recoveryx=recovery"), Ok(None));
    }

    #[test]
    fn a_malformed_command_line_yields_no_intent_at_all() {
        for cmdline in [
            "mos.recovery=recovery mos.recovery=factory",
            "mos.recovery=",
            "quiet mos.recovery",
        ] {
            assert!(
                intent_from_cmdline(cmdline).is_err(),
                "`{cmdline}` must not produce an intent"
            );
        }
    }
}
