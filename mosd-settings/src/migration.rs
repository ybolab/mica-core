//! Bottlerocket-style bidirectional schema migrations over TOML documents.

use crate::error::SettingsError;

/// One schema migration step between adjacent versions.
///
/// `up` migrates a document from `target_version() - 1` to `target_version()`;
/// `down` reverses it.
pub trait Migration {
    /// Version this migration's `up` step produces.
    fn target_version(&self) -> u32;
    /// Migrate a document from `target_version() - 1` to `target_version()`.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Migration`] when the document cannot be migrated.
    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError>;
    /// Migrate a document from `target_version()` back to `target_version() - 1`.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Migration`] when the document cannot be migrated.
    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError>;
}

/// Ordered set of migrations able to walk a document between schema versions.
pub struct MigrationRegistry {
    migrations: Vec<Box<dyn Migration>>,
}

impl MigrationRegistry {
    /// Registry over `migrations`, ordered by target version.
    #[must_use]
    pub fn new(mut migrations: Vec<Box<dyn Migration>>) -> Self {
        migrations.sort_by_key(|migration| migration.target_version());
        Self { migrations }
    }

    /// Walk `doc` from schema version `from` to `to`, applying `up` steps
    /// ascending or `down` steps descending.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError::Migration`] when a required step is missing
    /// (version gap) or a step fails.
    pub fn migrate(&self, doc: &mut toml::Table, from: u32, to: u32) -> Result<(), SettingsError> {
        if from < to {
            for version in from + 1..=to {
                self.find(version)?.up(doc)?;
            }
        } else if from > to {
            for version in (to + 1..=from).rev() {
                self.find(version)?.down(doc)?;
            }
        }
        Ok(())
    }

    fn find(&self, target_version: u32) -> Result<&dyn Migration, SettingsError> {
        self.migrations
            .iter()
            .find(|migration| migration.target_version() == target_version)
            .map(Box::as_ref)
            .ok_or_else(|| {
                SettingsError::Migration(format!(
                    "no migration targeting schema version {target_version}"
                ))
            })
    }
}

impl Default for MigrationRegistry {
    /// Registry holding every migration shipped with this crate.
    fn default() -> Self {
        Self::new(vec![
            Box::new(MigrateV0ToV1),
            Box::new(MigrateV1ToV2),
            Box::new(MigrateV2ToV3),
            Box::new(MigrateV3ToV4),
            Box::new(MigrateV4ToV5),
            Box::new(MigrateV5ToV6),
            Box::new(MigrateV6ToV7),
            Box::new(MigrateV7ToV8),
            Box::new(MigrateV8ToV9),
            Box::new(MigrateV9ToV10),
            Box::new(MigrateV10ToV11),
            Box::new(MigrateV11ToV12),
        ])
    }
}

/// Walk `doc` between schema versions using the built-in migrations.
///
/// # Errors
///
/// Returns [`SettingsError::Migration`] when a required step is missing or fails.
pub fn migrate(doc: &mut toml::Table, from: u32, to: u32) -> Result<(), SettingsError> {
    MigrationRegistry::default().migrate(doc, from, to)
}

/// v0 -> v1: v0 is a legacy document holding only `hostname`.
///
/// `up` stamps `schema_version = 1` and adds an empty `network` table when
/// absent; `down` removes both keys.
pub struct MigrateV0ToV1;

impl Migration for MigrateV0ToV1 {
    fn target_version(&self) -> u32 {
        1
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(1));
        if !doc.contains_key("network") {
            doc.insert(
                "network".to_string(),
                toml::Value::Table(toml::Table::new()),
            );
        }
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.remove("schema_version");
        doc.remove("network");
        Ok(())
    }
}

/// v1 -> v2: adds the apid-owned `access` subtree.
///
/// `up` stamps `schema_version = 2` and adds an empty `access` table when
/// absent; `down` removes the `access` key entirely. Rolling back to v1 drops
/// the web admin password, which is acceptable because v1 software has no
/// apid.
pub struct MigrateV1ToV2;

impl Migration for MigrateV1ToV2 {
    fn target_version(&self) -> u32 {
        2
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(2));
        if !doc.contains_key("access") {
            doc.insert("access".to_string(), toml::Value::Table(toml::Table::new()));
        }
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(1));
        doc.remove("access");
        Ok(())
    }
}

/// v2 -> v3: adds the `provisioning` and `wifi` subtrees and the v3-only keys
/// inside `access` (`ssh`, `console`, `device`).
///
/// `up` stamps `schema_version = 3` and adds empty `provisioning` and `wifi`
/// tables when absent. An existing `access` table is left untouched, so
/// `access.webAdmin` survives verbatim; the v3-only keys inside it come from
/// the model's serde defaults on deserialization.
///
/// `down` stamps `schema_version = 2`, removes `provisioning` and `wifi`, and
/// removes `ssh`, `console` and `device` from `access` while keeping
/// `access.webAdmin`. Rolling back to v2 therefore loses the SSH policy, the
/// console shell policy, and the device credential hash and its generation
/// counter, all deliberately: v2 software has no reconciler for any of them, so
/// keeping the keys would leave a document v2 cannot deserialize
/// (`deny_unknown_fields`) while dropping them costs nothing v2 could have
/// acted on. Rolling forward again restores the v3 defaults, not the values
/// that were there before the rollback.
pub struct MigrateV2ToV3;

impl Migration for MigrateV2ToV3 {
    fn target_version(&self) -> u32 {
        3
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(3));
        for key in ["provisioning", "wifi"] {
            if !doc.contains_key(key) {
                doc.insert(key.to_string(), toml::Value::Table(toml::Table::new()));
            }
        }
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(2));
        doc.remove("provisioning");
        doc.remove("wifi");
        if let Some(toml::Value::Table(access)) = doc.get_mut("access") {
            access.remove("ssh");
            access.remove("console");
            access.remove("device");
        }
        Ok(())
    }
}

