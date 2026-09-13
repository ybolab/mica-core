//! First-boot self-provisioning: turning an empty STATE into a working device.
//!
//! `docs/design/provisioning.md` section 2 states the product behaviour: when
//! STATE holds no configuration the device generates its own, persists it, and
//! is thereafter a fully working, configurable appliance. Wiping STATE returns
//! it here, which is what makes "factory reset" mean anything.
//!
//! None of this may need a network: an appliance is unboxed on a bench with no
//! DHCP server, no DNS and possibly no cable, and still has to come up with a
//! hostname, an identity and a credential. The seeded hostname is therefore
//! derived from the device identity — drawn from the system CSPRNG by
//! [`crate::identity`] — and this module references no networking API at all:
//! no socket, resolver, DHCP lease, MAC lookup or wait on a network unit. Its
//! only I/O is reading the image profile file and writing STATE. Seeding is
//! committed by exactly one [`Store::save`], everything before it mutating a
//! private copy, so a failure at any step leaves STATE and the caller's tree
//! untouched. [`ensure_provisioned`] is idempotent: on a provisioned device it
//! returns before touching anything, so no credential is regenerated, no
//! operator setting reverted and `seededGeneration` does not move.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use micad_settings::{ProvisioningState, Settings, Store};

use crate::identity;

/// Revision of the seeding rules implemented by this module.
///
/// Recorded in `provisioning.seededGeneration` so a later image can tell which
/// rules produced a fielded device's tree. Bump it when the seeded values
/// change in a way a re-seed would have to repair; do not bump it for changes
/// that only affect devices which have not provisioned yet.
pub const SEEDING_GENERATION: u32 = 1;

/// Default location of the image profile file, shipped read-only in the rootfs.
pub const DEFAULT_PROFILE_PATH: &str = "/usr/lib/mica/profile.conf";

/// Key read out of the profile file.
const PROFILE_KEY: &str = "MICA_PROFILE";

/// Prefix of a seeded hostname.
const HOSTNAME_PREFIX: &str = "mos-";

/// How many leading hex characters of the device identifier the seeded hostname
/// carries. Eight hex characters is 32 bits, enough that two devices on one
/// LAN colliding is not a practical concern, and short enough to read off a
/// label.
const HOSTNAME_ID_CHARS: usize = 8;

/// Which image this rootfs is.
///
/// The two profiles currently seed IDENTICAL values: SSH is off on both, and
/// no other seeded value has ever been profile-dependent. So this enum selects
/// nothing today. It is kept because [`read_profile`]'s fail-closed parsing is
/// load-bearing on its own — an unreadable or unrecognised profile must resolve
/// to the conservative variant rather than propagate as an error — and because
/// a per-profile default is the kind of thing that gets re-introduced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// Development image.
    Dev,
    /// Production image: `docs/design/access.md` section 5 defaults.
    Prod,
}

impl Profile {
    /// Whether `access.ssh.enabled` is seeded on for this profile.
    ///
    /// False for BOTH profiles. SSH is off on a fresh device regardless of the
    /// image it booted; persistent access is granted by installing a public key
    /// in `access.ssh.authorizedKeys`, and a transient root password can be set
    /// through the web UI for the current boot only. A dev image that seeded
    /// SSH on would be reachable before any of that was configured.
    ///
    /// Written as an exhaustive `match` rather than a bare `false` so that
    /// re-introducing a per-profile default is a one-arm edit the compiler
    /// checks, and so a new variant cannot silently inherit this answer.
    #[must_use]
    pub fn ssh_enabled_default(self) -> bool {
        match self {
            Self::Dev | Self::Prod => false,
        }
    }
}

/// What [`ensure_provisioned`] had to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The tree was seeded and persisted.
    Seeded,
    /// `provisioning.state` was already `complete`; nothing was read or written.
    AlreadyProvisioned,
}

