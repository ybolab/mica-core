//! On-device identity and per-device credential generation.
//!
//! The rootfs is squashfs + dm-verity: read-only and byte-identical across the
//! fleet, so nothing secret can be baked into the image — it would be a
//! fleet-wide shared secret and would make the verity root hash depend on a
//! random value. A device instead gives itself an identity and its credentials
//! here, at first boot, from the system CSPRNG, persisted to the STATE
//! partition (`/var/lib/mica`, the only writable place that survives an A/B
//! update): the device password, which authenticates the operator on SSH, the
//! local console and the apid admin UI, and the AP PSK, the WPA2 pre-shared key
//! for provisioning AP mode. [`ensure_identity`] is idempotent and never
//! regenerates an existing credential, which would lock the operator out of a
//! fielded device. Wiring it into startup, reconciling the password into
//! `/etc/shadow` and rendering the PSK into `hostapd.conf` live elsewhere.

use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, SaltString};
// Only `verify_password` needs these, and that function is `#[cfg(test)]`.
#[cfg(test)]
use argon2::password_hash::{PasswordHash, PasswordVerifier};
use micad_settings::Settings;
use ring::rand::{SecureRandom, SystemRandom};

/// Default STATE-backed directory holding the settings file and the secrets.
///
/// `/var/lib/mica` is a bind mount whose source is `/mnt/data/state/mica`.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/mica";

/// Sub-directory of the state directory holding plaintext secrets.
const SECRETS_DIR: &str = "secrets";

/// File holding the plaintext device password.
const DEVICE_PASSWORD_FILE: &str = "device-password";

/// File holding the plaintext WPA2 pre-shared key for provisioning AP mode.
const AP_PSK_FILE: &str = "ap-psk";

/// Mode of the secrets directory: owner-only, so a non-root local process
/// cannot even list it.
const SECRETS_DIR_MODE: u32 = 0o700;

/// Mode of every secret file: owner read/write only.
const SECRET_FILE_MODE: u32 = 0o600;

/// Alphabet for generated secrets: digits and uppercase letters minus the
/// six characters an operator cannot reliably tell apart when reading a label
/// (`0`, `O`, `o`, `1`, `l`, `I`). Lowercase is excluded wholesale, which
/// removes `o` and `l`; `0`, `1`, `O` and `I` are removed by hand. Exactly 32
/// characters remain, so each one carries 5 bits.
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// Length of a generated secret in characters.
///
/// 16 characters of a 32-symbol alphabet is 80 bits of entropy, and sits inside
/// the 8..=63 character range WPA2 requires of a pre-shared key.
const SECRET_LEN: usize = 16;

/// Length of the device identifier in random bytes; rendered as 32 hex chars.
const DEVICE_ID_BYTES: usize = 16;

/// Length of the per-hash Argon2 salt in random bytes; the PHC recommendation.
const SALT_BYTES: usize = 16;

/// Largest byte value plus one that may be folded into [`ALPHABET`] without
/// bias: the biggest multiple of the alphabet length that fits in a byte.
const UNBIASED_LIMIT: usize = 256 - (256 % ALPHABET.len());

/// What [`ensure_identity`] had to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// At least one part of the identity was missing and has been generated.
    Created,
    /// Everything was already present; nothing was generated or rewritten.
    AlreadyPresent,
}