/// v3 -> v4: adds `access.ssh.authorizedKeys`, the persistent SSH access list.
///
/// `up` stamps `schema_version = 4` and ensures `access.ssh.authorizedKeys`
/// exists as an empty array, creating `access` and `access.ssh` when the
/// document does not carry them yet. An existing array is left exactly as it
/// is, so running `up` over an already-migrated document changes nothing.
///
/// `down` stamps `schema_version = 3` and removes the key.
pub struct MigrateV3ToV4;

impl Migration for MigrateV3ToV4 {
    fn target_version(&self) -> u32 {
        4
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(4));
        let access = child_table(doc, "access")?;
        let ssh = child_table(access, "ssh")?;
        match ssh.get("authorizedKeys") {
            None => {
                ssh.insert("authorizedKeys".to_string(), toml::Value::Array(Vec::new()));
            }
            Some(toml::Value::Array(_)) => {}
            Some(other) => {
                return Err(SettingsError::Migration(format!(
                    "access.ssh.authorizedKeys must be an array, found a {}",
                    other.type_str()
                )));
            }
        }
        Ok(())
    }

    /// Remove `access.ssh.authorizedKeys`, discarding whatever it held.
    ///
    /// A non-empty list is discarded, not carried forward, and that is the
    /// correct trade rather than a limitation. A v3 image has no code that
    /// renders the list into an `authorized_keys` file, so a document that
    /// kept the keys would be a v3 device promising an access path it cannot
    /// actually serve: the operator would see the keys in the settings tree
    /// and believe they work. Worse, v3 deserializes `access.ssh` with
    /// `deny_unknown_fields`, so a leftover key makes the whole document
    /// unloadable. Dropping the list makes the rollback honest — the operator
    /// falls back to that release's access story, and rolling forward again
    /// starts from an empty list rather than from keys nobody re-authorized.
    ///
    /// A document with no `access` or no `access.ssh` table is left untouched.
    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(3));
        if let Some(toml::Value::Table(access)) = doc.get_mut("access")
            && let Some(toml::Value::Table(ssh)) = access.get_mut("ssh")
        {
            ssh.remove("authorizedKeys");
        }
        Ok(())
    }
}

/// Borrow `doc[key]` as a table, creating an empty one when it is absent.
///
/// # Errors
///
/// Returns [`SettingsError::Migration`] when the key exists but is not a table,
/// which is a hand-edited document rather than an old one.
fn child_table<'doc>(
    doc: &'doc mut toml::Table,
    key: &str,
) -> Result<&'doc mut toml::Table, SettingsError> {
    let entry = doc
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    match entry {
        toml::Value::Table(table) => Ok(table),
        other => Err(SettingsError::Migration(format!(
            "{key} must be a table, found a {}",
            other.type_str()
        ))),
    }
}

/// v4 -> v5: adds the `container` subtree carrying the engine switch.
///
/// `up` stamps `schema_version = 5` and adds `container.enabled = false` when
/// absent. False, not true, and not "whatever the device was doing": a
/// document arriving from v4 was written by software with no container switch,
/// so there is no operator decision to preserve, and the safe reading of an
/// absent decision is the one that runs nothing.
pub struct MigrateV4ToV5;

impl Migration for MigrateV4ToV5 {
    fn target_version(&self) -> u32 {
        5
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(5));
        let container = child_table(doc, "container")?;
        match container.get("enabled") {
            None => {
                container.insert("enabled".to_string(), toml::Value::Boolean(false));
            }
            Some(toml::Value::Boolean(_)) => {}
            Some(other) => {
                return Err(SettingsError::Migration(format!(
                    "container.enabled must be a boolean, found a {}",
                    other.type_str()
                )));
            }
        }
        Ok(())
    }

    /// Remove the `container` subtree, discarding an `enabled = true`.
    ///
    /// Discarding it is the correct trade, for the reason
    /// [`MigrateV3ToV4::down`] discards authorized keys and for one more. v4
    /// software has no `ContainerReconciler`, so a preserved `true` would be a
    /// device whose settings tree claims containers are on while nothing binds
    /// the Quadlet directory or starts a unit. And v4's `Settings` carries
    /// `deny_unknown_fields`, so a leftover `container` table makes the whole
    /// document unloadable; a rollback that bricked settings parsing is far
    /// worse than a switch the operator has to set again.
    ///
    /// A document with no `container` table is left untouched.
    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(4));
        doc.remove("container");
        Ok(())
    }
}
/// v5 -> v6: adds the `mqtt` subtree carrying the broker/bridge master switch.
///
/// `up` stamps `schema_version = 6` and adds `mqtt.enabled = false` when
/// absent. False is not a cautious guess, it is what the device was already
/// doing: no shipped image ever carried a broker, so the bridge has never once
/// connected and every device arriving from v5 has been retrying into nothing.
/// An existing boolean is left exactly as it is, so `up` over an
/// already-migrated document changes nothing.
///
/// `listen` and `auth` are deliberately NOT seeded. Both are
/// `#[serde(default)]`, so an absent table deserializes to the documented
/// defaults; writing them here would put a copy of those defaults into every
/// device's settings file, where they would be indistinguishable from an
/// operator's choice the day a default changes.
pub struct MigrateV5ToV6;

impl Migration for MigrateV5ToV6 {
    fn target_version(&self) -> u32 {
        6
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(6));
        let mqtt = child_table(doc, "mqtt")?;
        match mqtt.get("enabled") {
            None => {
                mqtt.insert("enabled".to_string(), toml::Value::Boolean(false));
            }
            Some(toml::Value::Boolean(_)) => {}
            Some(other) => {
                return Err(SettingsError::Migration(format!(
                    "mqtt.enabled must be a boolean, found a {}",
                    other.type_str()
                )));
            }
        }
        Ok(())
    }

    /// Remove the `mqtt` subtree, discarding an `enabled = true`.
    ///
    /// Discarding it is the correct trade, for the two reasons
    /// [`MigrateV4ToV5::down`] gives for the container switch. v5 software has
    /// no `MqttReconciler`, so a preserved `true` would be a settings tree
    /// announcing that MQTT is on while nothing starts a broker or bridge unit.
    /// And v5's `Settings` carries `deny_unknown_fields`, so a leftover `mqtt`
    /// table does not merely mislead — it makes the whole document fail to
    /// deserialize, taking the hostname, the network configuration and the
    /// admin credential with it. A rollback that bricked settings parsing is
    /// far worse than a switch the operator sets again.
    ///
    /// A document with no `mqtt` table is left untouched.
    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(5));
        doc.remove("mqtt");
        Ok(())
    }
}

