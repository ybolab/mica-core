//! Transient root password: set once by the operator, gone on the next boot.
//!
//! The appliance ships with root carrying no password at all and SSH off,
//! persistent access being by SSH public key. A transient password exists for
//! the one case a key cannot cover — an operator standing in front of a device
//! with no key installed yet — and must not outlive that session, so it is
//! deliberately not a setting: nothing about it is persisted in the settings
//! tree and nothing re-applies it on the next boot.
//!
//! Two files on STATE carry it: the shadow file itself, whose `root:` hash
//! field is rewritten with a bcrypt hash of the password, and a marker beside
//! it, [`transient_marker_path`], holding exactly that hash.
//! `mos-shadow-reconcile` runs on every boot before sshd and mosd; when the
//! marker's hash still equals the shadow file's root hash it rewrites the field
//! to `!` and deletes the marker, so the password vanishes. When the two
//! disagree the shadow file is left alone: something other than this module
//! owns the current hash — a dev image's build-time `ROOT_PASSWORD`, for
//! instance — and clearing it would overwrite a credential this code did not
//! set. That distinction is the whole reason a marker exists instead of "lock
//! root on every boot". Nothing here logs the password or the hash.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Default shadow file holding the root account's password hash.
///
/// Restated rather than shared with the sshd reconciler: that module is private
/// to `reconciler`, and this constant is read from `main` to configure the bus
/// service. The two must name the same file, which they do by construction —
/// `/etc/shadow` is a symlink onto STATE on the mos image.
pub const DEFAULT_SHADOW: &str = "/etc/shadow";
/// Environment variable overriding [`DEFAULT_SHADOW`]. The same variable the
/// sshd reconciler honours, so one override redirects both and a test can point
/// the whole daemon at a temporary file.
pub const SHADOW_ENV: &str = "MOSD_SHADOW_PATH";

/// Name of the marker file, resolved beside the shadow file rather than at a
/// fixed path: the shadow file is `/var/lib/mos/shadow` once `var-lib-mos.mount`
/// is up and `/mnt/state/mos/shadow` before it, and both must name the same
/// STATE-backed marker.
const MARKER_NAME: &str = "transient-root-password";
/// Mode of the marker: owner read/write only. Its content is a password hash,
/// so it gets the shadow file's discretion rather than a config file's.
const MARKER_MODE: u32 = 0o600;
/// Account whose password hash mosd owns.
const ROOT_ACCOUNT: &str = "root";
/// Prefix identifying that account's shadow entry.
const ROOT_PREFIX: &str = "root:";
/// Field index of the password hash in a shadow entry.
const SHADOW_HASH_FIELD: usize = 1;
/// bcrypt cost for the shadow hash. 12 is the current defensible default: a
/// few hundred milliseconds per verification on the target class of hardware,
/// which is tolerable for an interactive login and expensive for an attacker
/// working through a stolen shadow file.
const BCRYPT_COST: u32 = 12;
/// Shortest password accepted.
///
/// Not security theatre: the moment sshd comes up this password is reachable
/// over the network, so it is guessed at network speed rather than at
/// keyboard speed. Eight bytes is the floor, not a recommendation.
const MIN_PASSWORD_BYTES: usize = 8;
/// Longest password accepted. bcrypt itself only reads the first 72 bytes, so
/// this is a bound on the input rather than on the strength of the result: it
/// keeps an unbounded string off the D-Bus path and out of the hasher.
const MAX_PASSWORD_BYTES: usize = 256;

/// Shadow path mosd operates on: [`SHADOW_ENV`] if set, else
/// [`DEFAULT_SHADOW`].
pub fn production_shadow_path() -> PathBuf {
    std::env::var(SHADOW_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SHADOW))
}

