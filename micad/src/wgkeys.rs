//! WireGuard private keys: lazy on-device generation, and the public half.
//!
//! A WireGuard private key is machine-only. Nothing outside this module ever
//! holds one: it is drawn from the system CSPRNG on the first reconcile of a
//! `kind = "wireguard"` entry that has no key yet, written to a file only
//! systemd-networkd can read, and read back only here, only to derive the
//! public half. There is no accessor for it, so no bus method, no route and no
//! live-state field can grow one by accident, and the settings tree carries no
//! field it could be written into either.
//!
//! Two deviations from the device password and the AP PSK
//! ([`crate::identity`]), both forced by who reads the file. Those secrets are
//! read by root, so they live at 0600 under a 0700 directory; a WireGuard key
//! is read by systemd-networkd, which drops to its own user, so the key file is
//! `root:systemd-network` 0640 under a sibling directory of the same ownership
//! at 0750 — group-readable, never world-readable. And where the AP PSK refuses
//! to invent a value it does not have, this generates one: an AP PSK must match
//! what an operator was told out of band, so inventing it would strand them,
//! while a WireGuard private key must never be told to anyone, so inventing it
//! is the only correct behaviour.
//!
//! Like the AP reconciler, this module contains no logging statement at all,
//! and no error it returns names anything but a path.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, fchown};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::rand::{SecureRandom, SystemRandom};
use x25519_dalek::{PublicKey, StaticSecret};

/// Sub-directory of the state directory holding the key files networkd reads.
///
/// A sibling of `secrets/` rather than anything under it, and the name says so
/// because the path is load-bearing. Traversal needs execute on every
/// component, `identity::ensure_secrets_dir` pins `secrets/` to 0700 on every
/// pass, and it pins it unconditionally — so a key anywhere below that
/// directory is a key the `systemd-network` user cannot reach whatever this
/// module sets on its own directory and file. One component under the state
/// directory, created and moded here, is the whole path.
const NETWORKD_SECRETS_DIR: &str = "networkd-secrets";

/// Mode of the key directory: owner plus group, no world.
const KEY_DIR_MODE: u32 = 0o750;

/// Mode of every key file: owner read/write, group read, no world.
const KEY_FILE_MODE: u32 = 0o640;

/// Group that must be able to read a key file: the user systemd-networkd runs
/// as.
const KEY_GROUP: &str = "systemd-network";

/// The group database, read to turn [`KEY_GROUP`] into a numeric gid.
const GROUP_FILE: &str = "/etc/group";

/// Length of an X25519 key in bytes.
const KEY_LEN: usize = 32;

/// The key store for one device: where the key files live, and the group that
/// may read them.
///
/// Cheap to construct and inert until used — nothing is created, and no key is
/// drawn, until [`Keystore::ensure`] runs for an interface that has none. That
/// is what lets the network reconciler be built anywhere, including in a
/// process that will never reconcile a tunnel.
#[derive(Debug, Clone)]
pub struct Keystore {
    dir: PathBuf,
    group: Option<u32>,
}

impl Keystore {
    /// A key store under `state_dir`, chowning what it writes to `group`.
    ///
    /// The directory is always `state_dir` joined with
    /// [`NETWORKD_SECRETS_DIR`] and never a path a caller composed, so there is
    /// no way to build a store whose keys sit somewhere untraversable.
    ///
    /// `group` is `None` when there is no group to give the files to, which
    /// leaves them `root:root` at 0640 — tighter than intended, never laxer.
    #[must_use]
    pub fn under(state_dir: &Path, group: Option<u32>) -> Self {
        Self {
            dir: state_dir.join(NETWORKD_SECRETS_DIR),
            group,
        }
    }

