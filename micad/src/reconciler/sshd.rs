//! SSH access reconciler: renders the sshd drop-in from `access.ssh` and drives
//! `ssh.service`. Three system effects, in this order:
//!
//! - `/etc/ssh/authorized_keys.d/<account>` is rendered from
//!   `access.ssh.authorizedKeys`, at 0600, after the list has been re-validated
//!   — one file per managed login account, every one holding the same key list.
//! - `/etc/ssh/sshd_config.d/10-mos.conf` is rendered from `access.ssh`. That
//!   directory is the one writable part of `/etc` on the mos read-only root, a
//!   STATE-backed bind mount (`etc-ssh.mount`).
//! - `ssh.service` is brought to the state `access.ssh.enabled` asks for.
//!
//! Configuration before service start, deliberately: an sshd started against a
//! stale drop-in listens on the wrong port, or accepts an authentication method
//! the operator has already turned off.
//!
//! The device password does not reach PAM. The credential of record for shell
//! access is an SSH public key, or a transient password set through
//! `crate::transient` that the next boot clears; nothing reads
//! `secrets/device-password` back, because a hash in the root shadow entry
//! would be a password on a fielded device that never expires.
//! `PasswordAuthentication` is gated on that transient password: the rendered
//! value is the setting AND `transient::transient_password_active`, so root
//! ships locked and stays locked unless a transient password is active.
//! `AuthorizedKeysFile` is not rendered here — it is the static image file
//! `05-mos-authorized-keys.conf`, which sorts ahead of `10-mos.conf`, and sshd
//! keeps the first value it obtains for a non-repeatable keyword.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use micad_settings::{AuthorizedKey, Settings, SshSettings};
use serde_json::json;

use super::Reconciler;
use super::systemd::{Systemd, UnitControl, is_active, is_enabled};
use crate::transient;
use crate::transient::write_atomically;

/// Unit implementing the SSH server.
const SSH_UNIT: &str = "ssh.service";
/// Drop-in rendered from `access.ssh`; `sshd_config` includes this directory.
const DEFAULT_DROP_IN: &str = "/etc/ssh/sshd_config.d/10-mos.conf";
/// Directory the per-account authorized-keys files are rendered into.
///
/// Under `/etc/ssh`, a STATE-backed bind mount (`etc-ssh.mount` binds
/// `/mnt/data/state/ssh` over it), so the files survive an A/B update; `/root` is on
/// the ephemeral filesystem and a key written there is gone on the next boot.
///
/// The static `05-mos-authorized-keys.conf` points sshd at
/// `/etc/ssh/authorized_keys.d/%u`, which sshd expands **per login user**, so
/// the file name inside this directory is the account name and nothing else.
const DEFAULT_AUTHORIZED_KEYS_DIR: &str = "/etc/ssh/authorized_keys.d";
/// Accounts this reconciler renders authorized keys for, in render order.
///
/// **One key set, rendered for every managed login account.** Keys are not
/// per-user in the settings tree, so every entry of `access.ssh.authorizedKeys`
/// is a root key as much as it is a `mos` key, and the web UI says so.
///
/// **A constant list, deliberately not a scan of `/etc/passwd`.** Scanning
/// would grant key access to any account a future package adds — a privilege
/// decision inherited from a dependency rather than made in review.
const MANAGED_LOGIN_ACCOUNTS: [&str; 2] = ["root", "mos"];
/// Environment variable overriding the drop-in path.
const DROP_IN_ENV: &str = "MOSD_SSHD_DROP_IN";
/// Environment variable overriding the authorized-keys directory.
///
/// A DIRECTORY, which is why the name says so: a value naming a single file
/// would be treated as a directory and render `<that file>/root`. Nothing in
/// the image sets it; the override exists for tests.
const AUTHORIZED_KEYS_DIR_ENV: &str = "MOSD_AUTHORIZED_KEYS_DIR";
/// Mode of the rendered drop-in: world-readable configuration, owner-writable.
const DROP_IN_MODE: u32 = 0o644;
/// Mode of the rendered authorized-keys file: owner-only. sshd reads it as
/// root, and nothing else has any business enumerating which keys open the
/// device.
const AUTHORIZED_KEYS_MODE: u32 = 0o600;
/// Mode of the authorized-keys directory when this reconciler creates it.
/// Traversable, because sshd checks the path, but writable only by root.
const AUTHORIZED_KEYS_DIR_MODE: u32 = 0o755;

/// Reconciler for the `access.ssh` settings subtree.
pub struct SshdReconciler<C: UnitControl> {
    drop_in_path: PathBuf,
    /// Directory the key files are rendered into: one file per
    /// [`MANAGED_LOGIN_ACCOUNTS`] entry, named for the account.
    authorized_keys_dir: PathBuf,
    /// Shadow file this reconciler's device operates on.
    ///
    /// Nothing here writes it. It is read — through
    /// [`transient::transient_password_active`], which looks for the marker
    /// beside it — because whether password authentication may be offered at
    /// all is a question about this exact path.
    shadow_path: PathBuf,
    control: C,
}

impl<C: UnitControl> SshdReconciler<C> {
    /// Create an sshd reconciler writing `drop_in_path` and one key file per
    /// managed account under `authorized_keys_dir`, tracking the shadow file at
    /// `shadow_path`, and driving `ssh.service` through `control`.
    ///
    /// Every path is a parameter so tests run entirely inside a temporary
    /// directory and never touch the host's sshd.
    pub fn new(
        drop_in_path: PathBuf,
        authorized_keys_dir: PathBuf,
        shadow_path: PathBuf,
        control: C,
    ) -> Self {
        Self {
            drop_in_path,
            authorized_keys_dir,
            shadow_path,
            control,
        }
    }
}

impl SshdReconciler<Systemd> {
    /// Production reconciler: paths from [`DROP_IN_ENV`],
    /// [`AUTHORIZED_KEYS_DIR_ENV`] and [`transient::SHADOW_ENV`] if set, else
    /// the system locations.
    ///
    /// The shadow path is resolved by [`transient::production_shadow_path`],
    /// not by a second copy of the constant and env var: this reconciler and
    /// the transient module must agree which file the marker sits beside.
    pub fn production() -> Self {
        let drop_in = std::env::var(DROP_IN_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_DROP_IN));
        let authorized_keys_dir = std::env::var(AUTHORIZED_KEYS_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_AUTHORIZED_KEYS_DIR));
        Self::new(
            drop_in,
            authorized_keys_dir,
            transient::production_shadow_path(),
            Systemd::new(),
        )
    }
}

/// Refuse a listen address that could not be one.
///
/// sshd's `ListenAddress` grammar also admits `host:port` and hostname forms,
/// but this appliance's settings model only ever offers IP addresses, so the
/// strictest parse that fits is the right boundary: an `IpAddr`, or an IPv6
/// literal in the brackets sshd requires when a port follows. Anything else —
/// in particular anything carrying whitespace or a newline — is rejected
/// before the renderer sees it.
///
/// # Errors
///
/// Returns an error naming the first entry that does not parse. The value is
/// an address, not a secret, so naming it is diagnostic rather than a leak.
fn validate_listen_addresses(addresses: &[String]) -> Result<()> {
    for address in addresses {
        let ok = address.parse::<std::net::IpAddr>().is_ok()
            || address.parse::<std::net::SocketAddr>().is_ok();
        if !ok {
            return Err(anyhow!(
                "access.ssh.listenAddresses entry {address:?} is not an IP address"
            ));
        }
    }
    Ok(())
}