/// Marker recording the transient hash for the shadow file at `shadow_path`.
///
/// Derived from the shadow path, never hardcoded, so a caller working on an
/// alternate path — `mos-seed-state`'s `/mnt/state/mos/shadow`, or a test's
/// temporary file — gets the marker that belongs to it.
///
/// The symlink is resolved first, and that is why this is not one line.
/// `Path::with_file_name` is lexical: it rewrites the last component of the
/// string and resolves nothing. On the mos image `/etc/shadow` is a symlink onto
/// STATE, so writing the shadow file follows the link and succeeds while a
/// marker placed "beside" it lexically lands in the literal `/etc/`, a
/// dm-verity squashfs. The write then fails with `Read-only file system (os
/// error 30)`, mosd returns an error over the bus, and apid answers `502 Bad
/// Gateway`: the transient SSH root password, the documented way back into a
/// locked-out device, does not work at all. A test over already-resolved paths
/// cannot see that, so a test has to pass the `/etc/shadow` symlink production
/// passes. `canonicalize` needs the path to exist; when it does not, the
/// lexical answer is correct and is what is returned, an unresolvable path
/// having no symlink to follow.
pub fn transient_marker_path(shadow_path: &Path) -> PathBuf {
    std::fs::canonicalize(shadow_path)
        .unwrap_or_else(|_| shadow_path.to_path_buf())
        .with_file_name(MARKER_NAME)
}

/// Whether a transient root password is currently set.
///
/// "The marker exists and is not empty" — the same condition
/// `mos-shadow-reconcile` tests on the next boot, so the two agree about what
/// counts as set.
pub fn transient_password_active(shadow_path: &Path) -> bool {
    std::fs::metadata(transient_marker_path(shadow_path)).is_ok_and(|meta| meta.len() > 0)
}

/// Set a transient root password on the shadow file at `shadow_path`.
///
/// The password is hashed with bcrypt, written into the root account's hash
/// field, and the same hash is recorded in the marker beside it. It survives
/// until the next boot, when `mos-shadow-reconcile` clears both.
///
/// # Errors
///
/// Returns an error when `password` is rejected (see [`validate`]), when the
/// shadow file cannot be read or has no `root:` entry, or when either write
/// fails. No error message carries the password or the hash.
pub fn set_transient_root_password(shadow_path: &Path, password: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    validate(password)?;

    let current = std::fs::read_to_string(shadow_path)
        .with_context(|| format!("read {}", shadow_path.display()))?;
    let hash = bcrypt::hash(password, BCRYPT_COST)
        .map_err(|err| anyhow!("hash the transient password for the shadow file: {err}"))?;
    let updated = rewrite_root_hash(&current, &hash)
        .with_context(|| format!("update {}", shadow_path.display()))?;
    // Read the hash back out of the rewritten text rather than trusting that
    // the field went where it was meant to: the marker only works if it holds
    // byte-for-byte what the shadow file ends up carrying.
    let stored = root_hash(&updated)
        .with_context(|| format!("update {}", shadow_path.display()))?
        .to_string();

    let metadata = std::fs::metadata(shadow_path)
        .with_context(|| format!("stat {}", shadow_path.display()))?;
    // Marker FIRST, shadow file second, and the order is load-bearing. A crash
    // between the two writes leaves a marker naming a hash the shadow file
    // does not carry; the next boot's reconciler sees the mismatch, leaves the
    // file alone and removes the marker — harmless. The other order leaves a
    // changed shadow entry with no marker, which the reconciler must preserve
    // (that branch is what lets a hash mosd did not write survive): a password
    // that never expires, the exact failure this whole design exists to
    // prevent.
    write_atomically(
        &transient_marker_path(shadow_path),
        &format!("{stored}\n"),
        MARKER_MODE,
        None,
    )
    .with_context(|| "record the transient marker".to_string())?;
    write_atomically(
        shadow_path,
        &updated,
        metadata.permissions().mode() & 0o7777,
        Some((metadata.uid(), metadata.gid())),
    )?;

    tracing::warn!(
        shadow = %shadow_path.display(),
        "transient root password set; it is cleared on the next boot"
    );
    Ok(())
}