/// v6 -> v7: interface kinds — VLAN, bridge and WireGuard.
///
/// `up` stamps `schema_version = 7` and does nothing else, and that is the
/// whole migration. Every field v7 adds to a `network` entry — `kind` and the
/// `vlan`, `bridge` and `wireguard` blocks — is defaulted and skipped on
/// serialization, so a v6 document of physical interfaces and its v7 form
/// differ by the version integer alone. The v7 key-charset rule on `network`
/// keys is a property of the write path, not of the document, so no existing
/// loadable tree is rejected here either.
///
/// `down` stamps `schema_version = 6`, removes `kind` and the three blocks
/// from every entry, and **removes entirely** every entry whose `kind` was not
/// physical. Dropping those entries is [`MigrateV2ToV3::down`]'s trade, twice
/// over. v6's `IfaceSettings` carries `deny_unknown_fields`, so a leftover
/// `vlan` block makes the whole document unloadable — hostname, credential and
/// all. And a v6 reconciler handed a bare `wg0` entry it can no longer explain
/// would render a `.network` unit matching no device: the settings tree would
/// claim a tunnel the device has no code to create. A stripped stub is not a
/// degraded tunnel, it is a lie in unit form, so the entry goes.
///
/// The private key files (`wg-*.key`) are left on STATE, unreferenced: 0640 in
/// a root-owned directory, reused if the device rolls forward again, the same
/// posture as a secret whose first-boot write was interrupted.
pub struct MigrateV6ToV7;

impl Migration for MigrateV6ToV7 {
    fn target_version(&self) -> u32 {
        7
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(7));
        Ok(())
    }

    /// Strip v7 back out of `network`, entries and all. A document with no
    /// `network` table, or one that is not a table, is left untouched.
    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(6));
        if let Some(toml::Value::Table(network)) = doc.get_mut("network") {
            network.retain(|_, entry| {
                let kind = entry
                    .as_table()
                    .and_then(|iface| iface.get("kind"))
                    .and_then(toml::Value::as_str);
                matches!(kind, None | Some("physical"))
            });
            for (_, entry) in network.iter_mut() {
                if let toml::Value::Table(iface) = entry {
                    for key in ["kind", "vlan", "bridge", "wireguard"] {
                        iface.remove(key);
                    }
                }
            }
        }
        Ok(())
    }
}

/// v7 -> v8: adds `access.apiTokens`, the bearer API token list.
///
/// `up` stamps `schema_version = 8` and does nothing else, and that is the
/// whole migration -- [`MigrateV6ToV7`]'s shape, for the same reason. The one
/// field v8 adds is `#[serde(default, skip_serializing_if = "Vec::is_empty")]`,
/// so a v7 document and its v8 form differ by the version integer alone until
/// the first token is minted. Seeding an empty `apiTokens = []` into every
/// device's file was rejected on [`MigrateV5ToV6`]'s grounds: a written-out
/// default is indistinguishable from an operator's choice.
///
/// The bump itself is not optional, and the reason is the rollback rather than
/// the field. `AccessSettings` carries `deny_unknown_fields`, so a document
/// holding `access.apiTokens` is one a binary without this field cannot
/// deserialize at all. Only [`crate::Store`]'s tolerant load rescues it, and
/// that path runs on the strength of the on-disk `schema_version` being
/// *greater* than the reading binary's. Adding the field without the bump
/// would therefore make an A/B rollback out of a token-holding device a failed
/// settings load -- hostname, network and admin credential included -- rather
/// than a dropped key.
///
/// `down` stamps `schema_version = 7` and removes `access.apiTokens`,
/// discarding every token. That is [`MigrateV3ToV4::down`]'s trade: a v7 binary
/// has no bearer verification, so a preserved list would be a settings tree
/// advertising credentials nothing on the device will ever accept, and v7's
/// `deny_unknown_fields` would refuse the whole document rather than only the
/// key. Rolling forward again starts from an empty list, so an operator whose
/// device rolled back re-mints rather than discovering that a token they
/// believed revoked came back.
///
/// A document with no `access` table is left untouched.
pub struct MigrateV7ToV8;

impl Migration for MigrateV7ToV8 {
    fn target_version(&self) -> u32 {
        8
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(8));
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(7));
        if let Some(toml::Value::Table(access)) = doc.get_mut("access") {
            access.remove("apiTokens");
        }
        Ok(())
    }
}

/// v8 -> v9: adds the `time` subtree — NTP servers and the presentation
/// timezone.
///
/// `up` stamps `schema_version = 9` and does nothing else —
/// [`MigrateV7ToV8`]'s shape, for [`MigrateV6ToV7`]'s reason. Every field v9
/// adds is defaulted (`ntp.servers = []`, `timezone = "UTC"`), so a v8
/// document deserializes into a v9 tree unchanged, and seeding the defaults
/// into every device's file was rejected on [`MigrateV5ToV6`]'s grounds: a
/// written-out default is indistinguishable from an operator's choice the day
/// a default changes.
///
/// `down` stamps `schema_version = 8` and removes the `time` subtree,
/// discarding a configured server list and timezone. That is
/// [`MigrateV4ToV5::down`]'s trade both ways: a v8 binary has no time
/// reconciler to honour either value, and v8's `deny_unknown_fields` would
/// refuse the whole document — hostname, credential and all — if the table
/// were left behind. The device falls back to the fallback NTP pool and UTC,
/// which is exactly what every v8 image already did.
///
/// A document with no `time` table is left untouched by `down`.
pub struct MigrateV8ToV9;

impl Migration for MigrateV8ToV9 {
    fn target_version(&self) -> u32 {
        9
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(9));
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(8));
        doc.remove("time");
        Ok(())
    }
}