/// Provision this device if STATE says it has not been provisioned yet.
///
/// When `settings.provisioning.state` is `pending`:
/// [`identity::ensure_identity`] establishes `provisioning.deviceId` and both
/// per-device secrets on `state_dir`; `hostname` becomes `mos-<first eight hex
/// chars of deviceId>`, but only while it is still the built-in default, so an
/// operator who has already named the device keeps that name;
/// `access.ssh.enabled` is taken from the image profile at `profile_path`,
/// which today seeds it false for every profile (see
/// [`Profile::ssh_enabled_default`]); `network` is left empty on purpose,
/// because the image ships a static `80-dhcp.network` matching `eth*` so DHCP
/// already works, whereas seeding one entry per interface name would render
/// networkd units for interfaces that may not exist on this board; `wifi` is
/// left at its defaults, the station role staying off because there is no
/// network to join yet and AP mode being connd's call; `provisioning.state`
/// becomes `complete` and `seededGeneration` becomes [`SEEDING_GENERATION`];
/// and the whole tree is persisted with one [`Store::save`]. On success
/// `settings` is replaced with the seeded tree, so the caller's first reconcile
/// already sees it.
///
/// # Errors
///
/// Returns an error when the identity cannot be established or the settings
/// cannot be persisted; in both cases STATE and `settings` are left exactly as
/// they were, so the device stays unprovisioned and the next boot retries. A
/// profile file that is missing, unreadable or malformed is not an error: it
/// resolves to [`Profile::Prod`] with a warning (see [`read_profile`]).
pub fn ensure_provisioned(
    store: &Store,
    state_dir: &Path,
    profile_path: &Path,
    settings: &mut Settings,
) -> Result<Outcome> {
    if settings.provisioning.state == ProvisioningState::Complete {
        return Ok(Outcome::AlreadyProvisioned);
    }

    // Seed into a copy. `*settings` is only replaced once the save that commits
    // the tree has returned, so no caller ever observes a half-seeded tree and
    // no half-seeded tree reaches STATE.
    let mut seeded = settings.clone();

    identity::ensure_identity(state_dir, &mut seeded).context("establish device identity")?;
    let device_id = seeded
        .provisioning
        .device_id
        .clone()
        .context("device identity absent after generation")?;

    if seeded.hostname == Settings::default().hostname {
        seeded.hostname = seeded_hostname(&device_id);
    }

    seeded.access.ssh.enabled = read_profile(profile_path).ssh_enabled_default();

    seeded.provisioning.state = ProvisioningState::Complete;
    seeded.provisioning.seeded_generation = SEEDING_GENERATION;

    store.save(&seeded).context("persist the seeded settings")?;

    tracing::info!(
        hostname = seeded.hostname,
        ssh_enabled = seeded.access.ssh.enabled,
        seeded_generation = seeded.provisioning.seeded_generation,
        "first-boot provisioning complete"
    );
    *settings = seeded;
    Ok(Outcome::Seeded)
}

/// Read the image profile from a shell-style `KEY=value` file.
///
/// Fails closed: a file that is missing, unreadable, carries no `MICA_PROFILE`
/// key or carries a value this build does not know resolves to
/// [`Profile::Prod`] with a warning, and the comparison is case-sensitive, so
/// `DEV` is not `dev`. No seeded value currently depends on the answer — both
/// profiles seed the same tree — but the rule is kept because guessing
/// [`Profile::Dev`] on a production device is the expensive direction to be
/// wrong in. Blank lines and `#` comments are skipped, a value may be wrapped
/// in single or double quotes, and a repeated key takes its last value, as a
/// shell would.
#[must_use]
pub fn read_profile(path: &Path) -> Profile {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "image profile unreadable, assuming `prod` (SSH stays off)"
            );
            return Profile::Prod;
        }
    };
    let Some(value) = profile_value(&text) else {
        tracing::warn!(
            path = %path.display(),
            key = PROFILE_KEY,
            "image profile carries no profile key, assuming `prod` (SSH stays off)"
        );
        return Profile::Prod;
    };
    match value {
        "dev" => Profile::Dev,
        "prod" => Profile::Prod,
        other => {
            tracing::warn!(
                path = %path.display(),
                value = other,
                "unrecognised image profile, assuming `prod` (SSH stays off)"
            );
            Profile::Prod
        }
    }
}

/// Last value assigned to [`PROFILE_KEY`] in a shell-style `KEY=value` document.
fn profile_value(text: &str) -> Option<&str> {
    let mut value = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != PROFILE_KEY {
            continue;
        }
        value = Some(unquote(raw.trim()));
    }
    value
}

/// Strip one layer of matching single or double quotes, if present.
fn unquote(raw: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = raw
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    raw
}

