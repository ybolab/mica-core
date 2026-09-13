//! SSH access reconciler: renders dropbear's arguments and the managed
//! accounts' authorized keys from `access.ssh` and drives `dropbear.service`.
//! Three system effects, in this order:
//!
//! - `~/.ssh/authorized_keys` of every managed login account is rendered from
//!   `access.ssh.authorizedKeys`, after the list has been re-validated: one
//!   file per account, every one holding the same key list, owned by the
//!   account, 0600 in a 0700 `~/.ssh`. That is dropbear's default key file;
//!   the pinned build also takes `-D <directory>` for another location, and
//!   micad does not pass it.
//! - `/run/mica/dropbear.env` is rendered from `access.ssh`: exactly one line,
//!   `DROPBEAR_ARGS="..."`, which `dropbear.service` (mica-system) reads through
//!   `EnvironmentFile=` and appends to its `ExecStart`. dropbear has no
//!   configuration file; every policy is a flag.
//! - `dropbear.service` is brought to the state `access.ssh.enabled` asks for,
//!   and restarted when the arguments changed under a running server.
//!
//! Configuration before service start, deliberately: a dropbear started
//! against stale arguments listens on the wrong port, or accepts an
//! authentication method the operator has already turned off.
//!
//! The device password does not reach PAM (dropbear has none; it reads the
//! shadow file). The credential of record for shell access is an SSH public
//! key, or a transient password set through `crate::transient` that the next
//! boot clears; nothing reads `secrets/device-password` back, because a hash in
//! the root shadow entry would be a password on a fielded device that never
//! expires. Password authentication is gated on that transient password: `-s`
//! (no password logins) is rendered unless the setting AND
//! `transient::transient_password_active` are both true, so root ships locked
//! and stays locked unless a transient password is active.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use micad_settings::{AuthorizedKey, Settings, SshSettings};
use rustix::fs::{AtFlags, Mode, OFlags};
use serde_json::json;

use super::Reconciler;
use super::systemd::{Systemd, UnitControl, is_active, is_enabled};
use crate::transient;

/// Unit implementing the SSH server, shipped by mica-system.
const SSH_UNIT: &str = "dropbear.service";
/// `ActiveState` of a unit that exited unsuccessfully, possibly inside its
/// start-limit window.
const FAILED_STATE: &str = "failed";
/// Environment file carrying dropbear's arguments.
const DEFAULT_ENVIRONMENT_FILE: &str = "/run/mica/dropbear.env";
/// Account database the managed accounts' ids and homes are read from.
const DEFAULT_PASSWD: &str = "/etc/passwd";
/// Accounts this reconciler renders authorized keys for, in render order.
///
/// **One key set, rendered for every managed login account.** Keys are not
/// per-user in the settings tree, so every entry of `access.ssh.authorizedKeys`
/// is a root key as much as it is a `mos` key, and the web UI says so.
///
/// **A constant list, deliberately not a scan of `/etc/passwd`.** Scanning
/// would grant key access to any account a future package adds — a privilege
/// decision inherited from a dependency rather than made in review. The
/// account database is read only for where each of these accounts lives.
const MANAGED_LOGIN_ACCOUNTS: [&str; 2] = ["root", "mos"];
/// Environment variable overriding the environment file path.
const ENVIRONMENT_FILE_ENV: &str = "MOSD_DROPBEAR_ENV";
/// Environment variable overriding the account database path. Nothing in the
/// image sets it; the override exists for tests.
const PASSWD_ENV: &str = "MOSD_PASSWD_PATH";
/// Mode of the environment file: world-readable arguments, owner-writable.
const ENVIRONMENT_FILE_MODE: u32 = 0o644;
/// Mode of `~/.ssh`.
const SSH_DIR_MODE: u32 = 0o700;
/// Mode of `~/.ssh/authorized_keys`: owner-only. Nothing else has any business
/// enumerating which keys open the device.
const AUTHORIZED_KEYS_MODE: u32 = 0o600;
/// Name of the key file inside `~/.ssh`.
const AUTHORIZED_KEYS: &str = "authorized_keys";
/// Temporary sibling a key file is written to before the rename.
const AUTHORIZED_KEYS_TEMP: &str = ".authorized_keys.micad-tmp";
/// Most `-p` listeners dropbear binds (`DROPBEAR_MAX_PORTS`); it ignores the
/// rest without a word.
const MAX_LISTEN_ADDRESSES: usize = 10;

/// Reconciler for the `access.ssh` settings subtree.
pub struct SshdReconciler<C: UnitControl> {
    environment_path: PathBuf,
    /// Account database the managed accounts' uid, gid and home come from.
    passwd_path: PathBuf,
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
    /// Create a reconciler writing dropbear's arguments to `environment_path`
    /// and each managed account's key file under the home `passwd_path` names,
    /// tracking the shadow file at `shadow_path`, and driving
    /// `dropbear.service` through `control`.
    ///
    /// Every path is a parameter so tests run entirely inside a temporary
    /// directory and never touch the host's SSH server.
    pub fn new(
        environment_path: PathBuf,
        passwd_path: PathBuf,
        shadow_path: PathBuf,
        control: C,
    ) -> Self {
        Self {
            environment_path,
            passwd_path,
            shadow_path,
            control,
        }
    }
}

impl SshdReconciler<Systemd> {
    /// Production reconciler: paths from [`ENVIRONMENT_FILE_ENV`],
    /// [`PASSWD_ENV`] and [`transient::SHADOW_ENV`] if set, else the system
    /// locations.
    ///
    /// The shadow path is resolved by [`transient::production_shadow_path`],
    /// not by a second copy of the constant and env var: this reconciler and
    /// the transient module must agree which file the marker sits beside.
    pub fn production() -> Self {
        let from_env = |name: &str, default: &str| {
            std::env::var(name)
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(default))
        };
        Self::new(
            from_env(ENVIRONMENT_FILE_ENV, DEFAULT_ENVIRONMENT_FILE),
            from_env(PASSWD_ENV, DEFAULT_PASSWD),
            transient::production_shadow_path(),
            Systemd::new(),
        )
    }
}