/// Reject a password this module will not hash.
///
/// # Errors
///
/// Returns an error naming the rule that was broken, never the password:
/// shorter than [`MIN_PASSWORD_BYTES`], longer than [`MAX_PASSWORD_BYTES`], or
/// carrying `\0`, `\n` or `\r`. Those three would either terminate the hash
/// early or split the shadow entry across lines.
fn validate(password: &str) -> Result<()> {
    if password.len() < MIN_PASSWORD_BYTES {
        return Err(anyhow!(
            "password must be at least {MIN_PASSWORD_BYTES} bytes"
        ));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(anyhow!(
            "password must be at most {MAX_PASSWORD_BYTES} bytes"
        ));
    }
    if password.contains(['\0', '\n', '\r']) {
        return Err(anyhow!(
            "password must not contain a NUL, newline or carriage return"
        ));
    }
    Ok(())
}

/// The hash field of the `root:` line in `shadow`.
///
/// # Errors
///
/// Returns an error when there is no `root:` line — mosd owns that account's
/// credential, so a shadow file without it is a broken image, not an empty
/// job.
pub(crate) fn root_hash(shadow: &str) -> Result<&str> {
    shadow
        .lines()
        .find(|line| line.starts_with(ROOT_PREFIX))
        .and_then(|line| line.split(':').nth(SHADOW_HASH_FIELD))
        .ok_or_else(|| anyhow!("no `{ROOT_ACCOUNT}:` entry in shadow file"))
}

/// Replace the hash field of the `root:` line in `shadow` with `hash`.
///
/// Read-modify-write on the exact bytes: every other account's line, the field
/// count and ordering of the root line, and the presence or absence of a
/// trailing newline all survive untouched.
///
/// # Errors
///
/// Returns an error when there is no `root:` line.
pub(crate) fn rewrite_root_hash(shadow: &str, hash: &str) -> Result<String> {
    let mut lines: Vec<String> = shadow.split('\n').map(str::to_string).collect();
    let root = lines
        .iter_mut()
        .find(|line| line.starts_with(ROOT_PREFIX))
        .ok_or_else(|| anyhow!("no `{ROOT_ACCOUNT}:` entry in shadow file"))?;

    // The line matched `root:`, so splitting on `:` yields at least the name
    // and the hash field; every further field is carried over untouched.
    let mut fields: Vec<&str> = root.split(':').collect();
    fields[SHADOW_HASH_FIELD] = hash;
    *root = fields.join(":");

    Ok(lines.join("\n"))
}