/// v9 -> v10: adds `provisioning.document`, the provisioning-document record —
/// the applied document's version and digest, and the last import attempt.
///
/// `up` stamps `schema_version = 10` and does nothing else —
/// [`MigrateV8ToV9`]'s shape, for [`MigrateV6ToV7`]'s reason. The one field
/// v10 adds is `#[serde(default, skip_serializing_if = "Option::is_none")]`,
/// so a v9 document and its v10 form differ by the version integer alone until
/// a document is actually offered to the device, and no dead default is seeded
/// into any device's file.
///
/// `down` stamps `schema_version = 9` and removes the record, keeping every
/// other key of `provisioning` — `state`, `deviceId` and `seededGeneration`
/// are v3 and v9 keys a v9 binary owns and must not lose. That asymmetry is
/// the whole difference from [`MigrateV8ToV9::down`], which could drop its
/// subtree whole because v9 introduced all of it.
///
/// What a rollback costs here is bounded and self-repairing, unlike the token
/// and time subtrees: the record is a MEMO about a document, not the
/// configuration the document wrote. The settings the document applied stay
/// applied. What is lost is the digest, so the v9 binary — which has no
/// importer at all — cannot short-circuit; rolling forward again finds no
/// digest and re-applies the same document, which is a no-op by construction
/// because applying it a second time produces the tree it produced the first
/// time. A re-apply is therefore the correct behaviour after a rollback and
/// not a defect of it.
///
/// A document with no `provisioning` table is left untouched by `down`.
pub struct MigrateV9ToV10;

impl Migration for MigrateV9ToV10 {
    fn target_version(&self) -> u32 {
        10
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(10));
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(9));
        if let Some(toml::Value::Table(provisioning)) = doc.get_mut("provisioning") {
            provisioning.remove("document");
        }
        Ok(())
    }
}

/// v10 -> v11: adds `access.claim`, the record of how the device left the
/// unclaimed state — the channel, the clock reading at the commit, and whether
/// the claiming credential is still the bootstrap secret it arrived as.
///
/// `up` stamps `schema_version = 11` and does nothing else —
/// [`MigrateV9ToV10`]'s shape, for [`MigrateV6ToV7`]'s reason. The one field
/// v11 adds is `#[serde(default, skip_serializing_if = "Option::is_none")]`,
/// so a v10 document and its v11 form differ by the version integer alone
/// until the device is claimed through `POST /api/v1/setup`.
///
/// `down` stamps `schema_version = 10` and removes the record, keeping every
/// other key of `access` — `webAdmin`, `ssh`, `console`, `device` and
/// `apiTokens` are keys a v10 binary owns and must not lose. That is
/// [`MigrateV9ToV10::down`]'s asymmetry for the same reason.
///
/// **What a rollback costs here is a bound, not a credential, and it fails
/// SAFE.** The claim record is what tells apid a credential has already been
/// rotated; a v10 binary has no rotation gate at all, so while it runs there is
/// nothing for the lost record to relax. Rolling forward again finds a claimed
/// device with no record, which
/// [`crate::ClaimSettings`] defines as a claim by provisioning document — so
/// the v11 binary re-asserts the rotation requirement on a credential that may
/// already have been rotated. That is one avoidable password change, demanded
/// of an operator who is already signed in, and it is the direction this
/// asymmetry has to fail in: the alternative is a bootstrap secret that a
/// rollback quietly excuses from ever being rotated.
///
/// A document with no `access` table is left untouched by `down`.
pub struct MigrateV10ToV11;

impl Migration for MigrateV10ToV11 {
    fn target_version(&self) -> u32 {
        11
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(11));
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(10));
        if let Some(toml::Value::Table(access)) = doc.get_mut("access") {
            access.remove("claim");
        }
        Ok(())
    }
}

/// v11 -> v12: adds `reset`, the staged reset intent
/// (`docs/design/recovery.md` §2.2) that one writer commits and another
/// applies.
///
/// `up` stamps `schema_version = 12` and does nothing else —
/// [`MigrateV10ToV11`]'s shape, for [`MigrateV6ToV7`]'s reason. The one field
/// v12 adds is `#[serde(default, skip_serializing_if = "Option::is_none")]`,
/// so a v11 document and its v12 form differ by the version integer alone
/// until a reset is staged.
///
/// `down` stamps `schema_version = 11` and removes the record, and removes
/// NOTHING else — [`MigrateV10ToV11::down`]'s reasoning, applied to the one
/// key v12 owns. Every other root key is a key a v11 binary owns and must not
/// lose.
///
/// **What a rollback costs here is a staged reset, and it fails SAFE.** A v11
/// binary has no reset applier at all, so a record it kept would sit unread;
/// dropping it means an operator who staged a reset, rolled the system back
/// and rolled forward again finds the reset did not happen and stages it
/// again. That is one repeated request, made by an operator who is present and
/// watching. The alternative direction — keeping a record a rollback made
/// invisible — is a device that reboots into a factory reset nobody asked for
/// at the moment it rolls forward, which is data loss caused by an update
/// decision. A reset must be caused by the operator who asked for it, so the
/// asymmetry falls this way.
pub struct MigrateV11ToV12;

impl Migration for MigrateV11ToV12 {
    fn target_version(&self) -> u32 {
        12
    }