    /// The production store: `<state_dir>/networkd-secrets`, group
    /// [`KEY_GROUP`] as the host's group database spells it.
    ///
    /// A missing `systemd-network` group leaves the files root-owned rather
    /// than failing: the device that lacks the group also lacks the
    /// systemd-networkd that reads the key, and refusing here would abort the
    /// whole network reconcile — every physical interface included — over one
    /// tunnel that could not have come up either way. Proving the group can
    /// read the file on a real image is the on-image check
    /// assigns to the kernel-and-image milestone.
    #[must_use]
    pub fn production(state_dir: &Path) -> Self {
        Self::under(state_dir, lookup_group(Path::new(GROUP_FILE), KEY_GROUP))
    }

    /// Path of `iface`'s private-key file, which is what the rendered
    /// `.netdev` names in `PrivateKeyFile=`.
    ///
    /// Total, and callable for an interface that has no key yet: the renderer
    /// needs the path before the key exists.
    #[must_use]
    pub fn key_path(&self, iface: &str) -> PathBuf {
        self.dir.join(format!("wg-{iface}.key"))
    }

    /// The public half of `iface`'s private key, drawing and storing a private
    /// key first if it has none.
    ///
    /// Idempotent: an interface that already has a key keeps it, so a
    /// reconcile pass never rotates by accident.
    ///
    /// # Errors
    ///
    /// Returns an error when the key directory or file cannot be written, or
    /// when an existing key file does not hold a 32-byte base64 key. The
    /// message names the path and never the contents.
    pub fn ensure(&self, iface: &str) -> Result<String> {
        match self.read_key(iface)? {
            Some(secret) => Ok(public_key(&secret)),
            None => self.write_new_key(iface),
        }
    }

    /// Draw a new private key for `iface`, replacing any existing one, and
    /// return its public half.
    ///
    /// The old key is gone from disk when this returns: the write is a rename
    /// over the same path, so there is no key history and no instant in which
    /// the file is missing.
    ///
    /// # Errors
    ///
    /// Returns an error when the key directory or file cannot be written.
    pub fn rotate(&self, iface: &str) -> Result<String> {
        self.write_new_key(iface)
    }

    /// Draw, store and return the public half of a fresh private key.
    fn write_new_key(&self, iface: &str) -> Result<String> {
        let secret = generate_key()?;
        self.write_key(iface, &BASE64.encode(secret.to_bytes()))?;
        Ok(public_key(&secret))
    }

    /// Write `key` to [`Self::key_path`] atomically, at [`KEY_FILE_MODE`] and
    /// owned by [`Self::group`].
    ///
    /// The shape `identity::write_secret` uses — temp file beside the target,
    /// fsync, rename, fsync the directory — with the mode AND the group set on
    /// the temp file before the rename, so the key is never reachable under its
    /// final name at a laxer mode or owned by a wider group.
    fn write_key(&self, iface: &str, key: &str) -> Result<()> {
        self.ensure_dir()?;
        let target = self.key_path(iface);
        let temp = self.dir.join(format!(".wg-{iface}.key.tmp"));

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(KEY_FILE_MODE)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        // `OpenOptions::mode` applies only when the file is created, and is
        // masked by the umask; this is what actually guarantees the mode,
        // including when a temp file survived an interrupted earlier run.
        file.set_permissions(Permissions::from_mode(KEY_FILE_MODE))
            .with_context(|| format!("chmod {}", temp.display()))?;
        if let Some(group) = self.group {
            // On the open handle rather than the path: the file whose mode was
            // just pinned is the file whose group is being set, with no window
            // in which the name could have been replaced.
            fchown(&file, None, Some(group))
                .with_context(|| format!("chgrp {}", temp.display()))?;
        }
        file.write_all(key.as_bytes())
            .with_context(|| format!("write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", temp.display()))?;
        drop(file);

        fs::rename(&temp, &target)
            .with_context(|| format!("rename {} to {}", temp.display(), target.display()))?;
        File::open(&self.dir)
            .and_then(|handle| handle.sync_all())
            .with_context(|| format!("fsync {}", self.dir.display()))?;
        Ok(())
    }