/// Write `contents` to `path` atomically: a temporary file in the same
/// directory, flushed, then renamed over the target.
///
/// Same directory because `rename` is only atomic within one filesystem, and
/// `/etc/ssh/sshd_config.d` is a separate mount from `/etc`.
///
/// `mode` is applied explicitly rather than left to the umask so the result is
/// deterministic, and `owner` (uid, gid) is restored when given — a shadow
/// file that comes back owned by `root:root` instead of `root:shadow` locks
/// out every setgid tool that reads it.
pub(crate) fn write_atomically(
    path: &Path,
    contents: &str,
    mode: u32,
    owner: Option<(u32, u32)>,
) -> Result<()> {
    // One implementation of the temp-and-rename dance, in crate::fswrite. This
    // is the variant that also restores an owner.
    crate::fswrite::write_config_owned(path, contents, mode, owner)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    /// Three accounts, nine fields each, trailing newline — the shape of a
    /// Debian `/etc/shadow`. `root` starts out locked (`!`).
    const SHADOW: &str = "root:!:19000:0:99999:7:::\n\
        daemon:*:19000:0:99999:7:::\n\
        operator:$6$rounds=5000$abcd$efgh:19100:0:99999:7:::\n";
    /// The two accounts a transient password must never touch.
    const OTHER_ACCOUNTS: &str = "daemon:*:19000:0:99999:7:::\n\
        operator:$6$rounds=5000$abcd$efgh:19100:0:99999:7:::\n";
    /// Opaque replacement hash for the pure-function rewrite tests.
    const CRYPT_HASH: &str = "$2b$12$abcdefghijklmnopqrstuvOJqM0iZ5wKzXwZ2G8bqZ0aVjPQnDGa";
    /// A password that satisfies every rule.
    const GOOD: &str = "correct horse battery";
    const SHADOW_MODE: u32 = 0o640;

    /// A shadow file at [`SHADOW_MODE`] inside `dir`, and its path.
    fn fixture(dir: &Path) -> PathBuf {
        let shadow = dir.join("shadow");
        std::fs::write(&shadow, SHADOW).unwrap();
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(SHADOW_MODE)).unwrap();
        shadow
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn stored_root_hash(path: &Path) -> String {
        root_hash(&std::fs::read_to_string(path).unwrap())
            .unwrap()
            .to_string()
    }

    // ---- the marker path --------------------------------------------------

    #[test]
    fn the_marker_sits_beside_the_shadow_file_whatever_that_is() {
        assert_eq!(
            transient_marker_path(Path::new("/var/lib/mos/shadow")),
            Path::new("/var/lib/mos/transient-root-password")
        );
        // The early path mos-seed-state uses, before var-lib-mos.mount is up.
        assert_eq!(
            transient_marker_path(Path::new("/mnt/state/mos/shadow")),
            Path::new("/mnt/state/mos/transient-root-password")
        );
        // Derived, not hardcoded: an arbitrary directory follows the file.
        assert_eq!(
            transient_marker_path(Path::new("/tmp/case-3/shadow")),
            Path::new("/tmp/case-3/transient-root-password")
        );
    }

    /// The case production passes, and the one the three above miss.
    ///
    /// Every path in the test above is already resolved. The only path mosd
    /// ever hands this function on a device is `/etc/shadow`, which on the mos
    /// image is a SYMLINK onto STATE -- and `Path::with_file_name` is lexical,
    /// so a marker resolved lexically lands in the literal `/etc/`, a
    /// read-only dm-verity squashfs: mosd fails with `os error 30`, apid
    /// answers 502, and the transient SSH root password -- the documented way
    /// back into a locked-out device -- does not work at all.
    ///
    /// The assertion is that the marker follows the LINK TARGET's directory,
    /// not the link's own.
    #[test]
    fn the_marker_follows_a_symlinked_shadow_to_where_it_really_lives() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let etc = dir.path().join("etc");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir_all(&etc).unwrap();

        let real_shadow = state.join("shadow");
        std::fs::write(&real_shadow, "root:!:20000:::::\n").unwrap();
        let link = etc.join("shadow");
        std::os::unix::fs::symlink(&real_shadow, &link).unwrap();

        assert_eq!(
            transient_marker_path(&link),
            std::fs::canonicalize(&state).unwrap().join(MARKER_NAME),
            "the marker must sit beside the shadow file the link POINTS AT; \
             beside the link itself is a read-only squashfs on a real device"
        );
    }

    /// A path that does not exist yet keeps the lexical answer, because an
    /// unresolvable path has no symlink to follow. A caller probing before the
    /// file is created must not be handed an error.
    #[test]
    fn a_path_that_does_not_exist_still_gets_a_marker_beside_it() {
        assert_eq!(
            transient_marker_path(Path::new("/tmp/definitely-not-created/shadow")),
            Path::new("/tmp/definitely-not-created/transient-root-password")
        );
    }

    // ---- what is rejected, and what is not --------------------------------

    #[test]
    fn a_password_shorter_than_eight_bytes_is_rejected_and_eight_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());

        for short in ["", "a", "1234567"] {
            let err = set_transient_root_password(&shadow, short).unwrap_err();
            assert!(
                format!("{err:#}").contains("at least 8 bytes"),
                "{short:?} should be rejected for length, got {err:#}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&shadow).unwrap(),
            SHADOW,
            "a rejected password must not reach the shadow file"
        );
        assert!(!transient_marker_path(&shadow).exists());

        set_transient_root_password(&shadow, "12345678").expect("8 bytes is the floor, not a wall");
        assert!(bcrypt::verify("12345678", &stored_root_hash(&shadow)).unwrap());
    }

    #[test]
    fn a_password_longer_than_the_ceiling_is_rejected_and_the_ceiling_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());

        let err = set_transient_root_password(&shadow, &"x".repeat(257)).unwrap_err();
        assert!(format!("{err:#}").contains("at most 256 bytes"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&shadow).unwrap(), SHADOW);
        assert!(!transient_marker_path(&shadow).exists());

        set_transient_root_password(&shadow, &"x".repeat(256)).expect("256 bytes is accepted");
        assert!(transient_password_active(&shadow));
    }

    #[test]
    fn control_characters_that_would_split_a_shadow_entry_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());

        for hostile in [
            "abcdefg\0h",
            "abcdefg\nh",
            "abcdefg\rh",
            "\ncorrect horse",
            "correct horse\r\n",
        ] {
            let err = set_transient_root_password(&shadow, hostile).unwrap_err();
            assert!(
                format!("{err:#}").contains("NUL, newline or carriage return"),
                "{hostile:?} should be rejected for content, got {err:#}"
            );
        }
        assert_eq!(std::fs::read_to_string(&shadow).unwrap(), SHADOW);
        assert!(!transient_marker_path(&shadow).exists());
    }

    #[test]
    fn a_rejection_message_never_carries_the_password() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());
        let secret = "hunter2!";

        for bad in [format!("{secret}\n"), secret[..7].to_string()] {
            let err = set_transient_root_password(&shadow, &bad).unwrap_err();
            let chain = format!("{err:#}");
            assert!(!chain.contains("hunter"), "password leaked into: {chain}");
        }
    }

    // ---- the successful path ----------------------------------------------

    #[test]
    fn the_shadow_hash_verifies_and_the_marker_repeats_it_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());
        let marker = transient_marker_path(&shadow);

        set_transient_root_password(&shadow, GOOD).unwrap();

        let written = stored_root_hash(&shadow);
        assert!(
            written.starts_with("$2b$12$"),
            "shadow must carry a crypt(3) format the image's libcrypt implements, got {written}"
        );
        assert!(bcrypt::verify(GOOD, &written).unwrap());
        assert!(
            !bcrypt::verify("correct horse battery ", &written).unwrap(),
            "a different password must not verify"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            format!("{written}\n"),
            "the marker is the hash and a newline, nothing else"
        );
        assert_eq!(mode_of(&marker), 0o600);
    }

    #[test]
    fn nothing_but_the_root_hash_field_moves() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());

        set_transient_root_password(&shadow, GOOD).unwrap();

        let written = stored_root_hash(&shadow);
        assert_eq!(
            std::fs::read_to_string(&shadow).unwrap(),
            format!("root:{written}:19000:0:99999:7:::\n{OTHER_ACCOUNTS}"),
            "every other account and every other field must survive byte-for-byte"
        );
        assert_eq!(
            mode_of(&shadow),
            SHADOW_MODE,
            "the shadow mode is preserved"
        );
    }

    #[test]
    fn no_temporary_file_survives_the_write() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());

        set_transient_root_password(&shadow, GOOD).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains("mosd-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn active_is_false_before_true_after_and_false_once_the_marker_goes() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());

        assert!(!transient_password_active(&shadow));

        set_transient_root_password(&shadow, GOOD).unwrap();
        assert!(transient_password_active(&shadow));

        // What mos-shadow-reconcile does on the next boot.
        std::fs::remove_file(transient_marker_path(&shadow)).unwrap();
        assert!(!transient_password_active(&shadow));
    }

    #[test]
    fn an_empty_marker_does_not_count_as_active() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());
        std::fs::write(transient_marker_path(&shadow), "").unwrap();

        assert!(
            !transient_password_active(&shadow),
            "an empty marker carries no hash, so nothing is set"
        );
    }

    #[test]
    fn a_second_call_replaces_both_files_and_they_still_agree() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());
        let marker = transient_marker_path(&shadow);

        set_transient_root_password(&shadow, GOOD).unwrap();
        let first_hash = stored_root_hash(&shadow);
        let first_marker = std::fs::read_to_string(&marker).unwrap();

        set_transient_root_password(&shadow, "a different one").unwrap();

        let second_hash = stored_root_hash(&shadow);
        assert_ne!(
            second_hash, first_hash,
            "bcrypt salts every hash, so a replacement must differ even for the same password"
        );
        assert_ne!(std::fs::read_to_string(&marker).unwrap(), first_marker);
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            format!("{second_hash}\n"),
            "the marker must track the hash that is actually in the shadow file"
        );
        assert!(bcrypt::verify("a different one", &second_hash).unwrap());
        assert!(!bcrypt::verify(GOOD, &second_hash).unwrap());
    }

    #[test]
    fn re_setting_the_same_password_still_produces_an_agreeing_pair() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());
        let marker = transient_marker_path(&shadow);

        set_transient_root_password(&shadow, GOOD).unwrap();
        let first = stored_root_hash(&shadow);
        set_transient_root_password(&shadow, GOOD).unwrap();
        let second = stored_root_hash(&shadow);

        assert_ne!(first, second, "a fresh salt on every call");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            format!("{second}\n")
        );
    }

    #[test]
    fn a_shadow_file_without_a_root_entry_writes_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("shadow");
        std::fs::write(&shadow, OTHER_ACCOUNTS).unwrap();

        let err = set_transient_root_password(&shadow, GOOD).unwrap_err();

        assert!(format!("{err:#}").contains("no `root:` entry"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&shadow).unwrap(), OTHER_ACCOUNTS);
        assert!(
            !transient_marker_path(&shadow).exists(),
            "no marker may outlive a shadow file that was never written"
        );
    }

    #[test]
    fn a_missing_shadow_file_is_an_error_and_is_not_created() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("shadow");

        let err = set_transient_root_password(&shadow, GOOD).unwrap_err();

        assert!(format!("{err:#}").contains("read "), "{err:#}");
        assert!(!shadow.exists());
        assert!(!transient_marker_path(&shadow).exists());
    }

    #[test]
    fn the_write_preserves_mode_and_ownership() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let shadow = fixture(dir.path());
        let before = std::fs::metadata(&shadow).unwrap();
        let (uid, gid) = (before.uid(), before.gid());
        // Only root may hand a file to another group; where that is possible,
        // assert against a gid the process would not produce by accident.
        let foreign_gid = std::os::unix::fs::chown(&shadow, None, Some(12))
            .is_ok()
            .then_some(12);

        set_transient_root_password(&shadow, GOOD).unwrap();

        let after = std::fs::metadata(&shadow).unwrap();
        assert_eq!(after.permissions().mode() & 0o7777, SHADOW_MODE);
        assert_eq!(after.uid(), uid);
        assert_eq!(after.gid(), foreign_gid.unwrap_or(gid));
    }

    // ---- the moved shadow primitives --------------------------------------

    #[test]
    fn rewrite_preserves_field_count_ordering_and_a_missing_trailing_newline() {
        let without_newline = SHADOW.trim_end_matches('\n');

        let updated = rewrite_root_hash(without_newline, CRYPT_HASH).unwrap();

        assert_eq!(
            updated,
            format!("root:{CRYPT_HASH}:19000:0:99999:7:::\n{OTHER_ACCOUNTS}")
                .trim_end_matches('\n')
        );
        assert!(!updated.ends_with('\n'));
        for line in updated.lines() {
            assert_eq!(line.split(':').count(), 9, "field count changed: {line}");
        }
    }

    #[test]
    fn rewrite_does_not_match_an_account_merely_containing_root() {
        let shadow = "chroot:!:19000:0:99999:7:::\nroot:!:19000:0:99999:7:::\n";

        let updated = rewrite_root_hash(shadow, CRYPT_HASH).unwrap();

        assert_eq!(
            updated,
            format!("chroot:!:19000:0:99999:7:::\nroot:{CRYPT_HASH}:19000:0:99999:7:::\n")
        );
    }

    #[test]
    fn root_hash_reads_the_root_entry_not_a_lookalike() {
        assert_eq!(root_hash(SHADOW).unwrap(), "!");
        assert_eq!(
            root_hash("chroot:LOOKALIKE:1::::::\nroot:REAL:1::::::\n").unwrap(),
            "REAL"
        );
        assert!(root_hash("daemon:*:1::::::\n").is_err());
    }
}