/// Hostname for a device with this identifier.
///
/// Derived from the device identity and nothing else: no DHCP option, no
/// reverse DNS lookup, no MAC address. That is what lets a device with no
/// network reach a named, working state, and it makes the hostname stable
/// across cables, subnets and NIC replacements.
///
/// `chars().take(..)` rather than a byte slice, so an identifier shorter than
/// [`HOSTNAME_ID_CHARS`] yields a short hostname instead of a panic.
fn seeded_hostname(device_id: &str) -> String {
    let suffix: String = device_id.chars().take(HOSTNAME_ID_CHARS).collect();
    format!("{HOSTNAME_PREFIX}{suffix}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use micad_settings::{ApMode, ProvisioningSettings, SshSettings, WifiSettings};
    use tempfile::TempDir;

    use super::*;

    /// A profile file naming the given profile, plus noise a real shell-style
    /// file would carry.
    fn write_profile(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("profile.conf");
        fs::write(&path, body).expect("write profile");
        path
    }

    fn prod_profile(dir: &Path) -> PathBuf {
        write_profile(dir, "MICA_PROFILE=prod\n")
    }

    fn settings_path(dir: &Path) -> PathBuf {
        dir.join("settings.toml")
    }

    fn store_in(dir: &Path) -> Store {
        // The `/mos/config/` namespace has to exist: an absent document is a
        // default, an absent namespace is the DATA medium being gone, and the
        // store refuses that rather than defaulting (PLAN-070 §5.2.6).
        let config = dir.join("config");
        fs::create_dir_all(&config).expect("create the config namespace");
        Store::new(settings_path(dir), config)
    }

    /// Every file under `<state_dir>/secrets`, by name, as raw bytes.
    fn secrets_snapshot(state_dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let dir = state_dir.join("secrets");
        let mut out = BTreeMap::new();
        for entry in fs::read_dir(&dir).expect("read secrets dir") {
            let entry = entry.expect("dir entry");
            let bytes = fs::read(entry.path()).expect("read secret");
            out.insert(entry.file_name().to_string_lossy().into_owned(), bytes);
        }
        out
    }

    // A fresh STATE is seeded into a tree the device can actually run on, and
    // that tree is on disk when the call returns.
    #[test]
    fn fresh_state_is_seeded_and_persisted() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());
        let mut settings = Settings::default();

        let outcome = ensure_provisioned(&store, dir.path(), &profile, &mut settings)
            .expect("ensure_provisioned");
        assert_eq!(outcome, Outcome::Seeded);

        let device_id = settings
            .provisioning
            .device_id
            .as_deref()
            .expect("device_id seeded");
        assert_eq!(device_id.len(), 32, "device_id must be 16 bytes of hex");
        assert_eq!(settings.provisioning.state, ProvisioningState::Complete);
        assert_eq!(settings.provisioning.seeded_generation, 1);
        assert_eq!(settings.hostname, format!("mos-{}", &device_id[..8]));
        assert!(
            !settings.access.ssh.enabled,
            "prod profile must not open SSH"
        );
        assert!(
            settings.access.device.password_hash.is_some(),
            "the device credential must exist after first boot"
        );

        // Nothing is invented for hardware this build cannot see.
        assert!(
            settings.network.is_empty(),
            "network must stay empty: the image's static 80-dhcp.network already \
             covers eth*, and a seeded entry would render a unit for an \
             interface that may not exist, got {:?}",
            settings.network
        );
        assert_eq!(settings.wifi, WifiSettings::default());
        assert!(!settings.wifi.client.enabled);
        assert_eq!(settings.wifi.ap.mode, ApMode::Off);

        // The one write really happened, and it holds the same tree.
        let on_disk = store.load().expect("load seeded settings");
        assert_eq!(on_disk, settings);
    }

    // The "run twice, same state" acceptance criterion. Note the outcome
    // assertion: without it this test passes even with the `Complete` guard
    // removed, because re-seeding an already seeded tree happens to reproduce
    // the same bytes.
    #[test]
    fn running_twice_is_a_genuine_no_op() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());
        let mut settings = Settings::default();

        assert_eq!(
            ensure_provisioned(&store, dir.path(), &profile, &mut settings).expect("first"),
            Outcome::Seeded
        );
        let first_tree = settings.clone();
        let first_bytes = fs::read(settings_path(dir.path())).expect("read settings file");
        let first_secrets = secrets_snapshot(dir.path());
        assert_eq!(first_secrets.len(), 2, "both secrets must exist");

        // Reload from STATE, exactly as the next boot would.
        let mut second = store.load().expect("reload");
        assert_eq!(second, first_tree);
        assert_eq!(
            ensure_provisioned(&store, dir.path(), &profile, &mut second).expect("second"),
            Outcome::AlreadyProvisioned
        );

        assert_eq!(second, first_tree, "the whole settings tree must not move");
        assert_eq!(second.provisioning.seeded_generation, 1);
        assert_eq!(
            fs::read(settings_path(dir.path())).expect("re-read settings file"),
            first_bytes,
            "settings.toml must be byte-identical after a second run"
        );
        assert_eq!(
            secrets_snapshot(dir.path()),
            first_secrets,
            "no credential may be regenerated on a re-run"
        );
    }

    // An operator's own values survive. This is the case that catches a guard
    // removal for real: the stored SSH state is the opposite of what the
    // profile would seed, and the hostname is not the built-in default.
    #[test]
    fn operator_changes_survive_a_re_run() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());

        let mut settings = Settings::default();
        ensure_provisioned(&store, dir.path(), &profile, &mut settings).expect("seed");

        // The operator names the device and opens SSH, as they are entitled to.
        settings.hostname = "edge-42".to_string();
        settings.access.ssh.enabled = true;
        settings.access.ssh.port = 2222;
        store.save(&settings).expect("save operator changes");

        let mut reloaded = store.load().expect("reload");
        assert_eq!(
            ensure_provisioned(&store, dir.path(), &profile, &mut reloaded).expect("re-run"),
            Outcome::AlreadyProvisioned
        );

        assert_eq!(reloaded.hostname, "edge-42");
        assert!(
            reloaded.access.ssh.enabled,
            "a re-run must not close SSH the operator opened"
        );
        assert_eq!(reloaded.access.ssh.port, 2222);
        assert_eq!(reloaded, settings);
        assert_eq!(store.load().expect("reload again"), settings);
    }

    // The hostname rule also protects an operator who renamed the device before
    // provisioning finished, e.g. through a pre-seeded STATE.
    #[test]
    fn seeding_does_not_overwrite_a_non_default_hostname() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());

        let mut settings = Settings {
            hostname: "edge-42".to_string(),
            ..Settings::default()
        };
        assert_eq!(settings.provisioning.state, ProvisioningState::Pending);

        ensure_provisioned(&store, dir.path(), &profile, &mut settings).expect("seed");
        assert_eq!(settings.hostname, "edge-42");
        assert_eq!(settings.provisioning.state, ProvisioningState::Complete);
    }

    // The profile matrix, including every way of being malformed. Only the
    // literal `dev` may open SSH.
    #[test]
    fn profile_matrix_fails_closed() {
        let dir = TempDir::new().expect("tempdir");

        // The one input that opens SSH.
        assert_eq!(
            read_profile(&write_profile(dir.path(), "MICA_PROFILE=dev\n")),
            Profile::Dev
        );
        assert_eq!(
            read_profile(&write_profile(dir.path(), "MICA_PROFILE=prod\n")),
            Profile::Prod
        );

        // Everything else must land on prod.
        for body in [
            "",
            "\n\n",
            "# MICA_PROFILE=dev\n",
            "MICA_PROFILE=\n",
            "MICA_PROFILE=DEV\n",
            "MICA_PROFILE=Dev\n",
            "MICA_PROFILE=development\n",
            "MICA_PROFILE=devel\n",
            "MICA_PROFILE = dev extra\n",
            "MICA_VARIANT=cx3576\n",
            "MOSPROFILE=dev\n",
            "MICA_PROFILE_EXTRA=dev\n",
            "\u{0}\u{1}garbage\u{2}\n",
        ] {
            let path = write_profile(dir.path(), body);
            assert_eq!(
                read_profile(&path),
                Profile::Prod,
                "malformed profile must fail closed, body {body:?}"
            );
        }

        // An absent file is the case that happens in practice: a rootfs
        // carrying no profile file, or a bind mount that did not come up. It
        // must not open SSH either.
        let missing = dir.path().join("no-such-profile.conf");
        assert!(!missing.exists());
        assert_eq!(read_profile(&missing), Profile::Prod);

        // A directory where a file is expected: unreadable, not merely absent.
        assert_eq!(read_profile(dir.path()), Profile::Prod);
    }

    // The parsing details the matrix above leans on, stated directly.
    #[test]
    fn profile_parsing_handles_shell_style_documents() {
        let dir = TempDir::new().expect("tempdir");
        let path = write_profile(
            dir.path(),
            "# image profile\n\
             \n\
             MICA_VARIANT=cx3576\n\
             MICA_PROFILE=\"dev\"\n",
        );
        assert_eq!(read_profile(&path), Profile::Dev);

        let path = write_profile(dir.path(), "MICA_PROFILE='dev'\n");
        assert_eq!(read_profile(&path), Profile::Dev);

        // Last assignment wins, as a shell would evaluate it.
        let path = write_profile(dir.path(), "MICA_PROFILE=dev\nMICA_PROFILE=prod\n");
        assert_eq!(read_profile(&path), Profile::Prod);
        let path = write_profile(dir.path(), "MICA_PROFILE=prod\nMICA_PROFILE=dev\n");
        assert_eq!(read_profile(&path), Profile::Dev);
    }

    // The profile really drives the seeded value, not just `read_profile`. Both
    // profiles now seed SSH OFF, so this is the assertion that the dev image
    // does not open SSH either: persistent access is a public key in
    // `access.ssh.authorizedKeys`, and the transient root password is set at
    // runtime. The prod side is unchanged and deliberately still asserted —
    // dropping it would leave the fail-closed direction untested.
    #[test]
    fn neither_profile_seeds_ssh_on() {
        for (body, expected) in [
            ("MICA_PROFILE=dev\n", false),
            ("MICA_PROFILE=prod\n", false),
        ] {
            let dir = TempDir::new().expect("tempdir");
            let store = store_in(dir.path());
            let profile = write_profile(dir.path(), body);
            let mut settings = Settings::default();

            ensure_provisioned(&store, dir.path(), &profile, &mut settings).expect("seed");
            assert_eq!(
                settings.access.ssh.enabled, expected,
                "profile {body:?} must seed ssh.enabled = {expected}; SSH is off on \
                 a fresh device whatever image it booted"
            );
            assert_eq!(
                store.load().expect("reload").access.ssh.enabled,
                expected,
                "the persisted tree must agree"
            );
            // Only `enabled` is profile-driven; the rest stays at the schema
            // defaults so a profile change cannot silently move the port.
            assert_eq!(settings.access.ssh.port, SshSettings::default().port);
        }

        // A device with no profile file at all is treated as production.
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let mut settings = Settings::default();
        ensure_provisioned(
            &store,
            dir.path(),
            &dir.path().join("absent.conf"),
            &mut settings,
        )
        .expect("seed");
        assert!(
            !settings.access.ssh.enabled,
            "a missing profile file must not open SSH"
        );
    }

    // The offline property. The hostname is a pure function of the device
    // identity, so it needs no DHCP, no DNS and no MAC lookup. This proves the
    // derivation; it does not prove the process makes no syscall, which is
    // covered instead by the module carrying no networking API at all.
    #[test]
    fn hostname_is_derived_only_from_the_device_identity() {
        let seed = |device_id: &str| {
            let dir = TempDir::new().expect("tempdir");
            let store = store_in(dir.path());
            let profile = prod_profile(dir.path());
            let mut settings = Settings {
                provisioning: ProvisioningSettings {
                    device_id: Some(device_id.to_string()),
                    ..ProvisioningSettings::default()
                },
                ..Settings::default()
            };
            ensure_provisioned(&store, dir.path(), &profile, &mut settings).expect("seed");
            // The pre-set identity is kept, not replaced.
            assert_eq!(settings.provisioning.device_id.as_deref(), Some(device_id));
            settings.hostname
        };

        let a = "0123456789abcdef0123456789abcdef";
        let b = "fedcba9876543210fedcba9876543210";
        // Same identity, two independent devices, two independent runs.
        assert_eq!(seed(a), seed(a));
        assert_eq!(seed(a), "mos-01234567");
        // A different identity must produce a different name, or two devices on
        // one LAN would collide.
        assert_ne!(seed(a), seed(b));
        assert_eq!(seed(b), "mos-fedcba98");

        // The two identities differ only after the eighth character, so this
        // catches a hostname built from a fixed prefix rather than the identity.
        let c = "0123456789abcdefffffffffffffffff";
        assert_eq!(seed(a), seed(c));

        // Shorter than the window: a short name, not a panic.
        assert_eq!(seed("abc"), "mos-abc");
    }

    // A failing save must not leave a tree claiming `complete`.
    #[test]
    fn a_failed_save_leaves_no_complete_tree_on_disk() {
        let dir = TempDir::new().expect("tempdir");
        let profile = prod_profile(dir.path());

        // The settings file's parent directory is a regular file, so
        // `Store::save`'s `create_dir_all` fails. Root-safe: this is a type
        // error on the path, not a permission check.
        let blocker = dir.path().join("blocked");
        fs::write(&blocker, b"not a directory").expect("write blocker");
        let config = dir.path().join("config");
        fs::create_dir_all(&config).expect("create the config namespace");
        let store = Store::new(blocker.join("settings.toml"), config);

        let mut settings = Settings::default();
        let err = ensure_provisioned(&store, dir.path(), &profile, &mut settings)
            .expect_err("save must fail");
        assert!(
            format!("{err:#}").contains("persist the seeded settings"),
            "unexpected error: {err:#}"
        );

        // Nothing claiming `complete` reached STATE, and the caller's tree was
        // not advanced either — the next boot retries from scratch.
        assert_eq!(settings, Settings::default());
        assert_eq!(settings.provisioning.state, ProvisioningState::Pending);
        assert_eq!(
            fs::read(&blocker).expect("re-read blocker"),
            b"not a directory",
            "the failed save must not have written anything"
        );
        assert!(!blocker.join("settings.toml").exists());
        assert!(!dir.path().join("settings.toml").exists());
    }

    // And a failure *before* the save must leave an existing on-disk tree
    // byte-identical, not partially rewritten.
    #[test]
    fn a_failed_identity_step_leaves_the_on_disk_tree_untouched() {
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());
        store.save(&Settings::default()).expect("seed the file");
        let before = fs::read(settings_path(dir.path())).expect("read settings file");

        // The state directory is a regular file, so the secrets directory
        // cannot be created and `ensure_identity` fails.
        let state_dir = dir.path().join("state-is-a-file");
        fs::write(&state_dir, b"x").expect("write blocker");

        let mut settings = store.load().expect("load");
        let err = ensure_provisioned(&store, &state_dir, &profile, &mut settings)
            .expect_err("identity must fail");
        assert!(
            format!("{err:#}").contains("establish device identity"),
            "unexpected error: {err:#}"
        );

        assert_eq!(settings.provisioning.state, ProvisioningState::Pending);
        assert!(settings.provisioning.device_id.is_none());
        assert_eq!(
            fs::read(settings_path(dir.path())).expect("re-read settings file"),
            before,
            "settings.toml must be byte-identical after a failed provisioning run"
        );
        assert_eq!(
            store.load().expect("reload").provisioning.state,
            ProvisioningState::Pending
        );
    }

    // The seeding revision is recorded, and it is the module's constant rather
    // than a counter that moves on every boot.
    #[test]
    fn seeded_generation_records_the_seeding_revision() {
        assert_eq!(SEEDING_GENERATION, 1);

        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());
        let mut settings = Settings::default();
        assert_eq!(settings.provisioning.seeded_generation, 0);

        ensure_provisioned(&store, dir.path(), &profile, &mut settings).expect("seed");
        assert_eq!(settings.provisioning.seeded_generation, 1);
        assert_eq!(
            store.load().expect("reload").provisioning.seeded_generation,
            1
        );

        // The field records *which* rules seeded the tree, not how many
        // attempts it took. A tree left `pending` by an interrupted run that
        // already carried a generation therefore gets stamped, not incremented.
        let dir = TempDir::new().expect("tempdir");
        let store = store_in(dir.path());
        let profile = prod_profile(dir.path());
        let mut interrupted = Settings {
            provisioning: ProvisioningSettings {
                seeded_generation: 5,
                ..ProvisioningSettings::default()
            },
            ..Settings::default()
        };
        ensure_provisioned(&store, dir.path(), &profile, &mut interrupted).expect("re-seed");
        assert_eq!(interrupted.provisioning.seeded_generation, 1);
    }
}