    /// `iface`'s stored private key, or `None` when it has none.
    ///
    /// Private to this module, and it stays that way: a key that cannot be read
    /// out is a key that cannot be published by mistake.
    fn read_key(&self, iface: &str) -> Result<Option<StaticSecret>> {
        let path = self.key_path(iface);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        // The message names the file and not what was in it: an unreadable key
        // is still a key.
        let bytes = decode_key(text.trim_end())
            .ok_or_else(|| anyhow!("{} does not hold a WireGuard key", path.display()))?;
        Ok(Some(StaticSecret::from(bytes)))
    }

    /// Create the key directory if needed and pin it to [`KEY_DIR_MODE`] and
    /// [`Self::group`].
    fn ensure_dir(&self) -> Result<()> {
        if !self.dir.is_dir() {
            if let Some(parent) = self.dir.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            // mkdir(2) carries the mode, so the directory is never momentarily
            // world-readable between creation and chmod.
            match DirBuilder::new().mode(KEY_DIR_MODE).create(&self.dir) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => {
                    return Err(err).with_context(|| format!("create {}", self.dir.display()));
                }
            }
        }
        // Unconditional, and not masked by the umask the way mkdir's mode is: a
        // directory left behind by an older or interrupted run gets tightened.
        fs::set_permissions(&self.dir, Permissions::from_mode(KEY_DIR_MODE))
            .with_context(|| format!("chmod {}", self.dir.display()))?;
        if let Some(group) = self.group {
            std::os::unix::fs::chown(&self.dir, None, Some(group))
                .with_context(|| format!("chgrp {}", self.dir.display()))?;
        }
        Ok(())
    }
}

/// A fresh X25519 private key from the system CSPRNG, clamped.
///
/// [`SystemRandom`] is the source [`crate::identity`] already draws the device
/// password and the AP PSK from, so there is one CSPRNG in this daemon and not
/// two. The fleet-wide-constant hazard the AP key guards against cannot arise
/// here: nothing is baked into the image, every key is drawn on the device.
fn generate_key() -> Result<StaticSecret> {
    let mut bytes = [0u8; KEY_LEN];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("system CSPRNG unavailable"))?;
    // The X25519 clamping RFC 7748 specifies, and what `wg genkey` writes: the
    // stored key is the clamped one, so the file, the kernel and the derived
    // public half all agree on the same scalar.
    bytes[0] &= 248;
    bytes[31] &= 127;
    bytes[31] |= 64;
    Ok(StaticSecret::from(bytes))
}

/// The base64 public half of `secret`.
fn public_key(secret: &StaticSecret) -> String {
    BASE64.encode(PublicKey::from(secret).to_bytes())
}

/// The 32 bytes `value` spells in base64, or `None` when it spells something
/// else.
fn decode_key(value: &str) -> Option<[u8; KEY_LEN]> {
    BASE64.decode(value).ok()?.try_into().ok()
}

/// Whether `value` is a base64 X25519 key, which is what a peer's `publicKey`
/// has to be.
///
/// The renderer interpolates the value onto a `PublicKey=` line, where a
/// newline is a new directive; requiring it to decode to exactly 32 bytes makes
/// injection structurally impossible rather than filtering for it — the
/// discipline the network reconciler's address validation already applies.
#[must_use]
pub fn is_key(value: &str) -> bool {
    decode_key(value).is_some()
}