/// Render the sshd drop-in for `ssh`.
///
/// Pure and deterministic: the same settings always produce the same bytes, so
/// a re-render can be compared against what is on disk to decide whether
/// anything changed. An empty `listen_addresses` emits no `ListenAddress`
/// directive at all, which is sshd's "listen on every address"; encoding
/// "listen nowhere" as the empty list would leave an operator who enables SSH
/// without naming an address with a running but unreachable server, and closure
/// is already expressed by `enabled: false`. `password_authentication` is the
/// effective value — the caller has already ANDed the setting with whether a
/// transient root password is active — passed as a parameter rather than read
/// again, so this function stays pure and its bytes stay comparable in a test.
/// No `AuthorizedKeysFile` directive is emitted: the static
/// `05-mos-authorized-keys.conf` owns that keyword and sorts first.
fn render_drop_in(ssh: &SshSettings, password_authentication: bool) -> String {
    let yes_no = |value: bool| if value { "yes" } else { "no" };
    let mut out = String::from("# Managed by micad from access.ssh. Do not edit.\n");
    out.push_str(&format!("Port {}\n", ssh.port));
    out.push_str(&format!(
        "PermitRootLogin {}\n",
        yes_no(ssh.permit_root_login)
    ));
    out.push_str(&format!(
        "PasswordAuthentication {}\n",
        yes_no(password_authentication)
    ));
    for address in &ssh.listen_addresses {
        out.push_str(&format!("ListenAddress {address}\n"));
    }
    out
}

/// Render the authorized-keys file for `keys`.
///
/// One entry per line in settings order — the operator's order, which is stable
/// across a load/store round-trip, so the render is deterministic and can be
/// compared against what is on disk. Each line is `<key>` or `<key> <comment>`.
///
/// An empty list renders an **empty file**, not an absent one. A removed key
/// has to stop working immediately, and "no file" versus "empty file" is a
/// distinction sshd does not need to make.
///
/// Pure: every value written here has already been through
/// [`micad_settings::validate_authorized_keys`] at the call site.
fn render_authorized_keys(keys: &[AuthorizedKey]) -> String {
    let mut out = String::new();
    for entry in keys {
        match &entry.comment {
            Some(comment) => out.push_str(&format!("{} {}\n", entry.key, comment)),
            None => out.push_str(&format!("{}\n", entry.key)),
        }
    }
    out
}

/// OpenSSH fingerprint of a canonical `<type> <blob>` key line.
///
/// The standard form: `SHA256:` followed by the unpadded base64 of the SHA-256
/// digest of the **decoded** blob — the same string `ssh-keygen -lf` prints.
///
/// Returns `None` when the line has no blob or the blob does not decode.
/// Publishing state must not fail a reconcile that already succeeded, so the
/// caller renders that as `null` rather than propagating an error.
fn fingerprint(key: &str) -> Option<String> {
    let blob = key.split(' ').nth(1)?;
    let decoded = micad_settings::decode_base64(blob)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &decoded);
    Some(format!(
        "SHA256:{}",
        micad_settings::encode_base64_nopad(digest.as_ref())
    ))
}

impl<C: UnitControl> SshdReconciler<C> {
    /// Render the drop-in and report whether its bytes changed.
    ///
    /// An unchanged render is not rewritten: the drop-in lives on STATE, and
    /// a rewrite that changes nothing still costs a flash write on every
    /// reconcile.
    fn apply_drop_in(&self, ssh: &SshSettings, password_authentication: bool) -> Result<bool> {
        let rendered = render_drop_in(ssh, password_authentication);
        if let Ok(current) = std::fs::read_to_string(&self.drop_in_path)
            && current == rendered
        {
            return Ok(false);
        }
        if let Some(directory) = self.drop_in_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        write_atomically(&self.drop_in_path, &rendered, DROP_IN_MODE, None)
            .with_context(|| format!("render {}", self.drop_in_path.display()))?;
        Ok(true)
    }

