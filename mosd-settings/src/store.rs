//! Atomic TOML persistence for the settings tree.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::error::SettingsError;
use crate::migration::migrate;
use crate::model::{SCHEMA_VERSION, Settings};

/// What the tolerant newer-schema load did, for the caller to log loudly.
///
/// Produced only when the on-disk `schema_version` was greater than
/// [`SCHEMA_VERSION`] — i.e. on the A/B rollback path. See
/// [`Store::load_with_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    /// The newer schema version the document carried.
    pub from: u32,
    /// Keys stripped to make the document parse, in the order they were
    /// dropped. Recursive: a same-named key elsewhere in the tree is dropped
    /// by the same pass.
    pub dropped_keys: Vec<String>,
    /// True when stripping was not enough — the newer schema reshaped an
    /// existing key — and the load fell back to [`Settings::default`],
    /// abandoning every stored setting including the admin credential.
    pub defaulted: bool,
}

/// The field name out of a serde `deny_unknown_fields` rejection, if that is
/// what `message` is.
///
/// Serde spells it `` unknown field `name`, expected ... `` and toml carries
/// the message through; the toml workspace pin (`=0.9`-line) keeps the
/// spelling stable, and the tolerant-load test fails loudly if it drifts.
fn unknown_field_name(message: &str) -> Option<String> {
    let rest = message.split("unknown field `").nth(1)?;
    let (name, _) = rest.split_once('`')?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Remove every key named `key` anywhere in `table`, recursively (arrays of
/// tables included). Returns whether anything was removed.
fn strip_key(table: &mut toml::Table, key: &str) -> bool {
    let mut removed = table.remove(key).is_some();
    for (_, value) in table.iter_mut() {
        removed |= strip_key_in_value(value, key);
    }
    removed
}

fn strip_key_in_value(value: &mut toml::Value, key: &str) -> bool {
    match value {
        toml::Value::Table(inner) => strip_key(inner, key),
        toml::Value::Array(items) => {
            let mut removed = false;
            for item in items {
                removed |= strip_key_in_value(item, key);
            }
            removed
        }
        _ => false,
    }
}

/// Default on-disk location of the settings file.
pub const DEFAULT_PATH: &str = "/var/lib/mos/settings.toml";

/// TOML-backed settings store with atomic writes and load-time migrations.
#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// Store backed by the file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Store backed by [`DEFAULT_PATH`].
    #[must_use]
    pub fn default_path() -> Self {
        Self::new(DEFAULT_PATH)
    }

    /// Load settings from disk, discarding the rollback report.
    ///
    /// See [`Store::load_with_report`] for the full contract; this wrapper
    /// exists so callers that cannot log (tests, one-shot tools) keep the
    /// short call. mosd itself calls [`Store::load_with_report`] and logs.
    ///
    /// # Errors
    ///
    /// As [`Store::load_with_report`].
    pub fn load(&self) -> Result<Settings, SettingsError> {
        self.load_with_report().map(|(settings, _)| settings)
    }

    /// Load settings from disk.
    ///
    /// A missing file yields [`Settings::default`] without creating the file.
    /// An existing document at or below [`SCHEMA_VERSION`] is migrated up
    /// before deserialization (a missing `schema_version` key means version
    /// 0); this path never yields a report.
    ///
    /// **A document from a NEWER schema loads tolerantly instead of failing**
    /// (`docs/design/api.md` §10.3 item 5; `docs/design/mosd.md` §5.2). This
    /// is the A/B rollback path: the other slot ran a newer mosd, wrote its
    /// schema to STATE, and this slot was rolled back to. Refusing such a
    /// document makes mosd propagate the error and exit, and under
    /// `Restart=on-failure` the rolled-back-to slot is then a crash loop —
    /// which also fails that slot's health gate, so a rollback whose whole
    /// point is reaching a working slot produces a device with no confirmable
    /// slot at all. The down-migrations cannot help here by construction: this
    /// binary cannot carry the migration a future schema will need.
    ///
    /// What "tolerantly" means is exactly what `mosd.md` §5.2 already defines
    /// a rollback to cost: **keys this schema does not know are dropped, and
    /// the device falls back to this version's behaviour.** Mechanically,
    /// unknown keys named by serde's `deny_unknown_fields` rejections are
    /// stripped one at a time (recursively — a same-named key at another
    /// position is dropped too, accepted and recorded below) until the
    /// document parses. The next [`Store::save`] persists the stripped
    /// document at this schema version, which is the documented
    /// "rolling forward again restores the defaults" behaviour.
    ///
    /// If the newer document still cannot be parsed after stripping — a
    /// future schema **reshaped** an existing key rather than adding one —
    /// the last resort is [`Settings::default`], reported, not an error.
    /// Written acceptance of what that costs: every setting including the
    /// admin password hash is abandoned and the device re-enters setup mode
    /// on its LAN. That is a real loss, accepted deliberately, because the
    /// alternative is the crash loop above: an unmanageable device on BOTH
    /// slots versus a manageable device that must be set up again. Schema
    /// authors owe the mitigation: **prefer additive bumps; a reshaping bump
    /// forfeits settings on rollback and must say so in its migration.**
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Io`] on read failures, [`SettingsError::Parse`]
    /// on invalid TOML (including a malformed `schema_version`), and
    /// [`SettingsError::Migration`] when an upward walk fails. A document
    /// newer than [`SCHEMA_VERSION`] never errors.
    pub fn load_with_report(&self) -> Result<(Settings, Option<RollbackReport>), SettingsError> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok((Settings::default(), None));
            }
            Err(err) => return Err(err.into()),
        };
        let mut doc: toml::Table = text
            .parse()
            .map_err(|err: toml::de::Error| SettingsError::Parse(err.to_string()))?;
        let from = match doc.get("schema_version") {
            None => 0,
            Some(toml::Value::Integer(version)) => u32::try_from(*version).map_err(|_| {
                SettingsError::Parse(format!("schema_version {version} out of range"))
            })?,
            Some(other) => {
                return Err(SettingsError::Parse(format!(
                    "schema_version must be an integer, got {other}"
                )));
            }
        };
        if from > SCHEMA_VERSION {
            return Ok(Self::load_newer(doc, from));
        }
        migrate(&mut doc, from, SCHEMA_VERSION)?;
        let migrated =
            toml::to_string(&doc).map_err(|err| SettingsError::Parse(err.to_string()))?;
        let settings =
            toml::from_str(&migrated).map_err(|err| SettingsError::Parse(err.to_string()))?;
        Ok((settings, None))
    }

    /// The tolerant path for a document newer than [`SCHEMA_VERSION`].
    ///
    /// Infallible by design — see [`Store::load_with_report`] for why the
    /// rollback path must not be able to fail the load.
    fn load_newer(mut doc: toml::Table, from: u32) -> (Settings, Option<RollbackReport>) {
        // The version stamp itself is the first "key this schema does not
        // recognise the value of": rewrite it to ours so the parse below is
        // over a document claiming the schema it is being read as.
        doc.insert(
            "schema_version".to_string(),
            toml::Value::Integer(i64::from(SCHEMA_VERSION)),
        );
        let mut dropped = Vec::new();
        // Bounded: each pass must strip at least one key or the loop ends.
        // The bound itself is defensive; a document has finitely many keys.
        for _ in 0..64 {
            let text = match toml::to_string(&doc) {
                Ok(text) => text,
                Err(_) => break,
            };
            match toml::from_str::<Settings>(&text) {
                Ok(settings) => {
                    return (
                        settings,
                        Some(RollbackReport {
                            from,
                            dropped_keys: dropped,
                            defaulted: false,
                        }),
                    );
                }
                Err(err) => {
                    let Some(key) = unknown_field_name(&err.to_string()) else {
                        break; // reshaped, not additive: fall through
                    };
                    if !strip_key(&mut doc, &key) {
                        break; // named key not found: cannot make progress
                    }
                    dropped.push(key);
                }
            }
        }
        (
            Settings::default(),
            Some(RollbackReport {
                from,
                dropped_keys: dropped,
                defaulted: true,
            }),
        )
    }

    /// Persist `settings` atomically: write to a temp file in the target
    /// directory, fsync it, rename it over the target, then fsync the
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Io`] on filesystem failures and
    /// [`SettingsError::Parse`] when serialization fails.
    pub fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        let parent = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        fs::create_dir_all(parent)?;
        let text =
            toml::to_string(settings).map_err(|err| SettingsError::Parse(err.to_string()))?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(text.as_bytes())?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|err| err.error)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v6 `network` entry, exactly as the rolled-back-to binary declares it:
    /// `dhcp`, an optional `static` block, and `deny_unknown_fields`. It is
    /// spelled out here rather than imported because the type it mirrors no
    /// longer exists in this crate — v7 is what [`Settings`] now is — and the
    /// question this test asks is what the *old* struct would accept.
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct V6Iface {
        dhcp: bool,
        #[serde(rename = "static", default)]
        static_: Option<toml::Table>,
    }

    /// The A/B rollback shape: a v7 tree of all three new kinds, read by a v6
    /// binary that has no migration for it.
    ///
    /// [`Store::load_newer`] strips the keys serde names, by name, everywhere,
    /// until the document parses. Against v6's `IfaceSettings` those names are
    /// `kind`, `vlan`, `bridge` and `wireguard`, and this asserts the fixed
    /// point of stripping them: every entry survives as a *physical stub* — a
    /// body v6 deserializes — rather than the whole document failing to load.
    /// The stub's rendered `.network` matches no device (its `.netdev` lived in
    /// `/run` and is gone at reboot), so the device falls back to the image
    /// default on its physical NICs: degraded to the pre-v7 feature set, and
    /// still reachable, which is the whole point of the tolerant path.
    #[test]
    fn a_v7_tree_strips_down_to_v6_physical_stubs() {
        let mut doc: toml::Table = r#"
schema_version = 7
hostname = "rolled-back"

[network.eth0]
dhcp = true

[network."eth0.100"]
kind = "vlan"
dhcp = false

[network."eth0.100".static]
address = "192.168.100.2/24"

[network."eth0.100".vlan]
parent = "eth0"
id = 100

[network.br0]
kind = "bridge"
dhcp = true

[network.br0.bridge]
ports = ["eth1", "eth2"]

[network.wg0]
kind = "wireguard"
dhcp = false

[network.wg0.wireguard]
listenPort = 51820

[[network.wg0.wireguard.peers]]
publicKey = "AI9C8xytM2fi+RUcnV5RvMnSq4ZQffgDZ37h0vc0AU8="
allowedIps = ["10.8.0.0/24"]
"#
        .parse()
        .unwrap();

        for key in ["kind", "vlan", "bridge", "wireguard"] {
            assert!(strip_key(&mut doc, key), "{key} was not there to strip");
        }

        let network = doc["network"].as_table().unwrap();
        assert_eq!(
            network.keys().collect::<Vec<_>>(),
            vec!["br0", "eth0", "eth0.100", "wg0"],
            "no entry is dropped by the strip: v6 keeps them all, as stubs"
        );
        for (name, entry) in network {
            let stub: V6Iface = entry
                .clone()
                .try_into()
                .unwrap_or_else(|err| panic!("{name} is not a body v6 could deserialize: {err}"));
            // What v6 can still act on survives; the peer's public key, the
            // VLAN id and the bridge port list are gone with their blocks.
            assert_eq!(stub.dhcp, name == "eth0" || name == "br0");
            assert_eq!(stub.static_.is_some(), name == "eth0.100");
        }
        let text = toml::to_string(&doc).unwrap();
        for gone in ["publicKey", "listenPort", "ports", "parent"] {
            assert!(!text.contains(gone), "{gone} survived the strip: {text}");
        }

        // A second pass has nothing left to take.
        for key in ["kind", "vlan", "bridge", "wireguard"] {
            assert!(!strip_key(&mut doc, key));
        }
    }
}