/// The gid `name` has in a group file, or `None` when the file has no such
/// group.
///
/// Parsed rather than looked up through `getgrnam`: this workspace forbids
/// `unsafe`, so there is no FFI call to make, and the group file is the same
/// four-colon-separated format on every image this daemon ships in.
fn lookup_group(group_file: &Path, name: &str) -> Option<u32> {
    let text = fs::read_to_string(group_file).ok()?;
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        // The password placeholder, between the name and the gid.
        fields.next()?;
        fields.next()?.parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    /// A store in a throwaway directory, with no group to give files to.
    ///
    /// Every test in this module works in a temporary directory: a key drawn
    /// anywhere else would be a real key on the machine running the tests.
    fn keystore_in(dir: &Path) -> Keystore {
        Keystore::under(dir, None)
    }

    /// The stored key file's contents. Test-only, and deliberately not a method
    /// on [`Keystore`]: the private key has no accessor in production code.
    fn stored_key(store: &Keystore, iface: &str) -> String {
        fs::read_to_string(store.key_path(iface)).expect("key file")
    }

    #[test]
    fn generates_a_key_lazily_and_then_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());
        assert!(!store.key_path("wg0").exists());

        let first = store.ensure("wg0").unwrap();
        let second = store.ensure("wg0").unwrap();

        // The second pass must not rotate: a reconcile runs on every settings
        // write anywhere in the tree, and a key that changed each time would
        // break the tunnel on every one of them.
        assert_eq!(first, second);
        assert_eq!(stored_key(&store, "wg0"), stored_key(&store, "wg0"));
        assert!(store.key_path("wg0").exists());
    }

    #[test]
    fn the_key_directory_is_0750_and_the_key_file_0640() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());

        store.ensure("wg0").unwrap();

        let dir_mode = fs::metadata(dir.path().join("networkd-secrets"))
            .unwrap()
            .permissions()
            .mode();
        let file_mode = fs::metadata(store.key_path("wg0"))
            .unwrap()
            .permissions()
            .mode();
        // Group-readable so systemd-networkd can read it, never world-readable.
        assert_eq!(dir_mode & 0o777, KEY_DIR_MODE);
        assert_eq!(file_mode & 0o777, KEY_FILE_MODE);
    }

    #[test]
    fn a_pre_existing_directory_is_tightened_before_a_key_lands_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());
        let key_dir = dir.path().join("networkd-secrets");
        fs::create_dir_all(&key_dir).unwrap();
        fs::set_permissions(&key_dir, Permissions::from_mode(0o777)).unwrap();

        store.ensure("wg0").unwrap();

        assert_eq!(
            fs::metadata(&key_dir).unwrap().permissions().mode() & 0o777,
            KEY_DIR_MODE
        );
    }

    #[test]
    fn the_key_file_and_its_directory_take_the_group_they_are_given() {
        let dir = tempfile::tempdir().unwrap();
        // A real gid other than the one a root-created file already has, so the
        // assertion cannot pass by accident. `daemon` is in every Debian group
        // file, and these tests run as root in the build container.
        let Some(group) = lookup_group(Path::new(GROUP_FILE), "daemon").filter(|gid| *gid != 0)
        else {
            return;
        };
        let store = Keystore::under(dir.path(), Some(group));

        store.ensure("wg0").unwrap();

        assert_eq!(fs::metadata(store.key_path("wg0")).unwrap().gid(), group);
        assert_eq!(
            fs::metadata(dir.path().join("networkd-secrets"))
                .unwrap()
                .gid(),
            group
        );
    }

    #[test]
    fn rotation_replaces_the_key_and_its_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());
        let first_public = store.ensure("wg0").unwrap();
        let first_stored = stored_key(&store, "wg0");

        let second_public = store.rotate("wg0").unwrap();

        assert_ne!(first_public, second_public);
        // The old key is gone from disk: one path, written by rename, no
        // history kept beside it.
        assert_ne!(stored_key(&store, "wg0"), first_stored);
        assert_eq!(
            fs::read_dir(dir.path().join("networkd-secrets"))
                .unwrap()
                .count(),
            1
        );
        // And the rotated file is still only as readable as the first one.
        assert_eq!(
            fs::metadata(store.key_path("wg0"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            KEY_FILE_MODE
        );
        // The public half a later reconcile derives is the rotated one.
        assert_eq!(store.ensure("wg0").unwrap(), second_public);
    }

    #[test]
    fn a_stored_key_is_32_clamped_bytes_and_is_not_its_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());

        let public = store.ensure("wg0").unwrap();

        let stored = decode_key(&stored_key(&store, "wg0")).expect("base64 key");
        assert_eq!(stored.len(), KEY_LEN);
        // RFC 7748 clamping, the same bits `wg genkey` sets.
        assert_eq!(stored[0] & 7, 0);
        assert_eq!(stored[31] & 128, 0);
        assert_eq!(stored[31] & 64, 64);
        // The file holds the private key, so it cannot equal the value that is
        // published.
        assert_ne!(stored_key(&store, "wg0"), public);
    }

    #[test]
    fn two_interfaces_get_two_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());

        let one = store.ensure("wg0").unwrap();
        let two = store.ensure("wg1").unwrap();

        assert_ne!(one, two);
        assert_ne!(stored_key(&store, "wg0"), stored_key(&store, "wg1"));
    }

    #[test]
    fn a_key_file_that_is_not_a_key_is_an_error_that_does_not_echo_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = keystore_in(dir.path());
        store.ensure("wg0").unwrap();
        // Not a key, and distinctive enough that an error carrying the file's
        // contents could not hide it.
        fs::write(store.key_path("wg0"), "CANARY-NOT-A-KEY").unwrap();

        let err = store.ensure("wg0").unwrap_err();

        assert!(err.to_string().contains("does not hold a WireGuard key"));
        assert!(!format!("{err:#}").contains("CANARY"), "{err:#}");
    }

    #[test]
    fn a_key_of_the_wrong_length_is_not_a_key() {
        assert!(is_key(&BASE64.encode([7u8; KEY_LEN])));
        assert!(!is_key(&BASE64.encode([7u8; KEY_LEN - 1])));
        assert!(!is_key(&BASE64.encode([7u8; KEY_LEN + 1])));
        assert!(!is_key("not base64 at all"));
        assert!(!is_key(""));
    }

    #[test]
    fn no_component_of_a_key_path_is_traversable_only_by_root() {
        let dir = tempfile::tempdir().unwrap();
        // The parent this store has to live beside on a real device, at the
        // mode `identity::ensure_secrets_dir` pins it to on every pass.
        let secrets = dir.path().join("secrets");
        fs::create_dir_all(&secrets).unwrap();
        fs::set_permissions(&secrets, Permissions::from_mode(0o700)).unwrap();
        let store = Keystore::under(dir.path(), None);

        store.ensure("wg0").unwrap();

        let key_path = store.key_path("wg0");
        // Reaching a file needs execute on every directory above it, so a key
        // under the 0700 secrets directory would be unreachable to
        // systemd-network whatever mode this module put on its own directory
        // and file. The mode assertions cannot see that; this can.
        assert!(!key_path.starts_with(&secrets), "{}", key_path.display());
        assert!(
            fs::read_dir(&secrets).unwrap().next().is_none(),
            "the key store must create nothing under the 0700 secrets directory"
        );
        let created: Vec<_> = key_path
            .ancestors()
            .skip(1)
            .take_while(|path| *path != dir.path())
            .collect();
        // One directory, created and moded here: an intermediate component
        // would be one nothing in this module sets a mode on.
        assert_eq!(created.len(), 1, "{created:?}");
        for path in created {
            let mode = fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o050,
                0o050,
                "{} is not readable and traversable by its group",
                path.display()
            );
        }
    }

    #[test]
    fn a_group_file_gives_up_its_gid_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let group_file = dir.path().join("group");
        fs::write(
            &group_file,
            "root:x:0:\nsystemd-network:x:998:\nsystemd-journal:x:999:alice\n",
        )
        .unwrap();

        assert_eq!(lookup_group(&group_file, KEY_GROUP), Some(998));
        assert_eq!(lookup_group(&group_file, "systemd-journal"), Some(999));
        assert_eq!(lookup_group(&group_file, "nobody"), None);
        // A file that is not there is a group that is not there, not a failure:
        // the caller's answer to both is the same.
        assert_eq!(lookup_group(&dir.path().join("absent"), KEY_GROUP), None);
    }
}