/// Give this device an identity and its per-device secrets, if it has none.
///
/// Generates only the parts that are absent: `provisioning.device_id` (16
/// random bytes as lowercase hex); `access.device.password_hash` plus the
/// plaintext device password at `<state_dir>/secrets/device-password`, bumping
/// `access.device.generation`; and the AP PSK at `<state_dir>/secrets/ap-psk`.
/// The two secrets are drawn independently — the plan reads as though
/// the AP PSK could be the device password again, but recovering the WiFi PSK
/// would then also hand over the root shell, and a second CSPRNG draw costs
/// nothing. The settings tree is mutated in place and not saved: the caller
/// owns the single atomic write, and `provisioning.state` is left alone.
/// Re-running on a provisioned device returns [`Outcome::AlreadyPresent`];
/// after an interrupted first boot the missing half is completed and the
/// present half left byte-identical, and a `password_hash` whose plaintext file
/// is missing is not regenerated, the hash being the credential of record.
///
/// # Errors
///
/// Returns an error when the system CSPRNG is unavailable, when hashing fails,
/// or when the state directory cannot be created or written.
pub fn ensure_identity(state_dir: &Path, settings: &mut Settings) -> Result<Outcome> {
    let rng = SystemRandom::new();
    let mut created = false;

    if settings.provisioning.device_id.is_none() {
        settings.provisioning.device_id = Some(generate_device_id(&rng)?);
        created = true;
    }

    if settings.access.device.password_hash.is_none() {
        let password = generate_secret(&rng, SECRET_LEN)?;
        let hash = hash_password(&password)?;
        // Plaintext first, hash second: the caller's settings save is the
        // commit point, so a crash in between orphans a plaintext that the next
        // boot overwrites, rather than stranding a hash with no way to learn
        // the password it stands for.
        write_secret(state_dir, DEVICE_PASSWORD_FILE, &password)?;
        settings.access.device.password_hash = Some(hash);
        settings.access.device.generation = settings.access.device.generation.saturating_add(1);
        created = true;
    }

    if read_secret(state_dir, AP_PSK_FILE)?.is_none() {
        let psk = generate_secret(&rng, SECRET_LEN)?;
        write_secret(state_dir, AP_PSK_FILE, &psk)?;
        created = true;
    }

    Ok(if created {
        Outcome::Created
    } else {
        Outcome::AlreadyPresent
    })
}

/// Read the plaintext device password from STATE, if it has been generated.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read.
// Compiled only for tests: production code has no reader — the credential of
// record for shell access is an SSH key or a transient password, and nothing
// else consumes the file yet. The provisioning tests still need to assert the
// secret landed where a future reader will look, which is what this is for. A
// future access reconciler that wants it lifts the cfg rather than
// reintroducing an allow(dead_code) that outlives its truth.
#[cfg(test)]
pub fn read_device_password(state_dir: &Path) -> Result<Option<String>> {
    read_secret(state_dir, DEVICE_PASSWORD_FILE)
}

/// Read the plaintext AP PSK from STATE, if it has been generated.
///
/// # Errors
///
/// Returns an error when the file exists but cannot be read.
pub fn read_ap_psk(state_dir: &Path) -> Result<Option<String>> {
    read_secret(state_dir, AP_PSK_FILE)
}

/// Hash `password` with Argon2id default parameters into a PHC string.
///
/// The parameters match apid's own hasher, so a hash written here verifies
/// there and vice versa. The salt comes from [`SystemRandom`] rather than
/// `password_hash`'s `OsRng`, which is gated behind a `rand_core` feature this
/// crate does not otherwise pull in; both are the operating system CSPRNG.
///
/// # Errors
///
/// Returns an error when the system CSPRNG is unavailable or the Argon2 backend
/// rejects the input.
pub fn hash_password(password: &str) -> Result<String> {
    let mut salt_bytes = [0u8; SALT_BYTES];
    SystemRandom::new()
        .fill(&mut salt_bytes)
        .map_err(|_| anyhow!("system CSPRNG unavailable"))?;
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|err| anyhow!("encode salt: {err}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow!("hash password: {err}"))
}

/// True when `password` matches the PHC-formatted `hash`.
///
/// A malformed hash verifies as false rather than erroring: a corrupt stored
/// credential must reject every password, not accept any.
///
/// **Test-only.** Nothing outside this module's tests calls it: the access and
/// connd reconcilers verify nothing against `access.device.passwordHash`, and
/// apid's admin login uses its own `apid::auth::verify_password`. It is kept
/// rather than deleted because two `ensure_identity` tests use it as their
/// assertion mechanism — "the stored hash verifies against the stored
/// plaintext" — and deleting it would either drop those assertions or re-inline
/// Argon2 twice. `#[cfg(test)]` is what keeps it from being a
/// security-control-shaped public API that nothing has wired up.
#[must_use]
#[cfg(test)]
fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash)
        .and_then(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed))
        .is_ok()
}