    fn up(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(12));
        Ok(())
    }

    fn down(&self, doc: &mut toml::Table) -> Result<(), SettingsError> {
        doc.insert("schema_version".to_string(), toml::Value::Integer(11));
        doc.remove("reset");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v5 document as a fielded device carries it: a real hostname, the
    /// container switch v5 introduced, and no `mqtt` key of any kind.
    fn v5_document() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 5
hostname = "cx3576"

[network]

[container]
enabled = true
"#,
        )
        .unwrap()
    }

    /// `up` stamps the version and seeds the switch at false -- and seeds
    /// nothing else. `listen` and `auth` are absent on purpose: they are
    /// `#[serde(default)]`, so writing them here would put dead copies of the
    /// defaults into every device's settings file.
    #[test]
    fn v5_document_gains_the_mqtt_switch_at_false_and_nothing_else() {
        let mut doc = v5_document();
        MigrateV5ToV6.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(6));
        let mqtt = doc["mqtt"].as_table().unwrap();
        assert_eq!(mqtt["enabled"], toml::Value::Boolean(false));
        assert!(
            !mqtt.contains_key("listen"),
            "seeded a dead table: {mqtt:?}"
        );
        assert!(!mqtt.contains_key("auth"), "seeded a dead table: {mqtt:?}");
        assert_eq!(mqtt.len(), 1);

        // Every v5 value survives untouched.
        assert_eq!(doc["hostname"], toml::Value::String("cx3576".to_string()));
        assert_eq!(doc["container"]["enabled"], toml::Value::Boolean(true));

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV5ToV6.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// An operator's `true` is a decision, not a default to be re-applied:
    /// `up` leaves an existing boolean exactly as it found it.
    #[test]
    fn v5_to_v6_up_preserves_an_existing_switch() {
        let mut doc = v5_document();
        doc.insert(
            "mqtt".to_string(),
            toml::Value::Table(toml::toml! { enabled = true }),
        );

        MigrateV5ToV6.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(6));
        assert_eq!(doc["mqtt"]["enabled"], toml::Value::Boolean(true));
    }

    /// A hand-edited `mqtt.enabled` that is not a boolean is an error naming
    /// the key and the type it found, not a silent overwrite of what the
    /// operator typed.
    #[test]
    fn v5_to_v6_up_refuses_a_non_boolean_switch() {
        let mut doc = v5_document();
        doc.insert(
            "mqtt".to_string(),
            toml::Value::Table(toml::toml! { enabled = "yes" }),
        );

        let err = MigrateV5ToV6.up(&mut doc).unwrap_err();

        let SettingsError::Migration(message) = err else {
            panic!("expected a migration error, got {err:?}");
        };
        assert!(message.contains("mqtt.enabled"), "{message}");
        assert!(message.contains("string"), "{message}");
    }

    /// `down` discards the whole subtree, `enabled = true` included: v5 has no
    /// `MqttReconciler` to honour it, and v5's `deny_unknown_fields` would
    /// refuse the entire document if the table were left behind.
    #[test]
    fn v6_document_migrates_down_discarding_an_enabled_switch() {
        let mut doc = v5_document();
        MigrateV5ToV6.up(&mut doc).unwrap();
        doc["mqtt"]["enabled"] = toml::Value::Boolean(true);

        MigrateV5ToV6.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(5));
        assert!(!doc.contains_key("mqtt"), "{doc:?}");
        assert_eq!(doc["container"]["enabled"], toml::Value::Boolean(true));
    }

    /// `down` over a document that never carried an `mqtt` table stamps the
    /// version and touches nothing else.
    #[test]
    fn v5_to_v6_down_handles_a_document_with_no_mqtt_table() {
        let mut doc = v5_document();
        doc.insert("schema_version".to_string(), toml::Value::Integer(6));
        let before = doc.clone();

        MigrateV5ToV6.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(5));
        assert!(!doc.contains_key("mqtt"));
        for key in ["hostname", "network", "container"] {
            assert_eq!(doc[key], before[key]);
        }
    }

    /// v5 -> v6 -> v5 returns the document it started from: the seeded switch
    /// is exactly what `down` removes.
    #[test]
    fn a_v5_document_round_trips_up_to_v6_and_back() {
        let original = v5_document();
        let mut doc = original.clone();

        migrate(&mut doc, 5, 6).unwrap();
        assert_eq!(doc["schema_version"], toml::Value::Integer(6));

        migrate(&mut doc, 6, 5).unwrap();
        assert_eq!(doc, original);
    }

    /// A v6 document as a fielded device carries it: physical interfaces only,
    /// because v6 has no other kind to spell.
    fn v6_document() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 6
hostname = "cx3576"

[network.eth0]
dhcp = true

[network.eth1]
dhcp = false

[network.eth1.static]
address = "10.0.0.7/24"
gateway = "10.0.0.1"
dns = ["10.0.0.1"]

[container]
enabled = true

[mqtt]
enabled = false
"#,
        )
        .unwrap()
    }

    /// A v7 document of the shape section 2.2 of the design specifies: one
    /// physical entry and one of each new kind.
    fn v7_document_with_every_kind() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 7
hostname = "cx3576"

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
"#,
        )
        .unwrap()
    }

    /// The whole of `up`: the version integer moves and not one other byte
    /// does. This is the property the A/B rollback story rests on — a v6 tree
    /// of physical interfaces IS its own v7 form — so it is asserted over the
    /// serialized document, not field by field.
    #[test]
    fn a_v6_document_gains_the_version_stamp_and_nothing_else() {
        let original = v6_document();
        let mut doc = original.clone();

        MigrateV6ToV7.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(7));
        let before = toml::to_string(&original).unwrap();
        let after = toml::to_string(&doc).unwrap();
        assert_eq!(
            after.replace("schema_version = 7", "schema_version = 6"),
            before,
            "up must differ from its input by the version integer alone"
        );

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV6ToV7.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// `down` takes the non-physical entries with it. Keeping them as stubs
    /// would leave v6 rendering `.network` units for devices no v6 code
    /// creates; keeping their blocks would leave a document v6's
    /// `deny_unknown_fields` refuses outright.
    #[test]
    fn v7_migrates_down_dropping_every_non_physical_entry() {
        let mut doc = v7_document_with_every_kind();

        MigrateV6ToV7.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(6));
        let network = doc["network"].as_table().unwrap();
        assert_eq!(network.keys().collect::<Vec<_>>(), vec!["eth0"]);
        let eth0 = network["eth0"].as_table().unwrap();
        assert_eq!(eth0["dhcp"], toml::Value::Boolean(true));
        for key in ["kind", "vlan", "bridge", "wireguard"] {
            assert!(!eth0.contains_key(key), "{eth0:?}");
        }
        assert_eq!(doc["hostname"], toml::Value::String("cx3576".to_string()));
    }

    /// An explicit `kind = "physical"` is a v7 spelling of a v6 entry, so the
    /// entry stays and only the key goes.
    #[test]
    fn v7_down_keeps_an_explicitly_physical_entry_and_strips_its_kind() {
        let mut doc = v6_document();
        doc.insert("schema_version".to_string(), toml::Value::Integer(7));
        doc["network"]["eth0"]
            .as_table_mut()
            .unwrap()
            .insert("kind".to_string(), toml::Value::String("physical".into()));

        MigrateV6ToV7.down(&mut doc).unwrap();

        let network = doc["network"].as_table().unwrap();
        assert_eq!(network.keys().collect::<Vec<_>>(), vec!["eth0", "eth1"]);
        assert!(!network["eth0"].as_table().unwrap().contains_key("kind"));
    }

    /// v6 -> v7 -> v6 returns the document it started from, because `up` added
    /// nothing for `down` to have to guess at.
    #[test]
    fn a_v6_document_round_trips_up_to_v7_and_back() {
        let original = v6_document();
        let mut doc = original.clone();

        migrate(&mut doc, 6, 7).unwrap();
        assert_eq!(doc["schema_version"], toml::Value::Integer(7));

        migrate(&mut doc, 7, 6).unwrap();
        assert_eq!(doc, original);
    }

    /// A v7 document holding a minted token, which is the only interesting
    /// input to this step: a device with no tokens has nothing for `down` to
    /// discard and nothing for the bump to protect.
    fn v7_document_with_a_token() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 7
