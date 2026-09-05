//! The documents the settings tree is stored as (PLAN-070 §5.2).
//!
//! **Storage is split; addressing is not.** [`crate::Settings`] is the one
//! tree every reader and every dot-path still sees. Underneath it, what an
//! integrator sets lives in `/mos/config/` as one flat JSON document per
//! reconciler, and what the device mints or observes about itself — the
//! identity record, the credential material derived from it, and a staged
//! reset intent — stays on STATE. [`crate::Store`] is the only thing that
//! converts between the two shapes.
//!
//! **The boundary is measured, not judged** (§5.2.1): it is the tier-1 reset
//! partition. Tier 1 returns what an integrator set, so the set it clears IS
//! the set that lives in `/mos/config/`, and `reset.rs` is the second reader
//! of the same line.
//!
//! **One version per document, each starting at v1** (§5.2.3). A
//! namespace-wide version is refused because a bump would rewrite every
//! document and there is no transaction across the renames, so a power loss
//! would leave a store caught mid-migration — a third state. Per document,
//! each is either old or new. The rules that come with that:
//!
//! - a bump is **additive**, and `skip_serializing_if` keeps a new optional
//!   table out of a document that does not use it, so two adjacent versions of
//!   one document differ by the version integer alone;
//! - every migration has a `down` as well as an `up`, and the `down` states
//!   what it discards;
//! - the reason for both is **A/B rollback survivability**: the system slot
//!   can go backwards and the configuration on DATA does not, so an older
//!   binary must be able to read a newer document. [`crate::Store`]'s tolerant
//!   load is what carries that at runtime;
//! - **no migration may move a key from one document to another**, because
//!   that is the migration with no transaction. A key that has to move is a
//!   new key in the destination and a deprecation in the source.
//!
//! The `V0→V12` chain that used to migrate the single `settings.toml` is not
//! ported: it is deleted with the document it migrated. This tree is in system
//! development and carries no compatibility obligation, so a split migration
//! would be code written to convert a document that does not exist. The four
//! rules above are the extract of the twelve arguments that chain carried, and
//! they are the part that was ever load-bearing.

use std::collections::BTreeMap;

use crate::model::{
    AccessSettings, ApiToken, ClaimSettings, ConsoleSettings, ContainerSettings,
    DeviceCredentialSettings, IfaceSettings, MqttSettings, ProvisioningSettings, ResetSettings,
    Settings, SshSettings, TimeSettings, WebAdminSettings, WifiSettings,
};

/// Directory the `/mos/config/` documents live in when nothing relocates it.
pub const DEFAULT_CONFIG_DIR: &str = "/mos/config";

/// Mode `mos-data-layout` establishes on `/mos/config/` (§5.2.4).
///
/// Stated here as well as in the layout script because the charter and the
/// mode have to agree in both readers: the namespace is credential material —
/// `wifi.json` carries the site's WPA2 pre-shared key — and `0755` would
/// publish it to every process on the device.
pub const CONFIG_DIR_MODE: u32 = 0o700;

/// Mode every document is written at, set before the rename (§5.2.4).
pub const DOCUMENT_MODE: u32 = 0o600;

/// The namespace's own index: the documents this schema writes, in the order
/// §5.2.2's table names them.
///
/// `updates.json` is not here. The update policy document is PLAN-071's, and
/// it is an occupant of this directory rather than a member of the settings
/// tree.
pub const CONFIG_DOCUMENTS: [&str; 7] = [
    SYSTEM_DOCUMENT,
    NETWORK_DOCUMENT,
    WIFI_DOCUMENT,
    SSH_DOCUMENT,
    MQTT_DOCUMENT,
    TIME_DOCUMENT,
    CONTAINER_DOCUMENT,
];