/// Draw a fresh device identifier: [`DEVICE_ID_BYTES`] CSPRNG bytes as
/// lowercase hex. Independent of both secrets, so learning one tells nothing
/// about the others.
fn generate_device_id(rng: &SystemRandom) -> Result<String> {
    let mut bytes = [0u8; DEVICE_ID_BYTES];
    rng.fill(&mut bytes)
        .map_err(|_| anyhow!("system CSPRNG unavailable"))?;
    Ok(hex::encode(bytes))
}

/// Draw `len` characters uniformly from [`ALPHABET`] using the system CSPRNG.
///
/// Bytes at or above [`UNBIASED_LIMIT`] are discarded rather than folded into
/// range, because `byte % N` over a full 0..=255 byte makes the first
/// `256 % N` symbols more likely than the rest — a modulo bias that quietly
/// costs entropy. `ALPHABET` is 32 long and 256 is a multiple of 32, so today
/// the limit is 256 and no byte is ever actually rejected. The rejection stays
/// anyway: what makes the mapping unbiased is the alphabet's length, and the
/// alphabet is the part likely to change.
fn generate_secret(rng: &SystemRandom, len: usize) -> Result<String> {
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        rng.fill(&mut buf)
            .map_err(|_| anyhow!("system CSPRNG unavailable"))?;
        for &byte in &buf {
            if usize::from(byte) >= UNBIASED_LIMIT {
                continue;
            }
            out.push(char::from(ALPHABET[usize::from(byte) % ALPHABET.len()]));
            if out.len() == len {
                break;
            }
        }
    }
    Ok(out)
}

/// Path of the secrets directory under `state_dir`.
fn secrets_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(SECRETS_DIR)
}

/// Create the secrets directory if needed and pin it to [`SECRETS_DIR_MODE`].
fn ensure_secrets_dir(state_dir: &Path) -> Result<PathBuf> {
    let dir = secrets_dir(state_dir);
    if !dir.is_dir() {
        if let Some(parent) = dir.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create state directory {}", parent.display()))?;
        }
        // mkdir(2) carries the mode, so the directory is never momentarily
        // group- or world-readable between creation and chmod.
        match DirBuilder::new().mode(SECRETS_DIR_MODE).create(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| format!("create {}", dir.display()));
            }
        }
    }
    // Unconditional, and not masked by the umask the way mkdir's mode is: a
    // directory left behind by an older or interrupted run gets tightened.
    fs::set_permissions(&dir, Permissions::from_mode(SECRETS_DIR_MODE))
        .with_context(|| format!("chmod {}", dir.display()))?;
    Ok(dir)
}