hostname = "cx3576"

[network]

[access.webAdmin]
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA"

[[access.apiTokens]]
id = "3f2a9c41"
name = "ci-deploy"
hash = "0000000000000000000000000000000000000000000000000000000000000001"
created = 1700000000
"#,
        )
        .unwrap()
    }

    /// `up` stamps the version and touches nothing else. The field v8 adds is
    /// defaulted and skipped when empty, so there is nothing to seed -- and
    /// seeding an empty array would put a dead default into every device's
    /// file, which is what `MigrateV5ToV6` refuses to do for `listen` and
    /// `auth`.
    #[test]
    fn v7_to_v8_up_stamps_the_version_and_seeds_nothing() {
        let mut doc = v7_document_with_a_token();
        doc.remove("access");
        let before = doc.clone();

        MigrateV7ToV8.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(8));
        assert!(!doc.contains_key("access"), "seeded a table: {doc:?}");
        let mut expected = before;
        expected.insert("schema_version".to_string(), toml::Value::Integer(8));
        assert_eq!(doc, expected);

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV7ToV8.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// `down` discards the token list, credential and all: a v7 binary has no
    /// bearer verification to honour it, and v7's `deny_unknown_fields` would
    /// refuse the whole document if the key were left behind.
    #[test]
    fn v8_document_migrates_down_discarding_every_token() {
        let mut doc = v7_document_with_a_token();
        MigrateV7ToV8.up(&mut doc).unwrap();

        MigrateV7ToV8.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(7));
        let access = doc["access"].as_table().unwrap();
        assert!(
            !access.contains_key("apiTokens"),
            "the token list survived the rollback: {access:?}"
        );
        // Everything else in `access` survives, the admin credential included.
        assert!(access.contains_key("webAdmin"));
        assert_eq!(doc["hostname"], toml::Value::String("cx3576".to_string()));
    }

    /// The round trip is not lossless, and this pins which half is lost: the
    /// version returns, the tokens do not.
    #[test]
    fn v7_to_v8_and_back_returns_the_version_but_not_the_tokens() {
        let mut doc = v7_document_with_a_token();
        MigrateV7ToV8.up(&mut doc).unwrap();
        MigrateV7ToV8.down(&mut doc).unwrap();
        MigrateV7ToV8.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(8));
        assert!(!doc["access"].as_table().unwrap().contains_key("apiTokens"));
    }

    /// A document with no `access` table at all -- a v1 tree walked forward --
    /// is left alone by `down` rather than gaining an empty one.
    #[test]
    fn v7_to_v8_down_leaves_a_document_without_access_untouched() {
        let mut doc = v7_document_with_a_token();
        doc.remove("access");
        let before = doc.clone();

        MigrateV7ToV8.down(&mut doc).unwrap();

        assert!(!doc.contains_key("access"), "{doc:?}");
        assert_eq!(doc["hostname"], before["hostname"]);
    }

    /// A v8 document holding a configured time subtree, the only interesting
    /// input to the v9 step: a device on the defaults has nothing for `down`
    /// to discard.
    fn v8_document_with_time() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 8
hostname = "cx3576"

[network]

[time]
timezone = "Europe/Berlin"

[time.ntp]
servers = ["0.pool.ntp.org", "192.0.2.7"]
"#,
        )
        .unwrap()
    }

    /// `up` stamps the version and touches nothing else: every v9 field is
    /// defaulted, so a v8 document and its v9 form differ by the version
    /// integer alone, and no dead default is seeded into any device's file.
    #[test]
    fn v8_to_v9_up_stamps_the_version_and_seeds_nothing() {
        let mut doc = v8_document_with_time();
        doc.remove("time");
        let before = doc.clone();

        MigrateV8ToV9.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(9));
        assert!(!doc.contains_key("time"), "seeded a table: {doc:?}");
        let mut expected = before;
        expected.insert("schema_version".to_string(), toml::Value::Integer(9));
        assert_eq!(doc, expected);

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV8ToV9.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// `down` discards the whole subtree, configured servers and zone
    /// included: a v8 binary has no time reconciler to honour them, and v8's
    /// `deny_unknown_fields` would refuse the entire document if the table
    /// were left behind.
    #[test]
    fn v9_document_migrates_down_discarding_the_time_subtree() {
        let mut doc = v8_document_with_time();
        MigrateV8ToV9.up(&mut doc).unwrap();

        MigrateV8ToV9.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(8));
        assert!(!doc.contains_key("time"), "{doc:?}");
        assert_eq!(doc["hostname"], toml::Value::String("cx3576".to_string()));
    }

    /// The round trip is not lossless, and this pins which half is lost: the
    /// version returns, the configured time subtree does not.
    #[test]
    fn v8_to_v9_and_back_returns_the_version_but_not_the_time_subtree() {
        let mut doc = v8_document_with_time();
        MigrateV8ToV9.up(&mut doc).unwrap();
        MigrateV8ToV9.down(&mut doc).unwrap();
        MigrateV8ToV9.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(9));
        assert!(!doc.contains_key("time"), "{doc:?}");
    }

    /// A v9 document as a provisioned device carries it: the three
    /// `provisioning` keys v3 and v9 own, and no document record of any kind.
    fn v9_document_with_provisioning() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 9