/// `hostname` and `access.console`; the hostname reconciler.
pub const SYSTEM_DOCUMENT: &str = "system.json";
/// The `network` subtree; the network reconciler.
pub const NETWORK_DOCUMENT: &str = "network.json";
/// The whole `wifi` subtree; the `wifiAp` and `wifiClient` reconcilers.
///
/// Both, in one document, because `wifiAp` declares the whole `wifi` subtree
/// rather than `wifi.ap` — a narrowing that was tried, was measured to leave a
/// cross-subtree dependency invisible to the overlap test, and was reverted
/// with the reason recorded at the declaration. Grouping by reconciler keeps
/// that coupling inside one atomic write instead of splitting it across two
/// files with no transaction between them.
pub const WIFI_DOCUMENT: &str = "wifi.json";
/// `access.ssh`; the sshd reconciler.
pub const SSH_DOCUMENT: &str = "ssh.json";
/// The `mqtt` subtree; the mqtt reconciler.
pub const MQTT_DOCUMENT: &str = "mqtt.json";
/// The `time` subtree; the time reconciler.
pub const TIME_DOCUMENT: &str = "time.json";
/// The `container` subtree; the container reconciler.
pub const CONTAINER_DOCUMENT: &str = "container.json";

/// Schema version of `system.json`.
pub const SYSTEM_SCHEMA_VERSION: u32 = 1;
/// Schema version of `network.json`.
pub const NETWORK_SCHEMA_VERSION: u32 = 1;
/// Schema version of `wifi.json`.
pub const WIFI_SCHEMA_VERSION: u32 = 1;
/// Schema version of `ssh.json`.
pub const SSH_SCHEMA_VERSION: u32 = 1;
/// Schema version of `mqtt.json`.
pub const MQTT_SCHEMA_VERSION: u32 = 1;
/// Schema version of `time.json`.
pub const TIME_SCHEMA_VERSION: u32 = 1;
/// Schema version of `container.json`.
pub const CONTAINER_SCHEMA_VERSION: u32 = 1;
/// Schema version of the STATE document.
///
/// The remainder is a document too and takes the same rule: it is not exempt
/// from the per-document version for being what is left over.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// `system.json`: the hostname and the local console policy.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// System hostname; `hostname` in the addressed tree.
    pub hostname: String,
    /// `access.console` in the addressed tree.
    #[serde(default)]
    pub console: ConsoleSettings,
}

/// `network.json`: the per-interface network configuration.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `network` in the addressed tree, keyed by interface name.
    #[serde(default)]
    pub network: BTreeMap<String, IfaceSettings>,
}

/// `wifi.json`: the station and access-point settings, and the site's WPA2
/// key.
///
/// This is the document the namespace's `0600` exists for.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WifiDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `wifi` in the addressed tree.
    #[serde(default)]
    pub wifi: WifiSettings,
}

/// `ssh.json`: the SSH channel policy.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `access.ssh` in the addressed tree.
    #[serde(default)]
    pub ssh: SshSettings,
}

/// `mqtt.json`: the broker and bridge policy.
///
/// Policy only: the broker reads its accounts from a STATE file rather than
/// from `mqtt.auth`, which is the rule already stated at the field — the tree
/// carries the policy, the STATE file carries the secret.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `mqtt` in the addressed tree.
    #[serde(default)]
    pub mqtt: MqttSettings,
}

/// `time.json`: the managed NTP servers and the presentation timezone.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `time` in the addressed tree.
    #[serde(default)]
    pub time: TimeSettings,
}

/// `container.json`: the container engine policy.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `container` in the addressed tree.
    #[serde(default)]
    pub container: ContainerSettings,
}

/// The half of `access` that stays on STATE.
///
/// The device credential and the management credential are minted or observed
/// by the device, not set by an integrator, and a staged claim is a record of
/// what happened rather than a setting. `access.ssh` and `access.console` are
/// on the other side of the boundary, which is why a read of the `access`
/// subtree composes two stores.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StateAccessSettings {
    /// Web admin credentials; absent until apid sets them.
    #[serde(rename = "webAdmin", skip_serializing_if = "Option::is_none")]
    pub web_admin: Option<WebAdminSettings>,
    /// How this device was claimed; absent until it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim: Option<ClaimSettings>,
    /// Device credential metadata; never holds a plaintext secret.
    pub device: DeviceCredentialSettings,
    /// Bearer API tokens, hashes only.
    ///
    /// Declared last so the TOML serializer emits this array of tables after
    /// every other key of `access`.
    #[serde(rename = "apiTokens", skip_serializing_if = "Vec::is_empty")]
    pub api_tokens: Vec<ApiToken>,
}