/// Write `value` to `<state_dir>/secrets/<name>` atomically and at
/// [`SECRET_FILE_MODE`].
///
/// Same shape as `micad_settings::Store::save`: write a temp file beside the
/// target, fsync it, rename it over the target, fsync the directory. The mode
/// is set on the temp file before the rename, so the secret is never reachable
/// under its final name at a laxer mode.
fn write_secret(state_dir: &Path, name: &str, value: &str) -> Result<()> {
    let dir = ensure_secrets_dir(state_dir)?;
    let target = dir.join(name);
    let temp = dir.join(format!(".{name}.tmp"));

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(SECRET_FILE_MODE)
        .open(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    // `OpenOptions::mode` applies only when the file is created, and is masked
    // by the umask; this is what actually guarantees the mode, including when a
    // temp file survived an interrupted earlier run.
    file.set_permissions(Permissions::from_mode(SECRET_FILE_MODE))
        .with_context(|| format!("chmod {}", temp.display()))?;
    file.write_all(value.as_bytes())
        .with_context(|| format!("write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("fsync {}", temp.display()))?;
    drop(file);

    fs::rename(&temp, &target)
        .with_context(|| format!("rename {} to {}", temp.display(), target.display()))?;
    File::open(&dir)
        .and_then(|handle| handle.sync_all())
        .with_context(|| format!("fsync {}", dir.display()))?;
    Ok(())
}

/// Read `<state_dir>/secrets/<name>`, or `None` when it does not exist.
///
/// Trailing whitespace is trimmed: the alphabet contains none, so trimming
/// cannot corrupt a generated secret, and it keeps a file an operator opened in
/// an editor (which appends a newline) from producing a PSK hostapd rejects.
fn read_secret(state_dir: &Path, name: &str) -> Result<Option<String>> {
    let path = secrets_dir(state_dir).join(name);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text.trim_end().to_string())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tempfile::TempDir;

    use super::*;

    /// Characters an operator cannot reliably read off a printed label.
    const AMBIGUOUS: &str = "0Oo1lI";

    fn provision() -> (TempDir, Settings) {
        let dir = TempDir::new().expect("tempdir");
        let mut settings = Settings::default();
        let outcome = ensure_identity(dir.path(), &mut settings).expect("ensure_identity");
        assert_eq!(outcome, Outcome::Created);
        (dir, settings)
    }

    fn secret_bytes(state_dir: &Path, name: &str) -> Vec<u8> {
        fs::read(secrets_dir(state_dir).join(name)).expect("read secret file")
    }

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("stat").permissions().mode() & 0o7777
    }

    // A fresh STATE gets a complete identity, and the stored hash is really
    // the hash of the plaintext that was written next to it.
    #[test]
    fn fresh_state_gets_identity_and_both_secrets() {
        let (dir, settings) = provision();

        let device_id = settings
            .provisioning
            .device_id
            .as_deref()
            .expect("device_id set");
        assert_eq!(device_id.len(), DEVICE_ID_BYTES * 2);
        assert!(
            device_id
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "device_id must be lowercase hex, got {device_id}"
        );

        let hash = settings
            .access
            .device
            .password_hash
            .as_deref()
            .expect("password_hash set");
        assert!(
            hash.starts_with("$argon2id$"),
            "not an argon2id PHC: {hash}"
        );
        assert_eq!(settings.access.device.generation, 1);

        let password = read_device_password(dir.path())
            .expect("read password")
            .expect("password present");
        let psk = read_ap_psk(dir.path())
            .expect("read psk")
            .expect("psk present");
        assert_eq!(password.len(), SECRET_LEN);
        assert_eq!(psk.len(), SECRET_LEN);
        assert_ne!(password, psk);

        assert!(
            verify_password(hash, &password),
            "stored hash does not verify against the stored plaintext"
        );

        // Untouched: declaring first boot finished is the caller's call.
        assert_eq!(
            settings.provisioning.state,
            micad_settings::ProvisioningState::Pending
        );
    }

    // A second call must not regenerate anything: a regenerated credential
    // locks the operator out of a fielded device.
    #[test]
    fn second_call_is_a_genuine_no_op() {
        let (dir, first) = provision();
        let password_before = secret_bytes(dir.path(), DEVICE_PASSWORD_FILE);
        let psk_before = secret_bytes(dir.path(), AP_PSK_FILE);

        let mut second = first.clone();
        let outcome = ensure_identity(dir.path(), &mut second).expect("ensure_identity");

        assert_eq!(outcome, Outcome::AlreadyPresent);
        assert_eq!(second, first, "settings tree changed on a re-run");
        assert_eq!(
            secret_bytes(dir.path(), DEVICE_PASSWORD_FILE),
            password_before
        );
        assert_eq!(secret_bytes(dir.path(), AP_PSK_FILE), psk_before);
    }

    // Interrupted first boot that got as far as the device_id: only the
    // credential half is completed.
    #[test]
    fn partial_state_with_device_id_only_completes_the_credential() {
        let dir = TempDir::new().expect("tempdir");
        let mut settings = Settings::default();
        settings.provisioning.device_id = Some("00112233445566778899aabbccddeeff".to_string());

        let outcome = ensure_identity(dir.path(), &mut settings).expect("ensure_identity");

        assert_eq!(outcome, Outcome::Created);
        assert_eq!(
            settings.provisioning.device_id.as_deref(),
            Some("00112233445566778899aabbccddeeff"),
            "the present half was regenerated"
        );
        let hash = settings
            .access
            .device
            .password_hash
            .as_deref()
            .expect("password_hash completed");
        let password = read_device_password(dir.path())
            .expect("read password")
            .expect("password present");
        assert!(verify_password(hash, &password));
        assert_eq!(settings.access.device.generation, 1);
        assert!(read_ap_psk(dir.path()).expect("read psk").is_some());
    }

    // The other direction: credential present, device_id missing. The
    // credential must come through byte-identical, hash and plaintext alike.
    #[test]
    fn partial_state_with_credential_only_completes_the_device_id() {
        let (dir, first) = provision();
        let password_before = secret_bytes(dir.path(), DEVICE_PASSWORD_FILE);
        let psk_before = secret_bytes(dir.path(), AP_PSK_FILE);

        let mut settings = first.clone();
        settings.provisioning.device_id = None;
        let outcome = ensure_identity(dir.path(), &mut settings).expect("ensure_identity");

        assert_eq!(outcome, Outcome::Created);
        assert_eq!(
            settings.access.device, first.access.device,
            "the credential half was rewritten"
        );
        assert_eq!(
            secret_bytes(dir.path(), DEVICE_PASSWORD_FILE),
            password_before
        );
        assert_eq!(secret_bytes(dir.path(), AP_PSK_FILE), psk_before);
        let device_id = settings
            .provisioning
            .device_id
            .as_deref()
            .expect("device_id completed");
        assert_eq!(device_id.len(), DEVICE_ID_BYTES * 2);
        assert_ne!(
            Some(device_id),
            first.provisioning.device_id.as_deref(),
            "a fresh draw should not reproduce the old identifier"
        );
    }

    // The central guard: nothing generated here may be a fleet-wide constant,
    // and the two secrets of one device must be independent draws rather than
    // one string used twice.
    #[test]
    fn two_hundred_devices_share_no_secret() {
        const DEVICES: usize = 200;
        // Argon2id at apid's parameters is deliberately expensive, and an
        // unoptimised test build pays that 200 times; spread it over the
        // machine so the check gate stays usable.
        let lanes = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

        let drawn: Vec<(String, String, String)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..lanes)
                .map(|lane| {
                    let count = DEVICES / lanes + usize::from(lane < DEVICES % lanes);
                    scope.spawn(move || {
                        (0..count)
                            .map(|_| {
                                let (dir, settings) = provision();
                                let password = read_device_password(dir.path())
                                    .expect("read password")
                                    .expect("password present");
                                let psk = read_ap_psk(dir.path())
                                    .expect("read psk")
                                    .expect("psk present");
                                let device_id =
                                    settings.provisioning.device_id.expect("device_id set");
                                (password, psk, device_id)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("lane panicked"))
                .collect()
        });
        assert_eq!(drawn.len(), DEVICES);

        let mut passwords = HashSet::new();
        let mut psks = HashSet::new();
        let mut device_ids = HashSet::new();
        for (password, psk, device_id) in drawn {
            assert_ne!(
                password, psk,
                "the device password and the AP PSK are the same string"
            );
            passwords.insert(password);
            psks.insert(psk);
            device_ids.insert(device_id);
        }

        assert_eq!(passwords.len(), DEVICES, "device passwords repeat");
        assert_eq!(psks.len(), DEVICES, "AP PSKs repeat");
        assert_eq!(device_ids.len(), DEVICES, "device identifiers repeat");
        assert_eq!(
            passwords.union(&psks).count(),
            DEVICES * 2,
            "a password on one device is an AP PSK on another"
        );
    }

    // Read the bits back rather than trusting the create call: an inherited
    // umask or a temp file left by an earlier run could widen either. The
    // expected modes are written out as literals, not as the constants the
    // implementation uses, so that widening a constant fails here instead of
    // silently moving the goalposts.
    #[test]
    fn secret_files_are_0600_inside_a_0700_directory() {
        let (dir, _) = provision();
        let secrets = secrets_dir(dir.path());

        assert_eq!(mode_of(&secrets), 0o700);
        assert_eq!(mode_of(&secrets.join(DEVICE_PASSWORD_FILE)), 0o600);
        assert_eq!(mode_of(&secrets.join(AP_PSK_FILE)), 0o600);
    }

    // A pre-existing world-readable directory and a leftover temp file from an
    // interrupted run must both be tightened, not inherited.
    #[test]
    fn a_lax_pre_existing_secrets_directory_is_tightened() {
        let dir = TempDir::new().expect("tempdir");
        let secrets = secrets_dir(dir.path());
        fs::create_dir_all(&secrets).expect("mkdir");
        fs::set_permissions(&secrets, Permissions::from_mode(0o777)).expect("chmod");
        let stale = secrets.join(format!(".{DEVICE_PASSWORD_FILE}.tmp"));
        fs::write(&stale, b"stale").expect("write stale temp");
        fs::set_permissions(&stale, Permissions::from_mode(0o666)).expect("chmod stale");

        let mut settings = Settings::default();
        ensure_identity(dir.path(), &mut settings).expect("ensure_identity");

        assert_eq!(mode_of(&secrets), 0o700);
        assert_eq!(mode_of(&secrets.join(DEVICE_PASSWORD_FILE)), 0o600);
        assert!(!stale.exists(), "temp file left behind after rename");
    }

    // Alphabet and length, over enough characters that a stray symbol would
    // show up.
    #[test]
    fn generated_secrets_use_only_the_unambiguous_alphabet() {
        let rng = SystemRandom::new();
        let alphabet: HashSet<char> = ALPHABET.iter().map(|&b| char::from(b)).collect();
        assert_eq!(alphabet.len(), 32, "alphabet is not 32 distinct characters");
        for c in AMBIGUOUS.chars() {
            assert!(!alphabet.contains(&c), "ambiguous {c} is in the alphabet");
        }

        let mut seen = 0usize;
        for _ in 0..250 {
            let secret = generate_secret(&rng, SECRET_LEN).expect("generate");
            assert_eq!(secret.len(), SECRET_LEN);
            for c in secret.chars() {
                assert!(alphabet.contains(&c), "{c} is outside the alphabet");
                assert!(!AMBIGUOUS.contains(c), "ambiguous {c} was generated");
                seen += 1;
            }
        }
        assert_eq!(
            seen,
            250 * SECRET_LEN,
            "fewer characters checked than drawn"
        );
    }

    // A near miss is a miss.
    #[test]
    fn verify_password_rejects_a_one_character_miss() {
        let (dir, settings) = provision();
        let hash = settings
            .access
            .device
            .password_hash
            .expect("password_hash set");
        let password = read_device_password(dir.path())
            .expect("read password")
            .expect("password present");

        assert!(verify_password(&hash, &password));

        let mut near_miss = password.clone();
        let last = near_miss.pop().expect("non-empty");
        near_miss.push(if last == '2' { '3' } else { '2' });
        assert_ne!(near_miss, password);
        assert!(!verify_password(&hash, &near_miss), "near miss accepted");
        assert!(!verify_password("not a phc string", &password));
    }

    // Assert the invariant on the document that actually lands on STATE, rather
    // than trusting the types to have kept the plaintext out.
    #[test]
    fn settings_document_contains_no_plaintext_secret() {
        let (dir, settings) = provision();
        let password = read_device_password(dir.path())
            .expect("read password")
            .expect("password present");
        let psk = read_ap_psk(dir.path())
            .expect("read psk")
            .expect("psk present");

        let document = toml::to_string(&settings).expect("serialize settings");

        assert!(
            !document.contains(&password),
            "the device password is in settings.toml:\n{document}"
        );
        assert!(
            !document.contains(&psk),
            "the AP PSK is in settings.toml:\n{document}"
        );
        assert!(
            document.contains("passwordHash"),
            "the hash should be there:\n{document}"
        );
    }
}