    /// Render the same key list into one file per entry of `accounts`.
    ///
    /// The caller has already re-validated the list, and it is rendered once
    /// before the first file is opened, so no two accounts can be written from
    /// different key lists. An unchanged file is not rewritten: these live on
    /// STATE, and a no-op rewrite still costs a flash write. No account is
    /// checked for existence — asking `/etc/passwd` would couple this
    /// reconciler to account state it does not own, failing in the window where
    /// the account and this render land out of order. A key file for an account
    /// that cannot log in is inert: `AuthorizedKeysFile
    /// /etc/ssh/authorized_keys.d/%u` expands from the user sshd is
    /// authenticating, so a file no login names is never read. `accounts` is a
    /// parameter and not a read of [`MANAGED_LOGIN_ACCOUNTS`] so a test can
    /// prove that against an account name no system could have.
    fn apply_authorized_keys(&self, keys: &[AuthorizedKey], accounts: &[&str]) -> Result<()> {
        let rendered = render_authorized_keys(keys);
        if !self.authorized_keys_dir.exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&self.authorized_keys_dir)
                .with_context(|| format!("create {}", self.authorized_keys_dir.display()))?;
            // Explicitly, rather than letting the umask decide: sshd refuses a
            // key file it reaches through a group- or world-writable directory.
            std::fs::set_permissions(
                &self.authorized_keys_dir,
                std::fs::Permissions::from_mode(AUTHORIZED_KEYS_DIR_MODE),
            )
            .with_context(|| format!("set mode on {}", self.authorized_keys_dir.display()))?;
        }
        for account in accounts {
            let path = self.authorized_keys_dir.join(account);
            if let Ok(current) = std::fs::read_to_string(&path)
                && current == rendered
            {
                continue;
            }
            write_atomically(&path, &rendered, AUTHORIZED_KEYS_MODE, None)
                .with_context(|| format!("render {}", path.display()))?;
        }
        Ok(())
    }

    /// Every path this reconciler renders keys to, in
    /// [`MANAGED_LOGIN_ACCOUNTS`] order.
    fn authorized_keys_paths(&self) -> Vec<String> {
        MANAGED_LOGIN_ACCOUNTS
            .iter()
            .map(|account| self.authorized_keys_dir.join(account).display().to_string())
            .collect()
    }

    /// Bring `ssh.service` to the state `ssh.enabled` asks for. Reads before it
    /// writes, so a system already in the target state gets no calls at all.
    ///
    /// A configuration-only change reloads and never restarts. A rewritten
    /// drop-in that nothing re-reads is a configuration that silently did not
    /// take effect, so an already-running sshd has to be told — but a restart
    /// tears the daemon down, and the moment that matters most is exactly the
    /// one where an operator is setting a transient root password over their
    /// existing SSH session in order to gain access. sshd re-reads its
    /// configuration on `SIGHUP`, so a reload applies the change while every
    /// established session keeps running. `KillMode` is not what keeps them
    /// alive: Debian's `openssh-server` ships `KillMode=process`, which would
    /// spare established sessions across a restart, but nothing in this image
    /// chose that value and a future package revision can change it silently.
    ///
    /// This depends on `ssh.service` carrying `ExecReload`, which is the Debian
    /// package's and not this repo's. A unit without it makes systemd refuse
    /// the job, and the refusal is surfaced with an error naming `ExecReload`
    /// and saying the change has not been applied. There is deliberately no
    /// fallback to `restart`: it would reintroduce the disconnect this reload
    /// prevents and hide the missing `ExecReload`. Enable/disable and
    /// start/stop are unit state changes rather than configuration changes and
    /// stay as they are; a unit that is not running but should be is started,
    /// never reloaded, because reloading a stopped daemon applies a
    /// configuration to nothing.
    async fn apply_unit(&self, ssh: &SshSettings, config_changed: bool) -> Result<()> {
        if ssh.enabled {
            if !is_enabled(&self.control.unit_file_state(SSH_UNIT).await?) {
                self.control.enable(SSH_UNIT).await?;
            }
            if is_active(&self.control.active_state(SSH_UNIT).await?) {
                if config_changed {
                    self.control.reload(SSH_UNIT).await.with_context(|| {
                        format!(
                            "reload {SSH_UNIT} after a configuration change: the unit may lack \
                             ExecReload, in which case sshd is still running the previous \
                             configuration and the rendered change has not been applied"
                        )
                    })?;
                }
            } else {
                self.control.start(SSH_UNIT).await?;
            }
        } else {
            if is_active(&self.control.active_state(SSH_UNIT).await?) {
                self.control.stop(SSH_UNIT).await?;
            }
            if is_enabled(&self.control.unit_file_state(SSH_UNIT).await?) {
                self.control.disable(SSH_UNIT).await?;
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl<C: UnitControl> Reconciler for SshdReconciler<C> {
    fn name(&self) -> &'static str {
        "sshd"
    }

    fn subtree(&self) -> &'static str {
        "access.ssh"
    }

    async fn apply(&self, settings: &Settings) -> Result<serde_json::Value> {
        let ssh = &settings.access.ssh;

        // Before anything is written. The settings file lives on STATE and is
        // editable by anything that can write STATE, so the parser is the
        // security boundary and this is the second place it has to hold. A
        // failure here aborts the whole apply with every rendered file exactly
        // as it was: silently dropping the offending entry and rendering the
        // rest would leave the operator looking at a key in the UI that grants
        // nothing.
        micad_settings::validate_authorized_keys(&ssh.authorized_keys)?;
        // Same boundary, same reasoning: each listen address is interpolated
        // verbatim onto a `ListenAddress` line in the drop-in, where a newline
        // is a new sshd directive. Requiring an actual address makes injection
        // structurally impossible rather than filtering for it.
        validate_listen_addresses(&ssh.listen_addresses)?;

        // Outside the settings tree, so it has to be read on every apply: the
        // operator setting a transient password changes no setting at all, and
        // the bus method that sets one calls back through `apply_all`.
        let transient_active = transient::transient_password_active(&self.shadow_path);
        // `password_authentication` below is the EFFECTIVE value — what sshd is
        // actually told. `passwordAuthenticationRequested` in the published
        // state is the raw setting. Two similarly-named keys, so: effective =
        // requested AND a transient password is really active.
        let password_authentication = ssh.password_authentication && transient_active;

        // No "did it change" comes back, and none is wanted: only the drop-in
        // forces sshd to re-read anything. sshd re-reads the authorized-keys
        // file on every authentication attempt, so a key added or removed takes
        // effect without touching the unit at all.
        self.apply_authorized_keys(&ssh.authorized_keys, &MANAGED_LOGIN_ACCOUNTS)?;
        let config_changed = self.apply_drop_in(ssh, password_authentication)?;
        self.apply_unit(ssh, config_changed).await?;

        let authorized_keys: Vec<serde_json::Value> = ssh
            .authorized_keys
            .iter()
            .map(|entry| {
                json!({
                    // Never the key material itself: this tree is served over
                    // D-Bus and read by apid, and a fingerprint is what an
                    // operator needs in order to recognise a key.
                    "fingerprint": fingerprint(&entry.key),
                    "comment": entry.comment,
                })
            })
            .collect();

        Ok(json!({
            "enabled": ssh.enabled,
            "port": ssh.port,
            "permitRootLogin": ssh.permit_root_login,
            "passwordAuthentication": password_authentication,
            "passwordAuthenticationRequested": ssh.password_authentication,
            "transientPasswordActive": transient_active,
            "listenAddresses": ssh.listen_addresses,
            "dropIn": self.drop_in_path.display().to_string(),
            // Plural: one key set is rendered to one file per managed
            // account, so a single path could only ever name one of them.
            "authorizedKeysPaths": self.authorized_keys_paths(),
            "authorizedKeys": authorized_keys,
            "unit": SSH_UNIT,
            "activeState": self.control.active_state(SSH_UNIT).await?,
            "unitFileState": self.control.unit_file_state(SSH_UNIT).await?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::super::systemd::mock::MockUnitControl;
    use super::*;

    const GOLDEN_DEFAULTS: &str = "# Managed by micad from access.ssh. Do not edit.\n\
        Port 22\nPermitRootLogin yes\nPasswordAuthentication yes\n";
    /// What `apply` writes for default settings with **no** transient password
    /// active: the same drop-in with password authentication gated off.
    const GOLDEN_DEFAULTS_GATED: &str = "# Managed by micad from access.ssh. Do not edit.\n\
        Port 22\nPermitRootLogin yes\nPasswordAuthentication no\n";
    const GOLDEN_LISTEN: &str = "# Managed by micad from access.ssh. Do not edit.\n\
        Port 2222\nPermitRootLogin no\nPasswordAuthentication no\n\
        ListenAddress 10.0.0.5\nListenAddress fd00::1\n";

    /// Three accounts, nine fields each, trailing newline — the shape of a
    /// Debian `/etc/shadow`. `root` starts out locked (`!`).
    const SHADOW: &str = "root:!:19000:0:99999:7:::\n\
        daemon:*:19000:0:99999:7:::\n\
        operator:$6$rounds=5000$abcd$efgh:19100:0:99999:7:::\n";
    const SHADOW_MODE: u32 = 0o640;

    fn ssh_settings(enabled: bool) -> SshSettings {
        SshSettings {
            enabled,
            ..SshSettings::default()
        }
    }

    fn settings_with(ssh: SshSettings) -> Settings {
        Settings {
            access: micad_settings::AccessSettings {
                ssh,
                ..micad_settings::AccessSettings::default()
            },
            ..Settings::default()
        }
    }

    /// Paths of a fixture, all of them under the tempdir.
    struct Paths {
        drop_in: PathBuf,
        /// Directory the per-account key files land in.
        keys_dir: PathBuf,
        /// The `root` account's key file — the path the goldens below are
        /// written against.
        keys: PathBuf,
        /// The `mos` account's key file, holding the same bytes as `keys`.
        mos_keys: PathBuf,
        shadow: PathBuf,
    }

    /// Fixture rooted entirely inside `dir`: a drop-in path that does not
    /// exist yet and a shadow file at [`SHADOW_MODE`].
    fn fixture(
        dir: &Path,
        active: &str,
        file_state: &str,
    ) -> (SshdReconciler<MockUnitControl>, Paths) {
        fixture_with(dir, MockUnitControl::new(active, file_state))
    }

    /// Same fixture, driving `control` — so a test can supply a mock that
    /// models a unit file without `ExecReload`.
    fn fixture_with(
        dir: &Path,
        control: MockUnitControl,
    ) -> (SshdReconciler<MockUnitControl>, Paths) {
        let keys_dir = dir.join("authorized_keys.d");
        let paths = Paths {
            drop_in: dir.join("sshd_config.d").join("10-mos.conf"),
            keys: keys_dir.join("root"),
            mos_keys: keys_dir.join("mos"),
            keys_dir,
            shadow: dir.join("shadow"),
        };
        std::fs::write(&paths.shadow, SHADOW).unwrap();
        std::fs::set_permissions(&paths.shadow, std::fs::Permissions::from_mode(SHADOW_MODE))
            .unwrap();
        let reconciler = SshdReconciler::new(
            paths.drop_in.clone(),
            paths.keys_dir.clone(),
            paths.shadow.clone(),
            control,
        );
        (reconciler, paths)
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    // ---- rendering --------------------------------------------------------

    #[test]
    fn empty_listen_addresses_emit_no_listen_address_directive() {
        let rendered = render_drop_in(&SshSettings::default(), true);

        assert_eq!(rendered, GOLDEN_DEFAULTS);
        assert!(
            !rendered.contains("ListenAddress"),
            "empty listenAddresses means listen on all, so no directive: {rendered}"
        );
    }

    #[test]
    fn each_listen_address_becomes_one_directive() {
        let rendered = render_drop_in(
            &SshSettings {
                enabled: true,
                port: 2222,
                permit_root_login: false,
                password_authentication: false,
                listen_addresses: vec!["10.0.0.5".to_string(), "fd00::1".to_string()],
                authorized_keys: Vec::new(),
            },
            false,
        );

        assert_eq!(rendered, GOLDEN_LISTEN);
        assert_eq!(rendered.matches("ListenAddress ").count(), 2);
    }

    #[tokio::test]
    async fn apply_rejects_a_listen_address_that_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let mut ssh = ssh_settings(true);
        // A newline here would land verbatim on the ListenAddress line, where
        // it starts a new sshd directive.
        ssh.listen_addresses = vec!["10.0.0.5\nPermitRootLogin yes".to_string()];

        let err = reconciler.apply(&settings_with(ssh)).await.unwrap_err();

        assert!(err.to_string().contains("listenAddresses"), "{err}");
        assert!(
            !paths.drop_in.exists(),
            "the reconcile must abort before anything is rendered"
        );
    }

    #[test]
    fn render_is_deterministic() {
        let ssh = SshSettings {
            enabled: true,
            port: 2222,
            permit_root_login: false,
            password_authentication: true,
            listen_addresses: vec!["10.0.0.5".to_string()],
            authorized_keys: Vec::new(),
        };

        assert_eq!(
            render_drop_in(&ssh, true),
            render_drop_in(&ssh.clone(), true)
        );
    }

    #[tokio::test]
    async fn apply_writes_the_golden_drop_in_creating_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        // GOLDEN_DEFAULTS_GATED, not GOLDEN_DEFAULTS: the fixture writes no
        // transient marker, so password authentication is gated off.
        assert_eq!(
            std::fs::read_to_string(&paths.drop_in).unwrap(),
            GOLDEN_DEFAULTS_GATED
        );
        assert_eq!(mode_of(&paths.drop_in), 0o644);
    }

    #[tokio::test]
    async fn apply_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        for directory in [
            paths.drop_in.parent().unwrap(),
            paths.keys.parent().unwrap(),
            dir.path(),
        ] {
            let leftovers: Vec<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().contains("micad-tmp"))
                .collect();
            assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        }
    }

    // ---- unit state -------------------------------------------------------

    #[tokio::test]
    async fn disabled_to_enabled_enables_then_starts() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable ssh.service".to_string(),
                "start ssh.service".to_string()
            ]
        );
        // Enablement is a unit STATE change, not a configuration change: it is
        // correctly not a reload, and a stopped unit could not be reloaded
        // anyway.
        assert!(
            !reconciler
                .control
                .calls()
                .contains(&"reload ssh.service".to_string())
        );
        assert_eq!(state["enabled"], json!(true));
        assert_eq!(state["activeState"], json!("active"));
        assert_eq!(state["unitFileState"], json!("enabled-runtime"));
        assert_eq!(state["unit"], json!("ssh.service"));
        assert_eq!(reconciler.name(), "sshd");
        assert_eq!(reconciler.subtree(), "access.ssh");
    }

    #[tokio::test]
    async fn enabled_to_disabled_stops_then_disables() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "active", "enabled");

        let state = reconciler
            .apply(&settings_with(ssh_settings(false)))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec![
                "stop ssh.service".to_string(),
                "disable ssh.service".to_string()
            ]
        );
        // Likewise a unit STATE change. Reloading a daemon on the way out is
        // meaningless.
        assert!(
            !reconciler
                .control
                .calls()
                .contains(&"reload ssh.service".to_string())
        );
        assert_eq!(state["enabled"], json!(false));
        assert_eq!(state["activeState"], json!("inactive"));
        assert_eq!(state["unitFileState"], json!("disabled"));
    }

    #[tokio::test]
    async fn already_running_and_enabled_needs_no_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(paths.drop_in.parent().unwrap()).unwrap();
        std::fs::write(&paths.drop_in, GOLDEN_DEFAULTS_GATED).unwrap();

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "converged system got calls: {:?}",
            reconciler.control.calls()
        );
    }

    #[tokio::test]
    async fn already_stopped_and_disabled_needs_no_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with(ssh_settings(false)))
            .await
            .unwrap();

        assert!(
            reconciler.control.calls().is_empty(),
            "converged system got calls: {:?}",
            reconciler.control.calls()
        );
    }

    #[tokio::test]
    async fn reapplying_the_same_settings_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let settings = settings_with(ssh_settings(true));

        reconciler.apply(&settings).await.unwrap();
        let after_first = reconciler.control.calls();
        // A marker the reconciler would clobber if it rewrote the file: the
        // renderer always produces mode 0644.
        std::fs::set_permissions(&paths.drop_in, std::fs::Permissions::from_mode(0o600)).unwrap();

        reconciler.apply(&settings).await.unwrap();

        assert_eq!(reconciler.control.calls(), after_first);
        assert_eq!(
            mode_of(&paths.drop_in),
            0o600,
            "an unchanged drop-in must not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.drop_in).unwrap(),
            GOLDEN_DEFAULTS_GATED
        );
    }

    /// The central guard: a configuration-only change reloads a running sshd
    /// and must never restart it, so the operator's established session
    /// survives by construction rather than by `KillMode`'s grace.
    #[tokio::test]
    async fn changing_the_config_of_a_running_sshd_reloads_it_and_never_restarts_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        std::fs::create_dir_all(paths.drop_in.parent().unwrap()).unwrap();
        std::fs::write(&paths.drop_in, GOLDEN_DEFAULTS_GATED).unwrap();

        let state = reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                port: 2222,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        assert_eq!(
            reconciler.control.calls(),
            vec!["reload ssh.service".to_string()]
        );
        assert!(
            !reconciler
                .control
                .calls()
                .contains(&"restart ssh.service".to_string()),
            "a restart would drop the operator's live session: {:?}",
            reconciler.control.calls()
        );
        assert!(
            std::fs::read_to_string(&paths.drop_in)
                .unwrap()
                .contains("Port 2222\n")
        );
        assert_eq!(state["port"], json!(2222));
    }

    #[tokio::test]
    async fn changing_the_config_of_a_stopped_sshd_starts_it_without_reloading() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "enabled");
        std::fs::create_dir_all(paths.drop_in.parent().unwrap()).unwrap();
        std::fs::write(&paths.drop_in, GOLDEN_DEFAULTS_GATED).unwrap();

        reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                port: 2222,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        // A start reads the drop-in on the way up, so the change is applied
        // without a reload — and reloading a stopped daemon would apply the
        // configuration to nothing.
        assert_eq!(
            reconciler.control.calls(),
            vec!["start ssh.service".to_string()]
        );
        assert!(
            std::fs::read_to_string(&paths.drop_in)
                .unwrap()
                .contains("Port 2222\n")
        );
    }

    #[tokio::test]
    async fn a_unit_without_exec_reload_fails_loudly_and_is_never_restarted_instead() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture_with(
            dir.path(),
            MockUnitControl::with_failing_reload("active", "enabled"),
        );
        std::fs::create_dir_all(paths.drop_in.parent().unwrap()).unwrap();
        std::fs::write(&paths.drop_in, GOLDEN_DEFAULTS_GATED).unwrap();

        let error = reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                port: 2222,
                ..SshSettings::default()
            }))
            .await
            .unwrap_err();

        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("ExecReload"),
            "the error must send the next reader at the unit file: {rendered}"
        );
        assert!(
            rendered.contains("has not been applied"),
            "the error must say the change did not take effect: {rendered}"
        );
        assert!(
            !reconciler
                .control
                .calls()
                .contains(&"restart ssh.service".to_string()),
            "a silent restart fallback reintroduces the disconnect and hides the \
             missing ExecReload: {:?}",
            reconciler.control.calls()
        );
        assert_eq!(
            reconciler.control.calls(),
            vec!["reload ssh.service".to_string()],
            "the reload is attempted once and nothing follows it"
        );
    }

    // ---- the device password does not reach the shadow file ---------------

    #[tokio::test]
    async fn no_reconcile_writes_anything_into_the_shadow_file() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&paths.shadow).unwrap(),
            SHADOW,
            "the root entry is not this reconciler's to write any more"
        );
        assert_eq!(mode_of(&paths.shadow), SHADOW_MODE);
        assert!(
            state.get("rootPassword").is_none(),
            "the removed device-password write must not still be advertised: {state}"
        );
    }

    #[tokio::test]
    async fn a_shadow_file_that_is_missing_or_broken_does_not_stop_the_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        std::fs::remove_file(&paths.shadow).unwrap();

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .expect("the shadow file is not an input to this reconciler");

        assert!(
            !paths.shadow.exists(),
            "a missing shadow file must not be created"
        );
        assert_eq!(
            reconciler.control.calls(),
            vec![
                "enable ssh.service".to_string(),
                "start ssh.service".to_string()
            ]
        );
    }

    // ---- containment ------------------------------------------------------

    #[tokio::test]
    async fn every_path_the_reconciler_writes_stays_inside_the_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        for path in [&paths.drop_in, &paths.keys, &paths.mos_keys, &paths.shadow] {
            assert!(
                path.starts_with(dir.path()),
                "{} escapes the tempdir",
                path.display()
            );
        }
        assert_eq!(state["dropIn"], json!(paths.drop_in.display().to_string()));
        assert_ne!(state["dropIn"], json!(DEFAULT_DROP_IN));
        assert_eq!(
            state["authorizedKeysPaths"],
            json!([
                paths.keys.display().to_string(),
                paths.mos_keys.display().to_string(),
            ])
        );
        assert!(
            !state["authorizedKeysPaths"]
                .to_string()
                .contains(DEFAULT_AUTHORIZED_KEYS_DIR),
            "a test must never render into the real /etc/ssh"
        );
    }

    // ---- real keys, committed as test constants ---------------------------
    //
    // Generated with `ssh-keygen` purely for this test. Public keys are not
    // secrets, and these correspond to no device: the private halves were
    // discarded at generation time and exist nowhere.

    /// `ssh-keygen -t ed25519 -C rfct-034-test-ed25519`, verbatim.
    const REAL_ED25519_LINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIL99V7xPTOP3jZjnbVPM7xC+ckwzkOQPalUpsvtPzYo8 rfct-034-test-ed25519";
    /// `ssh-keygen -t rsa -b 2048 -C rfct-034-test-rsa`, verbatim.
    const REAL_RSA_LINE: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDT2F3imgGgI+xGNSQI+0alU1qRwyU3gCc8wU6msXSzZsVc8OYlg4VIqxsV/GLpBmgRz5lGoxjTT2TU0t1VwaMs845NqRIWzpG88ohD1LMn7RnUrNTxf4syFuvmELmYstqMfc6Q6rApqFoA6023Rl2orgd8N3SQ2wPAw8Rk9OLwim9/R7tX8C8FTbnMtepzTvOUNGTDAaKYhTZZnZpsGCwKa9f2aWyaS2XqLwn9uWpmHRUAkV10l45W2rLhnceejwwHotlZUIAFt8rlmS1ojRaLWqECVAuO5CDTt64KLLRniw8yHIYsWkeVsHZXCxq+J7oUVI3ogOSYs1M4I2eFCccD rfct-034-test-rsa";
    /// A second Ed25519 key, so the multi-key golden holds three distinct keys.
    const REAL_ED25519_SECOND_LINE: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILFM+HTH5h41h/zyK4CwjXx9E1l8Nwks1NaywRMiSsEP rfct-034-test-ed25519-second";

    /// Fingerprints as reported by `ssh-keygen -lf <file>` for the three keys
    /// above, copied from that command's output. Comparing this module's
    /// fingerprint against a constant that came from OpenSSH is the point: a
    /// fingerprint function checked only against itself proves nothing.
    const REAL_ED25519_FINGERPRINT: &str = "SHA256:HrgN3GLi6Mop2uSRjgOoxImM8zRkFmgqCKoeGD9QOaM";
    const REAL_RSA_FINGERPRINT: &str = "SHA256:zv0xTYuVTo5pFpcl/svzzz/vJFvoguWxKlghlXQS1bE";
    const REAL_ED25519_SECOND_FINGERPRINT: &str =
        "SHA256:d7yiR/zCsNFh8WmU6CGLWEG5vE06icIelqVoNc8TT2E";

    /// The canonical `<type> <blob>` half of a full `ssh-keygen` line.
    fn canonical(line: &str) -> String {
        let mut fields = line.splitn(3, ' ');
        let key_type = fields.next().unwrap();
        let blob = fields.next().unwrap();
        format!("{key_type} {blob}")
    }

    /// An [`AuthorizedKey`] built directly, bypassing the parser — the shape a
    /// corrupted settings file on STATE would present.
    fn raw_key(key: &str, comment: Option<&str>) -> AuthorizedKey {
        AuthorizedKey {
            key: key.to_string(),
            comment: comment.map(str::to_string),
        }
    }

    fn settings_with_keys(keys: Vec<AuthorizedKey>) -> Settings {
        settings_with(SshSettings {
            enabled: true,
            authorized_keys: keys,
            ..SshSettings::default()
        })
    }

    /// Set a transient password marker beside `shadow`, the way
    /// `transient::set_transient_root_password` does.
    fn set_marker(shadow: &Path) {
        std::fs::write(
            crate::transient::transient_marker_path(shadow),
            "$2b$12$notarealhashjustnonempty\n",
        )
        .unwrap();
    }

    // ---- golden renders ---------------------------------------------------

    #[test]
    fn an_empty_list_renders_an_empty_file() {
        assert_eq!(render_authorized_keys(&[]), "");
    }

    #[test]
    fn one_key_without_a_comment_renders_one_bare_line() {
        let rendered = render_authorized_keys(&[raw_key(&canonical(REAL_ED25519_LINE), None)]);

        assert_eq!(rendered, format!("{}\n", canonical(REAL_ED25519_LINE)));
    }

    #[test]
    fn one_key_with_a_comment_renders_key_space_comment() {
        let rendered = render_authorized_keys(&[raw_key(
            &canonical(REAL_ED25519_LINE),
            Some("laptop@example"),
        )]);

        assert_eq!(
            rendered,
            format!("{} laptop@example\n", canonical(REAL_ED25519_LINE))
        );
    }

    #[test]
    fn three_keys_render_in_settings_order_mixing_commented_and_bare() {
        let rendered = render_authorized_keys(&[
            raw_key(&canonical(REAL_ED25519_LINE), Some("first")),
            raw_key(&canonical(REAL_RSA_LINE), None),
            raw_key(&canonical(REAL_ED25519_SECOND_LINE), Some("third")),
        ]);

        assert_eq!(
            rendered,
            format!(
                "{} first\n{}\n{} third\n",
                canonical(REAL_ED25519_LINE),
                canonical(REAL_RSA_LINE),
                canonical(REAL_ED25519_SECOND_LINE)
            )
        );
        assert_eq!(rendered.lines().count(), 3);
    }

    #[test]
    fn rendering_the_same_keys_twice_gives_identical_bytes() {
        let keys = vec![
            raw_key(&canonical(REAL_ED25519_LINE), Some("first")),
            raw_key(&canonical(REAL_RSA_LINE), None),
        ];

        assert_eq!(
            render_authorized_keys(&keys),
            render_authorized_keys(&keys.clone())
        );
    }

    // ---- a real ssh-keygen key round-trips --------------------------------

    /// Genuine `ssh-keygen` output survives the parser and comes back out of
    /// the render byte-identical — asserted against a rendered file rather
    /// than against pasted key material.
    #[test]
    fn a_real_ssh_keygen_line_parses_canonicalises_and_renders_back_identically() {
        for line in [REAL_ED25519_LINE, REAL_RSA_LINE, REAL_ED25519_SECOND_LINE] {
            let parsed = micad_settings::parse_authorized_key(line)
                .unwrap_or_else(|err| panic!("real ssh-keygen line rejected: {line}: {err}"));

            assert_eq!(parsed.key, canonical(line), "comment leaked into `key`");
            assert_eq!(
                parsed.comment.as_deref(),
                Some(line.splitn(3, ' ').nth(2).unwrap())
            );
            assert_eq!(
                render_authorized_keys(std::slice::from_ref(&parsed)),
                format!("{line}\n"),
                "rendered line differs from the ssh-keygen line it came from"
            );
            micad_settings::validate_authorized_keys(std::slice::from_ref(&parsed)).unwrap();
        }
    }

    /// The same round-trip on a key generated at test time, so the committed
    /// constants above cannot quietly drift away from what OpenSSH emits.
    ///
    /// Skipped when `ssh-keygen` is absent, which is why the committed-constant
    /// test above exists as well: this file never becomes a silent no-op.
    #[test]
    fn a_freshly_generated_key_round_trips_when_ssh_keygen_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh");
        let generated = std::process::Command::new("ssh-keygen")
            .args([
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "fresh@rfct-034",
                "-f",
            ])
            .arg(&path)
            .status();
        let Ok(status) = generated else {
            eprintln!("ssh-keygen not on this host; committed-constant round-trip still ran");
            return;
        };
        assert!(status.success(), "ssh-keygen failed");

        let line = std::fs::read_to_string(path.with_extension("pub")).unwrap();
        let line = line.trim_end_matches('\n');
        let parsed = micad_settings::parse_authorized_key(line).unwrap();

        assert_eq!(parsed.key, canonical(line));
        assert_eq!(parsed.comment.as_deref(), Some("fresh@rfct-034"));
        assert_eq!(
            render_authorized_keys(std::slice::from_ref(&parsed)),
            format!("{line}\n")
        );
    }

    // ---- fingerprints agree with ssh-keygen -lf ---------------------------

    #[test]
    fn fingerprints_match_what_ssh_keygen_reports() {
        for (line, expected) in [
            (REAL_ED25519_LINE, REAL_ED25519_FINGERPRINT),
            (REAL_RSA_LINE, REAL_RSA_FINGERPRINT),
            (REAL_ED25519_SECOND_LINE, REAL_ED25519_SECOND_FINGERPRINT),
        ] {
            assert_eq!(fingerprint(&canonical(line)).as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_key_with_no_decodable_blob_has_no_fingerprint() {
        assert_eq!(fingerprint("ssh-ed25519"), None);
        assert_eq!(fingerprint("ssh-ed25519 not!base64"), None);
    }

    // ---- validation failures leave the file untouched ---------------------

    /// Apply once with a good key so there is a rendered file to protect, then
    /// apply `bad` and assert the failure changed nothing.
    async fn assert_bad_keys_leave_the_file_untouched(bad: Vec<AuthorizedKey>, what: &str) {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let good = vec![raw_key(&canonical(REAL_ED25519_LINE), Some("keep-me"))];
        reconciler
            .apply(&settings_with_keys(good))
            .await
            .expect("the good apply must succeed");
        let before = std::fs::read(&paths.keys).unwrap();
        assert!(!before.is_empty(), "nothing was rendered to protect");
        assert_eq!(
            std::fs::read(&paths.mos_keys).unwrap(),
            before,
            "the good apply must have rendered both accounts alike"
        );

        let error = reconciler
            .apply(&settings_with_keys(bad))
            .await
            .expect_err(&format!("{what} must fail the apply"));

        for path in [&paths.keys, &paths.mos_keys] {
            assert_eq!(
                std::fs::read(path).unwrap(),
                before,
                "{what}: {} must be byte-identical after a failed apply",
                path.display()
            );
        }
        let message = format!("{error:#}");
        assert!(
            message.contains("access.ssh.authorizedKeys"),
            "{what}: error should name the setting: {message}"
        );
    }

    #[tokio::test]
    async fn a_newline_embedded_in_the_key_field_fails_and_changes_nothing() {
        let injected = format!(
            "{}\nssh-ed25519 AAAAsomethingelse",
            canonical(REAL_ED25519_LINE)
        );
        assert_bad_keys_leave_the_file_untouched(
            vec![raw_key(&injected, None)],
            "a newline in the key field",
        )
        .await;
    }

    #[tokio::test]
    async fn a_comment_smuggled_into_the_key_field_fails_and_changes_nothing() {
        assert_bad_keys_leave_the_file_untouched(
            vec![raw_key(REAL_ED25519_LINE, None)],
            "a comment inside the key field",
        )
        .await;
    }

    #[tokio::test]
    async fn a_duplicate_key_pair_fails_and_changes_nothing() {
        assert_bad_keys_leave_the_file_untouched(
            vec![
                raw_key(&canonical(REAL_ED25519_LINE), Some("one")),
                raw_key(&canonical(REAL_ED25519_LINE), Some("two")),
            ],
            "a duplicated key",
        )
        .await;
    }

    #[tokio::test]
    async fn the_error_from_an_invalid_list_names_the_offending_index() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        let error = reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_ED25519_LINE), None),
                raw_key("ssh-ed25519 !!!!", None),
            ]))
            .await
            .expect_err("an unparseable entry must fail the apply");

        let message = format!("{error:#}");
        assert!(
            message.contains("entry 1"),
            "error should name the offending index: {message}"
        );
    }

    /// The positive direction of the same guard: a valid list renders, and it
    /// overwrites whatever was there before rather than appending to it.
    #[tokio::test]
    async fn a_valid_list_renders_and_overwrites_the_previous_content() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_ED25519_LINE), Some("first")),
                raw_key(&canonical(REAL_RSA_LINE), None),
            ]))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&paths.keys).unwrap(),
            format!(
                "{} first\n{}\n",
                canonical(REAL_ED25519_LINE),
                canonical(REAL_RSA_LINE)
            )
        );

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_SECOND_LINE),
                None,
            )]))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&paths.keys).unwrap(),
            format!("{}\n", canonical(REAL_ED25519_SECOND_LINE)),
            "the removed keys must be gone, not appended to"
        );
    }

    /// Removing every key empties the file rather than deleting it: an absent
    /// file and an empty file mean the same thing to sshd, and a key removed
    /// has to stop working immediately either way.
    #[tokio::test]
    async fn removing_every_key_empties_the_file_without_deleting_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        reconciler
            .apply(&settings_with_keys(Vec::new()))
            .await
            .unwrap();

        assert!(paths.keys.exists(), "the file must not be deleted");
        assert_eq!(std::fs::read_to_string(&paths.keys).unwrap(), "");
    }

    // ---- per-character hostile input in the comment -----------------------

    #[tokio::test]
    async fn each_control_character_in_a_comment_fails_and_changes_nothing() {
        for (ch, name) in [
            ('\0', "NUL"),
            ('\n', "line feed"),
            ('\r', "carriage return"),
            ('\t', "tab"),
            ('\u{7f}', "delete"),
        ] {
            assert_bad_keys_leave_the_file_untouched(
                vec![raw_key(
                    &canonical(REAL_ED25519_LINE),
                    Some(&format!("host{ch}name")),
                )],
                &format!("a {name} in the comment"),
            )
            .await;
        }
    }

    /// The other direction: shell metacharacters are ordinary comment text.
    /// The rendered file is read by sshd, not by a shell, and a guard that
    /// rejected these would refuse comments operators really write.
    #[tokio::test]
    async fn shell_metacharacters_in_a_comment_render_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let comment = "a$b`c\\d\"e;f";

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                Some(comment),
            )]))
            .await
            .expect("shell metacharacters are legitimate comment text");

        assert_eq!(
            std::fs::read_to_string(&paths.keys).unwrap(),
            format!("{} {comment}\n", canonical(REAL_ED25519_LINE))
        );
    }

    // ---- PasswordAuthentication gating ------------------------------------

    #[tokio::test]
    async fn without_a_transient_password_password_authentication_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                password_authentication: true,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        assert!(
            std::fs::read_to_string(&paths.drop_in)
                .unwrap()
                .contains("PasswordAuthentication no\n"),
            "root is locked, so the method cannot succeed and must not be offered"
        );
        assert_eq!(state["passwordAuthentication"], json!(false));
        assert_eq!(state["passwordAuthenticationRequested"], json!(true));
        assert_eq!(state["transientPasswordActive"], json!(false));
    }

    #[tokio::test]
    async fn a_marker_appearing_between_two_applies_turns_passwords_on_and_reloads_sshd() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        let settings = settings_with(SshSettings {
            enabled: true,
            password_authentication: true,
            ..SshSettings::default()
        });

        let before = reconciler.apply(&settings).await.unwrap();
        assert_eq!(before["passwordAuthentication"], json!(false));
        assert!(
            std::fs::read_to_string(&paths.drop_in)
                .unwrap()
                .contains("PasswordAuthentication no\n")
        );

        // Nothing in the settings tree changes here — this is exactly what
        // `SetTransientRootPassword` does before it calls `apply_all`.
        set_marker(&paths.shadow);
        let after = reconciler.apply(&settings).await.unwrap();

        assert_eq!(after["passwordAuthentication"], json!(true));
        assert_eq!(after["transientPasswordActive"], json!(true));
        assert!(
            std::fs::read_to_string(&paths.drop_in)
                .unwrap()
                .contains("PasswordAuthentication yes\n")
        );
        assert!(
            reconciler
                .control
                .calls()
                .contains(&"reload ssh.service".to_string()),
            "sshd must re-read the flipped drop-in: {:?}",
            reconciler.control.calls()
        );
        assert!(
            !reconciler
                .control
                .calls()
                .contains(&"restart ssh.service".to_string()),
            "this is the path where the operator is setting a password over the \
             very session a restart would drop: {:?}",
            reconciler.control.calls()
        );
    }

    #[tokio::test]
    async fn a_marker_does_not_turn_passwords_on_when_the_setting_says_no() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        set_marker(&paths.shadow);

        let state = reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                password_authentication: false,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        assert_eq!(
            state["passwordAuthentication"],
            json!(false),
            "the gate is an AND of setting and marker, not an OR"
        );
        assert_eq!(state["transientPasswordActive"], json!(true));
        assert_eq!(state["passwordAuthenticationRequested"], json!(false));
        assert!(
            std::fs::read_to_string(&paths.drop_in)
                .unwrap()
                .contains("PasswordAuthentication no\n")
        );
    }

    #[tokio::test]
    async fn an_empty_marker_does_not_count_as_a_transient_password() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        std::fs::write(crate::transient::transient_marker_path(&paths.shadow), "").unwrap();

        let state = reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                password_authentication: true,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        assert_eq!(state["transientPasswordActive"], json!(false));
        assert_eq!(state["passwordAuthentication"], json!(false));
    }

    #[test]
    fn the_rendered_drop_in_never_carries_an_authorized_keys_file_directive() {
        // The static 05-mos-authorized-keys.conf owns that keyword and sorts
        // first; sshd keeps the first value it sees, so emitting it here would
        // be dead text that a later reader would try to "fix".
        for effective in [true, false] {
            assert!(
                !render_drop_in(&SshSettings::default(), effective).contains("AuthorizedKeysFile")
            );
        }
    }

    // ---- published state --------------------------------------------------

    #[tokio::test]
    async fn published_state_carries_fingerprints_and_never_key_material() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_ED25519_LINE), Some("laptop")),
                raw_key(&canonical(REAL_RSA_LINE), None),
            ]))
            .await
            .unwrap();

        assert_eq!(
            state["authorizedKeysPaths"],
            json!([
                paths.keys.display().to_string(),
                paths.mos_keys.display().to_string(),
            ]),
            "every rendered path is named, in managed-account order"
        );
        assert_eq!(
            state["authorizedKeys"],
            json!([
                {"fingerprint": REAL_ED25519_FINGERPRINT, "comment": "laptop"},
                {"fingerprint": REAL_RSA_FINGERPRINT, "comment": null},
            ]),
            "fingerprints in render order, comment null when the key has none"
        );

        let serialised = state.to_string();
        for line in [REAL_ED25519_LINE, REAL_RSA_LINE] {
            let blob = line.split(' ').nth(1).unwrap();
            assert!(
                !serialised.contains(blob),
                "key material must never reach the published state tree"
            );
        }
    }

    #[tokio::test]
    async fn published_state_reports_an_empty_key_list_as_an_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "disabled");

        let state = reconciler
            .apply(&settings_with_keys(Vec::new()))
            .await
            .unwrap();

        assert_eq!(state["authorizedKeys"], json!([]));
    }

    // ---- permissions ------------------------------------------------------

    #[tokio::test]
    async fn every_rendered_key_file_is_0600_in_a_0755_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        assert_eq!(mode_of(&paths.keys), 0o600);
        assert_eq!(mode_of(&paths.mos_keys), 0o600);
        assert_eq!(mode_of(&paths.keys_dir), 0o755);
    }

    #[tokio::test]
    async fn an_unchanged_key_list_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let settings = settings_with_keys(vec![raw_key(&canonical(REAL_ED25519_LINE), None)]);
        reconciler.apply(&settings).await.unwrap();
        // A marker the reconciler would clobber if it rewrote the file: the
        // renderer always produces mode 0600.
        std::fs::set_permissions(&paths.keys, std::fs::Permissions::from_mode(0o640)).unwrap();

        reconciler.apply(&settings).await.unwrap();

        assert_eq!(
            mode_of(&paths.keys),
            0o640,
            "an unchanged key file must not be rewritten"
        );
    }

    // ---- one key set, rendered for every managed login account ------------
    //
    // `AuthorizedKeysFile /etc/ssh/authorized_keys.d/%u` is expanded per login
    // user, so a file exists for each account micad manages and all of them
    // carry the same list. These tests hold the plural property; the goldens
    // above still hold the `root` file byte-for-byte.

    /// The central guard. One validated list, two files, identical bytes.
    #[tokio::test]
    async fn one_key_list_renders_byte_identical_files_for_every_managed_account() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_ED25519_LINE), Some("laptop")),
                raw_key(&canonical(REAL_RSA_LINE), None),
            ]))
            .await
            .unwrap();

        let expected = format!(
            "{} laptop\n{}\n",
            canonical(REAL_ED25519_LINE),
            canonical(REAL_RSA_LINE)
        );
        for path in [&paths.keys, &paths.mos_keys] {
            assert!(path.exists(), "{} was not rendered", path.display());
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                expected,
                "{} does not carry the operator's key list",
                path.display()
            );
        }
        assert_eq!(
            std::fs::read(&paths.keys).unwrap(),
            std::fs::read(&paths.mos_keys).unwrap(),
            "one key set means byte-identical files"
        );
    }

    /// The file set is exactly the constant list — no more, no fewer. A scan of
    /// `/etc/passwd` would render for whatever accounts the host happens to
    /// have, which is the thing the constant exists to prevent.
    #[tokio::test]
    async fn only_the_managed_accounts_get_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        let mut rendered: Vec<String> = std::fs::read_dir(&paths.keys_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        rendered.sort();
        let mut expected: Vec<String> = MANAGED_LOGIN_ACCOUNTS
            .iter()
            .map(|a| a.to_string())
            .collect();
        expected.sort();
        assert_eq!(rendered, expected);
        assert_eq!(MANAGED_LOGIN_ACCOUNTS, ["root", "mos"]);
    }

    /// `mos` may not exist as an account when this ships: a sibling task adds
    /// it, and the two can merge in either order. Rendering a key file for an
    /// account that does not exist must therefore be an ordinary success —
    /// sshd only ever opens the file named by the user it is authenticating, so
    /// a file no login can name is inert rather than wrong.
    ///
    /// The account name here is one no system could plausibly carry, so this
    /// proves the render is unconditional rather than merely lucky about what
    /// the build host happens to have in `/etc/passwd`.
    #[test]
    fn a_key_file_renders_for_an_account_that_does_not_exist() {
        const ABSENT: &str = "no-such-account-rfct053";
        let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        assert!(
            !passwd.contains(ABSENT),
            "the fixture account must genuinely not exist for this test to mean anything"
        );
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply_authorized_keys(&[raw_key(&canonical(REAL_ED25519_LINE), None)], &[ABSENT])
            .expect("a render for a missing account must not fail the reconcile");

        assert_eq!(
            std::fs::read_to_string(paths.keys_dir.join(ABSENT)).unwrap(),
            format!("{}\n", canonical(REAL_ED25519_LINE)),
            "the render is the same whether or not the account exists"
        );
    }

    /// An empty list empties EVERY account file. Two empty files, not two
    /// deletions and not one of each: the golden rule above, per account.
    #[tokio::test]
    async fn an_empty_list_empties_every_account_file_without_deleting_any() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        reconciler
            .apply(&settings_with_keys(Vec::new()))
            .await
            .unwrap();

        for path in [&paths.keys, &paths.mos_keys] {
            assert!(path.exists(), "{} must not be deleted", path.display());
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                "",
                "{} must be empty",
                path.display()
            );
        }
    }

    /// A key the operator removes stops granting access to every account in the
    /// same reconcile. A rewrite that reached only `root` would leave the key
    /// live for `mos`, which is the removal silently not happening.
    #[tokio::test]
    async fn removing_one_key_of_three_rewrites_every_account_file_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_ED25519_LINE), Some("laptop")),
                raw_key(&canonical(REAL_RSA_LINE), None),
                raw_key(&canonical(REAL_ED25519_SECOND_LINE), Some("phone")),
            ]))
            .await
            .unwrap();

        reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_ED25519_LINE), Some("laptop")),
                raw_key(&canonical(REAL_ED25519_SECOND_LINE), Some("phone")),
            ]))
            .await
            .unwrap();

        let expected = format!(
            "{} laptop\n{} phone\n",
            canonical(REAL_ED25519_LINE),
            canonical(REAL_ED25519_SECOND_LINE)
        );
        let removed_blob = canonical(REAL_RSA_LINE);
        for path in [&paths.keys, &paths.mos_keys] {
            let content = std::fs::read_to_string(path).unwrap();
            assert_eq!(content, expected, "{} was not rewritten", path.display());
            assert!(
                !content.contains(&removed_blob),
                "the removed key still grants access through {}",
                path.display()
            );
        }
    }

    /// Fail-loud, per account, with no partial application. Validation runs
    /// before the first file is opened, so a rejected list cannot update one
    /// account while another keeps the previous keys — the ordering hazard this
    /// task introduced by writing more than one file.
    #[tokio::test]
    async fn a_validation_failure_updates_neither_account() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                Some("keep-me"),
            )]))
            .await
            .unwrap();
        let before = std::fs::read(&paths.keys).unwrap();
        assert_eq!(std::fs::read(&paths.mos_keys).unwrap(), before);

        // Valid first entry, rejected second: a renderer that wrote as it went
        // would have put the good prefix somewhere before failing.
        reconciler
            .apply(&settings_with_keys(vec![
                raw_key(&canonical(REAL_RSA_LINE), Some("new")),
                raw_key("ssh-ed25519 not-base64!!", None),
            ]))
            .await
            .expect_err("an invalid list must fail the apply");

        for path in [&paths.keys, &paths.mos_keys] {
            assert_eq!(
                std::fs::read(path).unwrap(),
                before,
                "{} changed during a failed apply",
                path.display()
            );
        }
    }

    /// No temporary file survives either write. Both files share one directory,
    /// so a leftover from the second write would be visible here too.
    #[tokio::test]
    async fn rendering_every_account_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(&paths.keys_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains("micad-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }
}