/// Refuse a listen address that could not be one, and more of them than
/// dropbear binds.
///
/// The settings model only ever offers IP addresses, optionally with a port,
/// so the strictest parse that fits is the right boundary: an `IpAddr`, or a
/// `SocketAddr` (an IPv6 one in brackets). Anything else — in particular
/// anything carrying whitespace, a quote or a newline, which would split or end
/// the `DROPBEAR_ARGS` value — is rejected before the renderer sees it.
///
/// # Errors
///
/// Returns an error naming the first entry that does not parse, or the count
/// when it exceeds [`MAX_LISTEN_ADDRESSES`]. The value is an address, not a
/// secret, so naming it is diagnostic rather than a leak.
fn validate_listen_addresses(addresses: &[String]) -> Result<()> {
    if addresses.len() > MAX_LISTEN_ADDRESSES {
        bail!(
            "access.ssh.listenAddresses holds {} entries; dropbear binds at most \
             {MAX_LISTEN_ADDRESSES} and would silently ignore the rest",
            addresses.len()
        );
    }
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

/// One `-p` listener for `address`: `addr:port`, IPv6 in brackets, and an
/// address that carries its own port keeps it. Called only on a validated
/// address.
fn listener(address: &str, port: u16) -> String {
    match address.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, port).to_string(),
        Err(_) => address.to_string(),
    }
}

/// Render `/run/mica/dropbear.env` for `ssh`.
///
/// Pure and deterministic: the same settings always produce the same bytes, so
/// a re-render can be compared against what is on disk to decide whether
/// anything changed. Exactly one line, `DROPBEAR_ARGS="..."`:
///
/// - `-p <addr>:<port>` per listen address, in tree order. An empty list is
///   `-p <port>`, which is dropbear's "every address": encoding "listen
///   nowhere" as the empty list would leave an operator who enables SSH
///   without naming an address with a running but unreachable server, and
///   closure is already expressed by `enabled: false`. A `-p` is always
///   present; mica-system's unit refuses a start without one.
/// - `-s` (no password logins) unless `password_authentication`, the
///   EFFECTIVE value — the caller has already ANDed the setting with whether a
///   transient root password is active — passed as a parameter rather than
///   read again, so this function stays pure.
/// - `-w` (no root logins) when `permitRootLogin` is false, which is what
///   OpenSSH's `PermitRootLogin no` meant: root refused by every method.
///
/// `-g` (no root password logins) is never rendered: the only password this
/// device can have is the transient ROOT password, so `-g` would either turn
/// off exactly that login or repeat `-s`.
fn render_environment(ssh: &SshSettings, password_authentication: bool) -> String {
    let mut args: Vec<String> = if ssh.listen_addresses.is_empty() {
        vec![format!("-p {}", ssh.port)]
    } else {
        ssh.listen_addresses
            .iter()
            .map(|address| format!("-p {}", listener(address, ssh.port)))
            .collect()
    };
    if !password_authentication {
        args.push("-s".to_string());
    }
    if !ssh.permit_root_login {
        args.push("-w".to_string());
    }
    format!("DROPBEAR_ARGS=\"{}\"\n", args.join(" "))
}

/// Render the authorized-keys file for `keys`.
///
/// One entry per line in settings order — the operator's order, which is stable
/// across a load/store round-trip, so the render is deterministic and can be
/// compared against what is on disk. Each line is `<key>` or `<key> <comment>`.
///
/// An empty list renders an **empty file**, not an absent one. A removed key
/// has to stop working immediately, and "no file" versus "empty file" is a
/// distinction dropbear does not need to make.
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

/// A managed account as the account database describes it.
#[derive(Debug, Clone, PartialEq)]
struct Account {
    name: String,
    uid: u32,
    gid: u32,
    home: PathBuf,
}

/// The `/etc/passwd` entry named `name`, if there is a well-formed one.
fn find_account(passwd: &str, name: &str) -> Option<Account> {
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() != 7 || fields[0] != name {
            return None;
        }
        Some(Account {
            name: name.to_string(),
            uid: fields[2].parse().ok()?,
            gid: fields[3].parse().ok()?,
            home: PathBuf::from(fields[5]),
        })
    })
}

/// An account whose `~/.ssh` is open and ready for its key file.
struct KeyTarget {
    account: Account,
    ssh_dir: OwnedFd,
}

impl KeyTarget {
    fn path(&self) -> PathBuf {
        self.account.home.join(".ssh").join(AUTHORIZED_KEYS)
    }
}

fn errno(err: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from(err)
}

/// Open `account`'s home and `~/.ssh` for a key file dropbear will honour.
///
/// dropbear checks, at every login, that `~/.ssh` and the home are owned by
/// the account or root and writable by neither group nor others, and refuses
/// every key otherwise. A home that fails that check is an error here, so the
/// failure is on the apply and not a key in the UI that grants nothing. It is
/// not repaired: the home is the operator's, and loosening or tightening it is
/// not this reconciler's call.
///
/// `~/.ssh` is created 0700 when missing and brought to the account's
/// ownership and 0700 when not. Everything below the home is reached through
/// directory descriptors with `O_NOFOLLOW`, because the home is writable by
/// the account and micad is root: a `~/.ssh` that is a symbolic link is
/// refused instead of followed to wherever it points.
///
/// Returns `None` for a home that does not exist — the account cannot log in
/// with a key either way.
fn open_key_target(account: Account) -> Result<Option<KeyTarget>> {
    let home = match rustix::fs::open(
        &account.home,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(home) => home,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(err) => {
            return Err(errno(err)).with_context(|| format!("open {}", account.home.display()));
        }
    };
    let stat = rustix::fs::fstat(&home)
        .map_err(errno)
        .with_context(|| format!("stat {}", account.home.display()))?;
    if (stat.st_uid != account.uid && stat.st_uid != 0) || stat.st_mode & 0o022 != 0 {
        bail!(
            "{} (home of {}) must be owned by the account or root and not be group- or \
             world-writable, or dropbear refuses every key under it",
            account.home.display(),
            account.name
        );
    }

    let ssh_path = account.home.join(".ssh");
    match rustix::fs::mkdirat(&home, ".ssh", Mode::from_raw_mode(SSH_DIR_MODE)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(err) => {
            return Err(errno(err)).with_context(|| format!("create {}", ssh_path.display()));
        }
    }
    let ssh_dir = rustix::fs::openat(
        &home,
        ".ssh",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno)
    .with_context(|| format!("open {} (a symbolic link is refused)", ssh_path.display()))?;
    let stat = rustix::fs::fstat(&ssh_dir)
        .map_err(errno)
        .with_context(|| format!("stat {}", ssh_path.display()))?;
    if stat.st_uid != account.uid || stat.st_gid != account.gid {
        rustix::fs::fchown(
            &ssh_dir,
            Some(rustix::fs::Uid::from_raw(account.uid)),
            Some(rustix::fs::Gid::from_raw(account.gid)),
        )
        .map_err(errno)
        .with_context(|| format!("set owner on {}", ssh_path.display()))?;
    }
    if stat.st_mode & 0o7777 != SSH_DIR_MODE {
        rustix::fs::fchmod(&ssh_dir, Mode::from_raw_mode(SSH_DIR_MODE))
            .map_err(errno)
            .with_context(|| format!("set mode on {}", ssh_path.display()))?;
    }
    Ok(Some(KeyTarget { account, ssh_dir }))
}