/// The STATE document: what the device mints or observes about itself, the
/// credential material derived from it, and the intents it is carrying out.
///
/// Still TOML at `/var/lib/mos/settings.toml`. The JSON rule is the
/// `/mos/config/` namespace's, and this document is not in it.
///
/// **The staged reset intent is here and not in `/mos/config/`, and that is
/// not a technicality.** Tiers 1 and 3 clear the configuration namespace; put
/// the record that asks for a reset inside it and the tier would clear the
/// thing that tells it to run, halfway through running.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateDocument {
    /// Version of this document alone.
    pub schema_version: u32,
    /// `provisioning` in the addressed tree.
    #[serde(default)]
    pub provisioning: ProvisioningSettings,
    /// The half of `access` that is not configuration.
    #[serde(default)]
    pub access: StateAccessSettings,
    /// A staged reset intent; absent unless one is waiting to be applied.
    ///
    /// Declared last so the TOML serializer emits this table after every other
    /// key of the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<ResetSettings>,
}

/// Every document one device holds, as one value.
///
/// The composition and the split are both total and both live here, so a field
/// that is added to [`Settings`] and forgotten in one direction does not
/// compile.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DocumentSet {
    pub system: SystemDocument,
    pub network: NetworkDocument,
    pub wifi: WifiDocument,
    pub ssh: SshDocument,
    pub mqtt: MqttDocument,
    pub time: TimeDocument,
    pub container: ContainerDocument,
    pub state: StateDocument,
}

impl DocumentSet {
    /// Split `settings` into the documents that store it.
    pub(crate) fn of(settings: &Settings) -> Self {
        Self {
            system: SystemDocument {
                schema_version: SYSTEM_SCHEMA_VERSION,
                hostname: settings.hostname.clone(),
                console: settings.access.console.clone(),
            },
            network: NetworkDocument {
                schema_version: NETWORK_SCHEMA_VERSION,
                network: settings.network.clone(),
            },
            wifi: WifiDocument {
                schema_version: WIFI_SCHEMA_VERSION,
                wifi: settings.wifi.clone(),
            },
            ssh: SshDocument {
                schema_version: SSH_SCHEMA_VERSION,
                ssh: settings.access.ssh.clone(),
            },
            mqtt: MqttDocument {
                schema_version: MQTT_SCHEMA_VERSION,
                mqtt: settings.mqtt.clone(),
            },
            time: TimeDocument {
                schema_version: TIME_SCHEMA_VERSION,
                time: settings.time.clone(),
            },
            container: ContainerDocument {
                schema_version: CONTAINER_SCHEMA_VERSION,
                container: settings.container.clone(),
            },
            state: StateDocument {
                schema_version: STATE_SCHEMA_VERSION,
                provisioning: settings.provisioning.clone(),
                access: StateAccessSettings {
                    web_admin: settings.access.web_admin.clone(),
                    claim: settings.access.claim,
                    device: settings.access.device.clone(),
                    api_tokens: settings.access.api_tokens.clone(),
                },
                reset: settings.reset.clone(),
            },
        }
    }

    /// Compose the documents back into the one addressed tree.
    pub(crate) fn compose(self) -> Settings {
        Settings {
            hostname: self.system.hostname,
            network: self.network.network,
            access: AccessSettings {
                web_admin: self.state.access.web_admin,
                claim: self.state.access.claim,
                ssh: self.ssh.ssh,
                console: self.system.console,
                device: self.state.access.device,
                api_tokens: self.state.access.api_tokens,
            },
            provisioning: self.state.provisioning,
            wifi: self.wifi.wifi,
            container: self.container.container,
            mqtt: self.mqtt.mqtt,
            time: self.time.time,
            reset: self.state.reset,
        }
    }
}

/// The document set a device with no stored document at all has.
///
/// This is the only place the "a missing document is its schema default"
/// rule is expressed, and it is expressed by splitting [`Settings::default`]
/// rather than by restating any default — so a subtree added to the schema
/// arrives here with the value its own type declares.
impl Default for DocumentSet {
    fn default() -> Self {
        Self::of(&Settings::default())
    }
}

macro_rules! document_default {
    ($document:ty, $field:ident) => {
        impl Default for $document {
            fn default() -> Self {
                DocumentSet::default().$field
            }
        }
    };
}

document_default!(SystemDocument, system);
document_default!(NetworkDocument, network);
document_default!(WifiDocument, wifi);
document_default!(SshDocument, ssh);
document_default!(MqttDocument, mqtt);
document_default!(TimeDocument, time);
document_default!(ContainerDocument, container);
document_default!(StateDocument, state);