hostname = "mos-0123abcd"

[network]

[provisioning]
state = "complete"
deviceId = "0123abcd0123abcd0123abcd0123abcd"
seededGeneration = 1
"#,
        )
        .unwrap()
    }

    /// `up` stamps the version and touches nothing else: the field v10 adds is
    /// `skip_serializing_if`, so a v9 document and its v10 form differ by the
    /// version integer alone until a document is offered.
    #[test]
    fn v9_to_v10_up_stamps_the_version_and_seeds_nothing() {
        let mut doc = v9_document_with_provisioning();
        let before = doc.clone();

        MigrateV9ToV10.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(10));
        let mut expected = before;
        expected.insert("schema_version".to_string(), toml::Value::Integer(10));
        assert_eq!(doc, expected);

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV9ToV10.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// `down` drops the record and KEEPS the rest of `provisioning`. The
    /// distinction is the point: `state`, `deviceId` and `seededGeneration`
    /// are keys a v9 binary owns, and removing the table wholesale would tell
    /// that binary the device had never provisioned itself — which would
    /// re-seed a fielded device's identity.
    #[test]
    fn v10_document_migrates_down_dropping_only_the_document_record() {
        let mut doc = v9_document_with_provisioning();
        MigrateV9ToV10.up(&mut doc).unwrap();
        let provisioning = doc
            .get_mut("provisioning")
            .and_then(toml::Value::as_table_mut)
            .unwrap();
        provisioning.insert(
            "document".to_string(),
            toml::Value::Table(
                toml::from_str(
                    r#"
appliedVersion = 1
appliedDigest = "c0ffee"

[lastImport]
source = "media"
outcome = "applied"
at = 0
"#,
                )
                .unwrap(),
            ),
        );

        MigrateV9ToV10.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(9));
        let provisioning = doc["provisioning"].as_table().unwrap();
        assert!(!provisioning.contains_key("document"), "{provisioning:?}");
        assert_eq!(
            provisioning["deviceId"],
            toml::Value::String("0123abcd0123abcd0123abcd0123abcd".to_string()),
            "the identity a v9 binary owns must survive the rollback"
        );
        assert_eq!(
            provisioning["state"],
            toml::Value::String("complete".to_string())
        );
        assert_eq!(provisioning["seededGeneration"], toml::Value::Integer(1));
        assert_eq!(
            doc["hostname"],
            toml::Value::String("mos-0123abcd".to_string())
        );
    }

    /// The round trip, and which half is lost: the version returns, the
    /// document record does not. Rolling forward finds no digest and re-applies
    /// the same document, which is a no-op by construction.
    #[test]
    fn v9_to_v10_and_back_returns_the_version_but_not_the_document_record() {
        let mut doc = v9_document_with_provisioning();
        MigrateV9ToV10.up(&mut doc).unwrap();
        doc.get_mut("provisioning")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "document".to_string(),
                toml::Value::Table(toml::Table::new()),
            );
        MigrateV9ToV10.down(&mut doc).unwrap();
        MigrateV9ToV10.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(10));
        assert!(
            !doc["provisioning"]
                .as_table()
                .unwrap()
                .contains_key("document"),
            "{doc:?}"
        );
    }

    /// A document with no `provisioning` table at all — a v9 tree written
    /// before anything provisioned it — passes through `down` untouched rather
    /// than gaining an empty table.
    #[test]
    fn v9_to_v10_down_leaves_a_document_without_provisioning_untouched() {
        let mut doc: toml::Table = toml::from_str(
            "schema_version = 10
hostname = \"mos\"\n",
        )
        .unwrap();

        MigrateV9ToV10.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(9));
        assert!(!doc.contains_key("provisioning"), "{doc:?}");
    }

    /// A v10 document as a claimed device carries it: an administrator
    /// credential, a minted token, and no claim record of any kind — which is
    /// exactly the shape v11 reads as "claimed by a provisioning document".
    fn v10_document_with_a_credential() -> toml::Table {
        toml::from_str(
            r#"
schema_version = 10
hostname = "mos-0123abcd"

[network]

[access.webAdmin]
password_hash = "$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2"

[access.device]
generation = 1

[[access.apiTokens]]
id = "3f2a9c41"
name = "ci"
hash = "0000000000000000000000000000000000000000000000000000000000000001"
created = 1700000000
"#,
        )
        .unwrap()
    }

    /// `up` stamps the version and touches nothing else: the field v11 adds is
    /// `skip_serializing_if`, so a v10 document and its v11 form differ by the
    /// version integer alone until the device is claimed through the setup
    /// route.
    #[test]
    fn v10_to_v11_up_stamps_the_version_and_seeds_nothing() {
        let mut doc = v10_document_with_a_credential();
        let before = doc.clone();

        MigrateV10ToV11.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(11));
        let mut expected = before;
        expected.insert("schema_version".to_string(), toml::Value::Integer(11));
        assert_eq!(doc, expected);

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV10ToV11.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// `down` drops the claim record and KEEPS the rest of `access`. The
    /// distinction is [`MigrateV9ToV10::down`]'s: `webAdmin` and `apiTokens`
    /// are the credentials a v10 binary authenticates with, and removing the
    /// table wholesale would put a fielded device back into setup mode — an
    /// unauthenticated write route reopened by a rollback.
    #[test]
    fn v11_document_migrates_down_dropping_only_the_claim_record() {
        let mut doc = v10_document_with_a_credential();
        MigrateV10ToV11.up(&mut doc).unwrap();
        let access = doc
            .get_mut("access")
            .and_then(toml::Value::as_table_mut)
            .unwrap();
        access.insert(
            "claim".to_string(),
            toml::Value::Table(
                toml::from_str(
                    r#"
via = "setup"
at = 1700000000
rotationRequired = false
"#,
                )
                .unwrap(),
            ),
        );

        MigrateV10ToV11.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(10));
        let access = doc["access"].as_table().unwrap();
        assert!(!access.contains_key("claim"), "{access:?}");
        assert!(
            access["webAdmin"]["password_hash"].as_str().is_some(),
            "the credential a v10 binary authenticates with must survive the rollback"
        );
        assert_eq!(access["apiTokens"].as_array().unwrap().len(), 1);
        assert_eq!(access["device"]["generation"], toml::Value::Integer(1));
    }

    /// The round trip, and which half is lost: the version returns, the claim
    /// record does not. Rolling forward finds a claimed device with no record,
    /// which v11 reads as a claim by provisioning document and answers by
    /// re-asserting the rotation requirement — one avoidable password change,
    /// which is the safe direction to fail in.
    #[test]
    fn v10_to_v11_and_back_returns_the_version_but_not_the_claim_record() {
        let mut doc = v10_document_with_a_credential();
        MigrateV10ToV11.up(&mut doc).unwrap();
        doc.get_mut("access")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert("claim".to_string(), toml::Value::Table(toml::Table::new()));
        MigrateV10ToV11.down(&mut doc).unwrap();
        MigrateV10ToV11.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(11));
        assert!(
            !doc["access"].as_table().unwrap().contains_key("claim"),
            "{doc:?}"
        );
    }

    /// A document with no `access` table at all — a tree written before
    /// anything claimed it — passes through `down` untouched rather than
    /// gaining an empty table.
    #[test]
    fn v10_to_v11_down_leaves_a_document_without_access_untouched() {
        let mut doc: toml::Table = toml::from_str(
            "schema_version = 11
hostname = \"mos\"\n",
        )
        .unwrap();

        MigrateV10ToV11.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(10));
        assert!(!doc.contains_key("access"), "{doc:?}");
    }

    /// A v11 document as a claimed, fielded device carries it.
    fn v11_document() -> toml::Table {
        let mut doc = v10_document_with_a_credential();
        MigrateV10ToV11.up(&mut doc).unwrap();
        doc.get_mut("access")
            .and_then(toml::Value::as_table_mut)
            .unwrap()
            .insert(
                "claim".to_string(),
                toml::Value::Table(
                    toml::from_str(
                        r#"
via = "setup"
at = 1700000000
rotationRequired = false
"#,
                    )
                    .unwrap(),
                ),
            );
        doc
    }

    /// `up` stamps the version and touches nothing else: the field v12 adds is
    /// `skip_serializing_if`, so a v11 document and its v12 form differ by the
    /// version integer alone until a reset is staged.
    #[test]
    fn v11_to_v12_up_stamps_the_version_and_seeds_nothing() {
        let mut doc = v11_document();
        let before = doc.clone();

        MigrateV11ToV12.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(12));
        let mut expected = before;
        expected.insert("schema_version".to_string(), toml::Value::Integer(12));
        assert_eq!(doc, expected);

        // And `up` over its own output changes nothing.
        let once = doc.clone();
        MigrateV11ToV12.up(&mut doc).unwrap();
        assert_eq!(doc, once);
    }

    /// `down` drops the staged intent and KEEPS every other root key. The
    /// distinction is [`MigrateV10ToV11::down`]'s, one level up: `access` and
    /// `provisioning` are what a v11 binary authenticates and identifies the
    /// device with, and a rollback that took them would reopen an
    /// unauthenticated write route.
    #[test]
    fn v12_document_migrates_down_dropping_only_the_staged_reset() {
        let mut doc = v11_document();
        MigrateV11ToV12.up(&mut doc).unwrap();
        doc.insert(
            "reset".to_string(),
            toml::Value::Table(
                toml::from_str(
                    r#"
tier = "full-factory"
requested = 1700000000
presence = "console-attach"
"#,
                )
                .unwrap(),
            ),
        );

        MigrateV11ToV12.down(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(11));
        assert!(!doc.contains_key("reset"), "{doc:?}");
        let access = doc["access"].as_table().unwrap();
        assert!(
            access["webAdmin"]["password_hash"].as_str().is_some(),
            "the credential a v11 binary authenticates with must survive the rollback"
        );
        assert!(access.contains_key("claim"), "{access:?}");
        assert_eq!(access["apiTokens"].as_array().unwrap().len(), 1);
    }

    /// The round trip, and which half is lost: the version returns, the staged
    /// reset does not. A device that rolled back and forward again did not get
    /// the reset — which is the safe direction, because the alternative is a
    /// device that factory-resets itself the moment an update rolls forward.
    #[test]
    fn v11_to_v12_and_back_returns_the_version_but_not_the_staged_reset() {
        let mut doc = v11_document();
        MigrateV11ToV12.up(&mut doc).unwrap();
        doc.insert("reset".to_string(), toml::Value::Table(toml::Table::new()));
        MigrateV11ToV12.down(&mut doc).unwrap();
        MigrateV11ToV12.up(&mut doc).unwrap();

        assert_eq!(doc["schema_version"], toml::Value::Integer(12));
        assert!(!doc.contains_key("reset"), "{doc:?}");
    }

    /// The registry walks the whole ladder in both directions, which is what
    /// makes `Store::load` able to read any fielded document. Driven from v0
    /// so a missing step anywhere fails here rather than on a device.
    #[test]
    fn the_registry_walks_v0_to_the_current_schema_and_back() {
        let mut doc: toml::Table = toml::from_str("hostname = \"mos\"\n").unwrap();

        migrate(&mut doc, 0, crate::SCHEMA_VERSION).unwrap();
        assert_eq!(
            doc["schema_version"],
            toml::Value::Integer(i64::from(crate::SCHEMA_VERSION))
        );

        migrate(&mut doc, crate::SCHEMA_VERSION, 0).unwrap();
        assert!(!doc.contains_key("schema_version"), "{doc:?}");
    }
}