/// Write `rendered` as `target`'s key file unless it already holds exactly
/// that.
///
/// A temporary file created exclusively and without following links, owned by
/// the account and 0600 before it has a name dropbear reads, then renamed over
/// the key file — which replaces a symbolic link planted there rather than
/// writing through it. An unchanged file is not rewritten: homes live on DATA,
/// and a no-op rewrite still costs a flash write.
fn write_key_file(target: &KeyTarget, rendered: &str) -> Result<()> {
    let path = target.path();
    // Non-blocking, and only a regular file of the rendered length is read:
    // the name is in a directory the account controls, and a FIFO there would
    // otherwise hold the open until somebody wrote to it.
    if let Ok(existing) = rustix::fs::openat(
        &target.ssh_dir,
        AUTHORIZED_KEYS,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) && rustix::fs::fstat(&existing).is_ok_and(|stat| {
        rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::RegularFile
            && u64::try_from(stat.st_size).ok() == Some(rendered.len() as u64)
    }) {
        let mut current = String::new();
        if std::fs::File::from(existing)
            .read_to_string(&mut current)
            .is_ok()
            && current == rendered
        {
            return Ok(());
        }
    }

    let temp_path = path.with_file_name(AUTHORIZED_KEYS_TEMP);
    // A leftover from an interrupted run, or anything planted under the name.
    let _ = rustix::fs::unlinkat(&target.ssh_dir, AUTHORIZED_KEYS_TEMP, AtFlags::empty());
    let temp = rustix::fs::openat(
        &target.ssh_dir,
        AUTHORIZED_KEYS_TEMP,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(AUTHORIZED_KEYS_MODE),
    )
    .map_err(errno)
    .with_context(|| format!("create {}", temp_path.display()))?;
    // Mode and owner explicitly and on the descriptor: the umask applied to
    // the create, and the name is not trusted between two calls.
    rustix::fs::fchmod(&temp, Mode::from_raw_mode(AUTHORIZED_KEYS_MODE))
        .map_err(errno)
        .with_context(|| format!("set mode on {}", temp_path.display()))?;
    rustix::fs::fchown(
        &temp,
        Some(rustix::fs::Uid::from_raw(target.account.uid)),
        Some(rustix::fs::Gid::from_raw(target.account.gid)),
    )
    .map_err(errno)
    .with_context(|| format!("set owner on {}", temp_path.display()))?;
    let mut file = std::fs::File::from(temp);
    file.write_all(rendered.as_bytes())
        .with_context(|| format!("write {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("flush {}", temp_path.display()))?;
    drop(file);
    rustix::fs::renameat(
        &target.ssh_dir,
        AUTHORIZED_KEYS_TEMP,
        &target.ssh_dir,
        AUTHORIZED_KEYS,
    )
    .map_err(errno)
    .with_context(|| format!("rename {} to {}", temp_path.display(), path.display()))?;
    Ok(())
}

impl<C: UnitControl> SshdReconciler<C> {
    /// Render the environment file and report whether its bytes changed.
    ///
    /// An unchanged render is not rewritten, so a reconcile that changes
    /// nothing does not look like a change and restart the server.
    fn apply_environment(&self, ssh: &SshSettings, password_authentication: bool) -> Result<bool> {
        let rendered = render_environment(ssh, password_authentication);
        if let Ok(current) = std::fs::read_to_string(&self.environment_path)
            && current == rendered
        {
            return Ok(false);
        }
        if let Some(directory) = self.environment_path.parent() {
            std::fs::create_dir_all(directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        crate::fswrite::write_config(&self.environment_path, &rendered, ENVIRONMENT_FILE_MODE)
            .with_context(|| format!("render {}", self.environment_path.display()))?;
        Ok(true)
    }

    /// Every managed account the key list can be rendered for, opened and
    /// checked before any key file is written.
    ///
    /// The whole set first, so a home dropbear would refuse fails the apply
    /// with no account's keys changed — the same no-partial-application rule
    /// the key validation holds. An account missing from the account database,
    /// or whose home does not exist, is skipped with a warning: it cannot log
    /// in with a key either way, and failing here would keep SSH closed for
    /// every other account. `accounts` is a parameter and not a read of
    /// [`MANAGED_LOGIN_ACCOUNTS`] so a test can use a name no system has.
    fn key_targets(&self, accounts: &[&str]) -> Result<Vec<KeyTarget>> {
        let passwd = std::fs::read_to_string(&self.passwd_path)
            .with_context(|| format!("read {}", self.passwd_path.display()))?;
        let mut targets = Vec::new();
        for name in accounts {
            let Some(account) = find_account(&passwd, name) else {
                tracing::warn!(
                    account = name,
                    passwd = %self.passwd_path.display(),
                    "managed SSH account is not in the account database; no keys rendered for it"
                );
                continue;
            };
            let home = account.home.clone();
            match open_key_target(account)? {
                Some(target) => targets.push(target),
                None => tracing::warn!(
                    account = name,
                    home = %home.display(),
                    "managed SSH account has no home directory; no keys rendered for it"
                ),
            }
        }
        Ok(targets)
    }

    /// Bring `dropbear.service` to the state `ssh.enabled` asks for. Reads
    /// before it writes, so a system already in the target state gets no calls
    /// at all.
    ///
    /// A changed environment file under a running server is a RESTART: dropbear
    /// reads its arguments once, at start, and re-reads nothing on `SIGHUP`, so
    /// a rewritten file that nothing restarts is a configuration that silently
    /// did not take effect. The established sessions survive it only because
    /// mica-system's `dropbear.service` sets `KillMode=process` — dropbear
    /// forks one process per connection into the unit's cgroup, and the
    /// default kill mode would end them all, including the session of an
    /// operator setting a transient root password in order to get in. A unit
    /// that is not running but should be is started, never restarted; a start
    /// reads the new file on the way up. Enable/disable and start/stop are unit
    /// state changes rather than configuration changes and stay as they are.
    async fn apply_unit(&self, ssh: &SshSettings, config_changed: bool) -> Result<()> {
        if ssh.enabled {
            if !is_enabled(&self.control.unit_file_state(SSH_UNIT).await?) {
                self.control.enable(SSH_UNIT).await?;
            }
            let active_state = self.control.active_state(SSH_UNIT).await?;
            if is_active(&active_state) {
                if config_changed {
                    self.control.restart(SSH_UNIT).await.with_context(|| {
                        format!(
                            "restart {SSH_UNIT} after its arguments changed: the running server \
                             still has the previous arguments"
                        )
                    })?;
                }
            } else {
                // The mqtt.rs start-limit amendment: dropbear.service restarts
                // on failure, so a server that could not bind can be `failed`
                // inside its start-limit window, where systemd refuses start
                // jobs. Clear the failure first -- only when there is one, so
                // the call log still says which apply rescued it.
                if active_state == FAILED_STATE {
                    self.control.reset_failed(SSH_UNIT).await?;
                }
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
        // The live-state key and the apply queue's name for this reconciler,
        // which apid and the API harness read; it names the SSH channel, not
        // the daemon behind it.
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
        // verbatim into the quoted `DROPBEAR_ARGS` value, where whitespace
        // splits an argument and a quote or newline ends the value. Requiring
        // an actual address makes injection structurally impossible rather
        // than filtering for it.
        validate_listen_addresses(&ssh.listen_addresses)?;

        // Outside the settings tree, so it has to be read on every apply: the
        // operator setting a transient password changes no setting at all, and
        // the bus method that sets one calls back through `apply_all`.
        let transient_active = transient::transient_password_active(&self.shadow_path);
        // `password_authentication` below is the EFFECTIVE value — what dropbear
        // is actually told. `passwordAuthenticationRequested` in the published
        // state is the raw setting. Two similarly-named keys, so: effective =
        // requested AND a transient password is really active.
        let password_authentication = ssh.password_authentication && transient_active;

        // Every home is checked before any key file is written. No "did it
        // change" comes back from the keys, and none is wanted: dropbear opens
        // the key file on every authentication attempt, so a key added or
        // removed takes effect without touching the unit at all.
        let targets = self.key_targets(&MANAGED_LOGIN_ACCOUNTS)?;
        let rendered = render_authorized_keys(&ssh.authorized_keys);
        for target in &targets {
            write_key_file(target, &rendered)?;
        }
        let config_changed = self.apply_environment(ssh, password_authentication)?;
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
        let authorized_keys_paths: Vec<String> = targets
            .iter()
            .map(|target| target.path().display().to_string())
            .collect();

        Ok(json!({
            "enabled": ssh.enabled,
            "port": ssh.port,
            "permitRootLogin": ssh.permit_root_login,
            "passwordAuthentication": password_authentication,
            "passwordAuthenticationRequested": ssh.password_authentication,
            "transientPasswordActive": transient_active,
            "listenAddresses": ssh.listen_addresses,
            "environmentFile": self.environment_path.display().to_string(),
            // Plural: one key set is rendered to one file per managed account
            // that exists, in managed-account order.
            "authorizedKeysPaths": authorized_keys_paths,
            "authorizedKeys": authorized_keys,
            "unit": SSH_UNIT,
            "activeState": self.control.active_state(SSH_UNIT).await?,
            "unitFileState": self.control.unit_file_state(SSH_UNIT).await?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    use super::super::systemd::mock::MockUnitControl;
    use super::*;

    const GOLDEN_DEFAULTS: &str = "DROPBEAR_ARGS=\"-p 22\"\n";
    /// What `apply` writes for default settings with **no** transient password
    /// active: the same arguments with password logins turned off.
    const GOLDEN_DEFAULTS_GATED: &str = "DROPBEAR_ARGS=\"-p 22 -s\"\n";
    const GOLDEN_LISTEN: &str = "DROPBEAR_ARGS=\"-p 10.0.0.5:2222 -p [fd00::1]:2222 -s -w\"\n";

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
        environment: PathBuf,
        passwd: PathBuf,
        root_home: PathBuf,
        mos_home: PathBuf,
        /// The `root` account's key file — the path the goldens below are
        /// written against.
        keys: PathBuf,
        /// The `mos` account's key file, holding the same bytes as `keys`.
        mos_keys: PathBuf,
        shadow: PathBuf,
    }

    /// The uid and gid this test runs as. Both managed accounts are given
    /// them, so the ownership the reconciler sets is one any test runner may
    /// set, root or not.
    fn own_ids() -> (u32, u32) {
        (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        )
    }

    /// An `/etc/passwd` naming `root` and `mos` at the given homes, with a
    /// system account between them.
    fn passwd_for(root_home: &Path, mos_home: &Path) -> String {
        let (uid, gid) = own_ids();
        format!(
            "root:x:{uid}:{gid}:root:{}:/bin/bash\n\
             daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
             mos:x:{uid}:{gid}:mos operator:{}:/bin/bash\n",
            root_home.display(),
            mos_home.display()
        )
    }

    /// Fixture rooted entirely inside `dir`: an environment file that does
    /// not exist yet, two 0755 homes with no `.ssh`, and a shadow file at
    /// [`SHADOW_MODE`].
    fn fixture(
        dir: &Path,
        active: &str,
        file_state: &str,
    ) -> (SshdReconciler<MockUnitControl>, Paths) {
        let root_home = dir.join("root");
        let mos_home = dir.join("home").join("mos");
        let paths = Paths {
            environment: dir.join("run").join("mica").join("dropbear.env"),
            passwd: dir.join("passwd"),
            keys: root_home.join(".ssh").join("authorized_keys"),
            mos_keys: mos_home.join(".ssh").join("authorized_keys"),
            root_home,
            mos_home,
            shadow: dir.join("shadow"),
        };
        for home in [&paths.root_home, &paths.mos_home] {
            std::fs::create_dir_all(home).unwrap();
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(&paths.passwd, passwd_for(&paths.root_home, &paths.mos_home)).unwrap();
        std::fs::write(&paths.shadow, SHADOW).unwrap();
        std::fs::set_permissions(&paths.shadow, std::fs::Permissions::from_mode(SHADOW_MODE))
            .unwrap();
        let reconciler = SshdReconciler::new(
            paths.environment.clone(),
            paths.passwd.clone(),
            paths.shadow.clone(),
            MockUnitControl::new(active, file_state),
        );
        (reconciler, paths)
    }

    /// Write `contents` as the environment file, as a previous apply would
    /// have.
    fn preexisting_environment(paths: &Paths, contents: &str) {
        std::fs::create_dir_all(paths.environment.parent().unwrap()).unwrap();
        std::fs::write(&paths.environment, contents).unwrap();
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn calls(reconciler: &SshdReconciler<MockUnitControl>) -> Vec<String> {
        reconciler.control.calls()
    }

    // ---- rendering --------------------------------------------------------

    #[test]
    fn empty_listen_addresses_render_one_listener_on_every_address() {
        let rendered = render_environment(&SshSettings::default(), true);

        assert_eq!(rendered, GOLDEN_DEFAULTS);
    }

    #[test]
    fn each_listen_address_becomes_one_listener() {
        let rendered = render_environment(
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
        assert_eq!(rendered.matches("-p ").count(), 2);
    }

    #[test]
    fn an_address_that_carries_its_own_port_keeps_it() {
        let rendered = render_environment(
            &SshSettings {
                listen_addresses: vec!["10.0.0.5:2200".to_string(), "[fd00::1]:2201".to_string()],
                ..SshSettings::default()
            },
            true,
        );

        assert_eq!(
            rendered,
            "DROPBEAR_ARGS=\"-p 10.0.0.5:2200 -p [fd00::1]:2201\"\n"
        );
    }

    /// The contract with mica-system's unit: one line, one variable, and
    /// always a `-p` (its ExecStartPre refuses a start without one).
    #[test]
    fn every_render_is_exactly_one_dropbear_args_line_with_a_listener() {
        for permit_root_login in [true, false] {
            for password in [true, false] {
                for listen in [vec![], vec!["192.0.2.1".to_string()]] {
                    let rendered = render_environment(
                        &SshSettings {
                            permit_root_login,
                            listen_addresses: listen,
                            ..SshSettings::default()
                        },
                        password,
                    );
                    assert_eq!(rendered.lines().count(), 1, "{rendered}");
                    assert!(rendered.starts_with("DROPBEAR_ARGS=\"-p "), "{rendered}");
                    assert!(rendered.ends_with("\"\n"), "{rendered}");
                }
            }
        }
    }

    /// `permitRootLogin = false` refuses root by every method, which is `-w`;
    /// `-g` would refuse only root's password, and the only password this
    /// device has is root's transient one.
    #[test]
    fn root_login_refusal_is_w_and_g_is_never_rendered() {
        for permit_root_login in [true, false] {
            for password in [true, false] {
                let rendered = render_environment(
                    &SshSettings {
                        permit_root_login,
                        ..SshSettings::default()
                    },
                    password,
                );
                assert_eq!(rendered.contains(" -w"), !permit_root_login, "{rendered}");
                assert_eq!(rendered.contains(" -s"), !password, "{rendered}");
                assert!(!rendered.contains("-g"), "{rendered}");
            }
        }
    }

    #[tokio::test]
    async fn apply_rejects_a_listen_address_that_is_not_one() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let mut ssh = ssh_settings(true);
        // A space or a quote here would land verbatim inside DROPBEAR_ARGS,
        // where it splits the value into arguments dropbear never asked for.
        ssh.listen_addresses = vec!["10.0.0.5 -B\" -R".to_string()];

        let err = reconciler.apply(&settings_with(ssh)).await.unwrap_err();

        assert!(err.to_string().contains("listenAddresses"), "{err}");
        assert!(
            !paths.environment.exists(),
            "the reconcile must abort before anything is rendered"
        );
        assert!(!paths.keys.exists());
        assert!(calls(&reconciler).is_empty());
    }

    #[tokio::test]
    async fn apply_rejects_more_listen_addresses_than_dropbear_binds() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let mut ssh = ssh_settings(true);
        ssh.listen_addresses = (1..=11).map(|i| format!("192.0.2.{i}")).collect();

        let err = reconciler.apply(&settings_with(ssh)).await.unwrap_err();

        assert!(err.to_string().contains("at most 10"), "{err}");
        assert!(!paths.environment.exists());
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
            render_environment(&ssh, true),
            render_environment(&ssh.clone(), true)
        );
    }

    #[tokio::test]
    async fn apply_writes_the_golden_environment_file_creating_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        // GOLDEN_DEFAULTS_GATED, not GOLDEN_DEFAULTS: the fixture writes no
        // transient marker, so password authentication is gated off.
        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            GOLDEN_DEFAULTS_GATED
        );
        assert_eq!(mode_of(&paths.environment), 0o644);
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
            paths.environment.parent().unwrap(),
            paths.keys.parent().unwrap(),
            paths.mos_keys.parent().unwrap(),
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
            calls(&reconciler),
            vec![
                "enable dropbear.service".to_string(),
                "start dropbear.service".to_string()
            ]
        );
        // Enablement is a unit STATE change, not a configuration change: the
        // start reads the freshly written arguments, so nothing restarts.
        assert!(!calls(&reconciler).contains(&"restart dropbear.service".to_string()));
        assert_eq!(state["enabled"], json!(true));
        assert_eq!(state["activeState"], json!("active"));
        assert_eq!(state["unitFileState"], json!("enabled-runtime"));
        assert_eq!(state["unit"], json!("dropbear.service"));
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
            calls(&reconciler),
            vec![
                "stop dropbear.service".to_string(),
                "disable dropbear.service".to_string()
            ]
        );
        assert_eq!(state["enabled"], json!(false));
        assert_eq!(state["activeState"], json!("inactive"));
        assert_eq!(state["unitFileState"], json!("disabled"));
    }

    #[tokio::test]
    async fn already_running_and_enabled_needs_no_calls() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        preexisting_environment(&paths, GOLDEN_DEFAULTS_GATED);

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert!(
            calls(&reconciler).is_empty(),
            "converged system got calls: {:?}",
            calls(&reconciler)
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
            calls(&reconciler).is_empty(),
            "converged system got calls: {:?}",
            calls(&reconciler)
        );
    }

    #[tokio::test]
    async fn reapplying_the_same_settings_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let settings = settings_with(ssh_settings(true));

        reconciler.apply(&settings).await.unwrap();
        let after_first = calls(&reconciler);
        // A marker the reconciler would clobber if it rewrote the file: the
        // renderer always produces mode 0644.
        std::fs::set_permissions(&paths.environment, std::fs::Permissions::from_mode(0o600))
            .unwrap();

        reconciler.apply(&settings).await.unwrap();

        assert_eq!(calls(&reconciler), after_first);
        assert_eq!(
            mode_of(&paths.environment),
            0o600,
            "an unchanged environment file must not be rewritten"
        );
        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            GOLDEN_DEFAULTS_GATED
        );
    }

    /// dropbear reads its arguments only at start, so changed arguments under
    /// a running server are a restart — and nothing else: the unit is neither
    /// stopped nor re-enabled on the way.
    #[tokio::test]
    async fn changing_the_arguments_of_a_running_dropbear_restarts_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        preexisting_environment(&paths, GOLDEN_DEFAULTS_GATED);

        let state = reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                port: 2222,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        assert_eq!(
            calls(&reconciler),
            vec!["restart dropbear.service".to_string()]
        );
        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            "DROPBEAR_ARGS=\"-p 2222 -s\"\n"
        );
        assert_eq!(state["port"], json!(2222));
    }

    #[tokio::test]
    async fn changing_the_arguments_of_a_stopped_dropbear_starts_it_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "enabled");
        preexisting_environment(&paths, GOLDEN_DEFAULTS_GATED);

        reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                port: 2222,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        // A start reads the file on the way up, so the change is applied
        // without a restart.
        assert_eq!(
            calls(&reconciler),
            vec!["start dropbear.service".to_string()]
        );
        assert!(
            std::fs::read_to_string(&paths.environment)
                .unwrap()
                .contains("-p 2222")
        );
    }

    /// dropbear.service fails its start without `/run/mica/dropbear.env`
    /// (the EnvironmentFile is required) and would accept only what the key
    /// files say at the moment of the first login, so every unit operation
    /// has to see the finished files. Observed from inside the unit control,
    /// at the moment each call is made.
    #[tokio::test]
    async fn arguments_and_keys_are_on_disk_before_any_unit_operation() {
        struct Observing {
            inner: MockUnitControl,
            watched: Vec<PathBuf>,
            seen: std::sync::Mutex<Vec<(String, bool)>>,
        }
        impl Observing {
            fn observe(&self, verb: &str, unit: &str) {
                let ready = self
                    .watched
                    .iter()
                    .all(|path| std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0));
                self.seen
                    .lock()
                    .unwrap()
                    .push((format!("{verb} {unit}"), ready));
            }
        }
        #[async_trait::async_trait]
        impl UnitControl for Observing {
            async fn active_state(&self, unit: &str) -> Result<String> {
                self.inner.active_state(unit).await
            }
            async fn unit_file_state(&self, unit: &str) -> Result<String> {
                self.inner.unit_file_state(unit).await
            }
            async fn start(&self, unit: &str) -> Result<()> {
                self.observe("start", unit);
                self.inner.start(unit).await
            }
            async fn stop(&self, unit: &str) -> Result<()> {
                self.observe("stop", unit);
                self.inner.stop(unit).await
            }
            async fn restart(&self, unit: &str) -> Result<()> {
                self.observe("restart", unit);
                self.inner.restart(unit).await
            }
            async fn reset_failed(&self, unit: &str) -> Result<()> {
                self.observe("reset-failed", unit);
                self.inner.reset_failed(unit).await
            }
            async fn enable(&self, unit: &str) -> Result<()> {
                self.observe("enable", unit);
                self.inner.enable(unit).await
            }
            async fn disable(&self, unit: &str) -> Result<()> {
                self.observe("disable", unit);
                self.inner.disable(unit).await
            }
            async fn daemon_reload(&self) -> Result<()> {
                self.inner.daemon_reload().await
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (probe, paths) = fixture(dir.path(), "inactive", "disabled");
        drop(probe);
        let reconciler = SshdReconciler::new(
            paths.environment.clone(),
            paths.passwd.clone(),
            paths.shadow.clone(),
            Observing {
                inner: MockUnitControl::new("inactive", "disabled"),
                watched: vec![
                    paths.environment.clone(),
                    paths.keys.clone(),
                    paths.mos_keys.clone(),
                ],
                seen: std::sync::Mutex::new(Vec::new()),
            },
        );
        let keys = vec![raw_key(&canonical(REAL_ED25519_LINE), None)];

        reconciler
            .apply(&settings_with_keys(keys.clone()))
            .await
            .unwrap();
        // And a change under the now running server: restart, same rule.
        reconciler
            .apply(&settings_with(SshSettings {
                enabled: true,
                port: 2222,
                authorized_keys: keys,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        let seen = reconciler.control.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                ("enable dropbear.service".to_string(), true),
                ("start dropbear.service".to_string(), true),
                ("restart dropbear.service".to_string(), true),
            ]
        );
    }

    /// dropbear.service restarts on failure under systemd's default start
    /// limit, so a server that cannot bind (an address not yet on any
    /// interface) ends up `failed` with start jobs refused. The operator's
    /// corrected settings must bring it up on this apply: the failure is
    /// cleared first, and only when there is one.
    #[tokio::test]
    async fn a_failed_dropbear_is_reset_before_it_is_started() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "failed", "enabled-runtime");

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert_eq!(
            calls(&reconciler),
            vec![
                "reset-failed dropbear.service".to_string(),
                "start dropbear.service".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_dropbear_that_is_not_failed_is_not_reset() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, _paths) = fixture(dir.path(), "inactive", "enabled-runtime");

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert_eq!(
            calls(&reconciler),
            vec!["start dropbear.service".to_string()]
        );
    }

    /// The whole policy surface through `apply`: root login, the password
    /// setting and whether a transient password is active, with a key list
    /// present. Keys are rendered whatever the password policy; `-s` follows
    /// the EFFECTIVE value; `-w` follows `permitRootLogin` alone.
    #[tokio::test]
    async fn every_root_password_and_transient_combination_renders_its_flags() {
        for permit_root_login in [true, false] {
            for requested in [true, false] {
                for transient in [true, false] {
                    let dir = tempfile::tempdir().unwrap();
                    let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
                    if transient {
                        set_marker(&paths.shadow);
                    }
                    let case = format!(
                        "permitRootLogin={permit_root_login} passwordAuthentication={requested} \
                         transient={transient}"
                    );

                    let state = reconciler
                        .apply(&settings_with(SshSettings {
                            enabled: true,
                            permit_root_login,
                            password_authentication: requested,
                            authorized_keys: vec![raw_key(&canonical(REAL_ED25519_LINE), None)],
                            ..SshSettings::default()
                        }))
                        .await
                        .unwrap();

                    let effective = requested && transient;
                    let mut expected = String::from("DROPBEAR_ARGS=\"-p 22");
                    if !effective {
                        expected.push_str(" -s");
                    }
                    if !permit_root_login {
                        expected.push_str(" -w");
                    }
                    expected.push_str("\"\n");
                    assert_eq!(
                        std::fs::read_to_string(&paths.environment).unwrap(),
                        expected,
                        "{case}"
                    );
                    assert_eq!(state["passwordAuthentication"], json!(effective), "{case}");
                    assert_eq!(
                        state["passwordAuthenticationRequested"],
                        json!(requested),
                        "{case}"
                    );
                    assert_eq!(state["transientPasswordActive"], json!(transient), "{case}");
                    assert_eq!(state["permitRootLogin"], json!(permit_root_login), "{case}");
                    for keys in [&paths.keys, &paths.mos_keys] {
                        assert_eq!(
                            std::fs::read_to_string(keys).unwrap(),
                            format!("{}\n", canonical(REAL_ED25519_LINE)),
                            "{case}: keys are rendered whatever the password policy"
                        );
                    }
                }
            }
        }
    }

    /// A port change reaches every listener, IPv4 and IPv6 alike, except an
    /// entry that names its own port.
    #[tokio::test]
    async fn a_port_change_reaches_every_listener_but_one_with_its_own_port() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let listen = vec![
            "192.0.2.7".to_string(),
            "2001:db8::7".to_string(),
            "[2001:db8::8]:2022".to_string(),
        ];

        for port in [22, 2200] {
            reconciler
                .apply(&settings_with(SshSettings {
                    enabled: true,
                    port,
                    listen_addresses: listen.clone(),
                    ..SshSettings::default()
                }))
                .await
                .unwrap();

            assert_eq!(
                std::fs::read_to_string(&paths.environment).unwrap(),
                format!(
                    "DROPBEAR_ARGS=\"-p 192.0.2.7:{port} -p [2001:db8::7]:{port} \
                     -p [2001:db8::8]:2022 -s\"\n"
                )
            );
        }
    }

    #[tokio::test]
    async fn changed_arguments_while_ssh_is_being_disabled_only_stop_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        preexisting_environment(&paths, GOLDEN_DEFAULTS_GATED);

        reconciler
            .apply(&settings_with(SshSettings {
                enabled: false,
                port: 2222,
                ..SshSettings::default()
            }))
            .await
            .unwrap();

        assert_eq!(
            calls(&reconciler),
            vec![
                "stop dropbear.service".to_string(),
                "disable dropbear.service".to_string()
            ]
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
                "enable dropbear.service".to_string(),
                "start dropbear.service".to_string()
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

        for path in [
            &paths.environment,
            &paths.keys,
            &paths.mos_keys,
            &paths.shadow,
        ] {
            assert!(
                path.starts_with(dir.path()),
                "{} escapes the tempdir",
                path.display()
            );
        }
        assert_eq!(
            state["environmentFile"],
            json!(paths.environment.display().to_string())
        );
        assert_ne!(state["environmentFile"], json!(DEFAULT_ENVIRONMENT_FILE));
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
                .contains("\"/root/"),
            "a test must never render into the real /root"
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
    /// file and an empty file mean the same thing to dropbear, and a key removed
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
    /// The rendered file is read by dropbear, not by a shell, and a guard that
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

    // ---- password authentication gating -----------------------------------

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

        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            GOLDEN_DEFAULTS_GATED,
            "root is locked, so the method cannot succeed and must not be offered"
        );
        assert_eq!(state["passwordAuthentication"], json!(false));
        assert_eq!(state["passwordAuthenticationRequested"], json!(true));
        assert_eq!(state["transientPasswordActive"], json!(false));
    }

    #[tokio::test]
    async fn a_marker_appearing_between_two_applies_turns_passwords_on_and_restarts_dropbear() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "active", "enabled");
        let settings = settings_with(SshSettings {
            enabled: true,
            password_authentication: true,
            ..SshSettings::default()
        });

        let before = reconciler.apply(&settings).await.unwrap();
        assert_eq!(before["passwordAuthentication"], json!(false));
        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            GOLDEN_DEFAULTS_GATED
        );
        let calls_before = calls(&reconciler).len();

        // Nothing in the settings tree changes here — this is exactly what
        // `SetTransientRootPassword` does before it calls `apply_all`.
        set_marker(&paths.shadow);
        let after = reconciler.apply(&settings).await.unwrap();

        assert_eq!(after["passwordAuthentication"], json!(true));
        assert_eq!(after["transientPasswordActive"], json!(true));
        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            GOLDEN_DEFAULTS
        );
        // A restart and nothing else: the server has to re-read -s, and a
        // stop/start pair would leave a window with no listener at all.
        // mica-system's KillMode=process is what keeps the operator's session.
        assert_eq!(
            calls(&reconciler)[calls_before..],
            ["restart dropbear.service".to_string()],
            "dropbear must pick up the flipped arguments: {:?}",
            calls(&reconciler)
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
        assert_eq!(
            std::fs::read_to_string(&paths.environment).unwrap(),
            GOLDEN_DEFAULTS_GATED
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

    // ---- ownership and permissions dropbear checks ------------------------

    #[tokio::test]
    async fn every_key_file_is_the_accounts_0600_file_in_its_0700_ssh_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let (uid, gid) = own_ids();

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        for file in [&paths.keys, &paths.mos_keys] {
            let ssh_dir = file.parent().unwrap();
            assert_eq!(mode_of(file), 0o600, "{}", file.display());
            assert_eq!(mode_of(ssh_dir), 0o700, "{}", ssh_dir.display());
            for path in [file.as_path(), ssh_dir] {
                let meta = std::fs::symlink_metadata(path).unwrap();
                assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{}", path.display());
            }
        }
    }

    #[tokio::test]
    async fn an_existing_ssh_directory_is_brought_to_0700() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let ssh_dir = paths.mos_home.join(".ssh");
        std::fs::create_dir(&ssh_dir).unwrap();
        std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(ssh_dir.join("known_hosts"), "operator data").unwrap();

        reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap();

        assert_eq!(mode_of(&ssh_dir), 0o700);
        assert_eq!(
            std::fs::read_to_string(ssh_dir.join("known_hosts")).unwrap(),
            "operator data",
            "only the key file is this reconciler's"
        );
    }

    /// dropbear refuses every key under a home that group or others can write.
    /// The apply fails naming the home, before ANY account's keys or the
    /// server's arguments change, and the home is not repaired.
    #[tokio::test]
    async fn a_writable_home_fails_the_apply_before_anything_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        std::fs::set_permissions(&paths.mos_home, std::fs::Permissions::from_mode(0o775)).unwrap();

        let err = reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap_err();

        let message = format!("{err:#}");
        assert!(
            message.contains(&paths.mos_home.display().to_string()),
            "{message}"
        );
        assert!(message.contains("group- or world-writable"), "{message}");
        assert!(
            !paths.keys.exists(),
            "root's keys changed on a failed apply"
        );
        assert!(!paths.mos_keys.exists());
        assert!(!paths.environment.exists());
        assert!(calls(&reconciler).is_empty());
        assert_eq!(mode_of(&paths.mos_home), 0o775);
    }

    /// The home is writable by the account and micad is root: a `~/.ssh`
    /// pointing elsewhere must not carry a root-written file there.
    #[tokio::test]
    async fn a_symlinked_ssh_directory_is_refused_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, paths.mos_home.join(".ssh")).unwrap();

        let err = reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .unwrap_err();

        assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
        assert!(!elsewhere.join("authorized_keys").exists());
        assert!(!paths.keys.exists());
    }

    /// Planted names inside `~/.ssh` are replaced, never written through.
    #[tokio::test]
    async fn symlinks_planted_as_the_key_file_or_its_temporary_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        let ssh_dir = paths.mos_home.join(".ssh");
        std::fs::create_dir(&ssh_dir).unwrap();
        std::os::unix::fs::symlink(&victim, ssh_dir.join(".authorized_keys.micad-tmp")).unwrap();
        std::os::unix::fs::symlink(&victim, ssh_dir.join("authorized_keys")).unwrap();

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
        assert!(
            !std::fs::symlink_metadata(&paths.mos_keys)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&paths.mos_keys).unwrap(),
            format!("{}\n", canonical(REAL_ED25519_LINE))
        );
    }

    /// micad reads the existing key file to skip a no-op rewrite. A FIFO
    /// planted under that name would block a plain read-only open until a
    /// writer appeared, holding every reconcile behind the account's whim.
    #[test]
    fn a_fifo_planted_as_the_key_file_neither_hangs_the_apply_nor_survives_it() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        std::fs::create_dir(paths.mos_home.join(".ssh")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &paths.mos_keys,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();

        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            let result = runtime.block_on(reconciler.apply(&settings_with(ssh_settings(true))));
            let _ = done.send(result.map(|_| ()).map_err(|err| format!("{err:#}")));
        });

        let result = finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the apply must not block on a FIFO");
        result.unwrap();
        assert!(
            std::fs::symlink_metadata(&paths.mos_keys)
                .unwrap()
                .file_type()
                .is_file()
        );
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
    // dropbear reads `~/.ssh/authorized_keys` of the user it is
    // authenticating, so a file exists for each account micad manages and all
    // of them carry the same list. These tests hold the plural property; the
    // goldens above still hold the `root` file byte-for-byte.

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
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                expected,
                "{} does not carry the operator's key list",
                path.display()
            );
        }
    }

    /// The file set is exactly the constant list. The fixture's account
    /// database also names `daemon`, whose home gets nothing: a scan of
    /// `/etc/passwd` would render for whatever accounts the host happens to
    /// have, which is the thing the constant exists to prevent.
    #[tokio::test]
    async fn only_the_managed_accounts_get_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let daemon_home = dir.path().join("daemon");
        std::fs::create_dir(&daemon_home).unwrap();
        let (uid, gid) = own_ids();
        std::fs::write(
            &paths.passwd,
            std::fs::read_to_string(&paths.passwd).unwrap().replace(
                "daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin",
                &format!(
                    "daemon:x:{uid}:{gid}:daemon:{}:/bin/bash",
                    daemon_home.display()
                ),
            ),
        )
        .unwrap();

        reconciler
            .apply(&settings_with_keys(vec![raw_key(
                &canonical(REAL_ED25519_LINE),
                None,
            )]))
            .await
            .unwrap();

        assert!(paths.keys.exists());
        assert!(paths.mos_keys.exists());
        assert!(!daemon_home.join(".ssh").exists());
        assert_eq!(MANAGED_LOGIN_ACCOUNTS, ["root", "mos"]);
    }

    /// An account the database does not name, or whose home does not exist,
    /// cannot log in with a key; it is skipped rather than failing the apply,
    /// which would keep SSH closed for root too, and it is absent from the
    /// published paths.
    #[tokio::test]
    async fn an_account_without_an_entry_or_a_home_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (reconciler, paths) = fixture(dir.path(), "inactive", "disabled");
        let passwd = std::fs::read_to_string(&paths.passwd).unwrap();
        let without_mos: String = passwd
            .lines()
            .filter(|line| !line.starts_with("mos:"))
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&paths.passwd, without_mos).unwrap();

        let state = reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .expect("a missing account must not fail the reconcile");

        assert!(paths.keys.exists());
        assert!(!paths.mos_home.join(".ssh").exists());
        assert_eq!(
            state["authorizedKeysPaths"],
            json!([paths.keys.display().to_string()])
        );

        std::fs::write(&paths.passwd, passwd).unwrap();
        std::fs::remove_dir(&paths.mos_home).unwrap();
        let state = reconciler
            .apply(&settings_with(ssh_settings(true)))
            .await
            .expect("a missing home must not fail the reconcile");
        assert!(!paths.mos_home.exists(), "a home is not created");
        assert_eq!(
            state["authorizedKeysPaths"],
            json!([paths.keys.display().to_string()])
        );
    }

    #[test]
    fn the_account_entry_is_found_by_exact_name() {
        let passwd = "mosx:x:5:5::/nowhere:/bin/sh\n\
                      broken:x:notanumber:1::/x:/bin/sh\n\
                      mos:x:1000:1000:mos operator:/home/mos:/bin/bash\n";

        assert_eq!(
            find_account(passwd, "mos"),
            Some(Account {
                name: "mos".to_string(),
                uid: 1000,
                gid: 1000,
                home: PathBuf::from("/home/mos"),
            })
        );
        assert_eq!(find_account(passwd, "broken"), None);
        assert_eq!(find_account(passwd, "root"), None);
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
}
