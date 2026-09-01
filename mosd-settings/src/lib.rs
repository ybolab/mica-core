//! Settings library for mosd.
//!
//! Provides the typed settings tree (schema v8), a Venus-style dot-path
//! get/set API, atomic TOML persistence on STATE, and a Bottlerocket-style
//! bidirectional migration framework.

#![forbid(unsafe_code)]

mod api_token;
mod authorized_key;
mod error;
mod migration;
mod model;
mod path;
mod store;

pub use api_token::{MAX_TOKENS, is_api_token_id, validate_api_tokens};
pub use authorized_key::{
    MAX_KEYS, decode_base64, encode_base64_nopad, parse_authorized_key, validate_authorized_keys,
};
pub use error::SettingsError;
pub use migration::{
    MigrateV0ToV1, MigrateV1ToV2, MigrateV2ToV3, MigrateV3ToV4, MigrateV4ToV5, MigrateV5ToV6,
    MigrateV6ToV7, MigrateV7ToV8, Migration, MigrationRegistry, migrate,
};
pub use model::{
    AccessSettings, ApMode, ApiToken, AuthorizedKey, BridgeConfig, ConsoleSettings,
    ContainerSettings, DeviceCredentialSettings, IfaceKind, IfaceSettings, MAX_PASSPHRASE_LEN,
    MIN_PASSPHRASE_LEN, MqttAuthSettings, MqttListenSettings, MqttSettings, ProvisioningSettings,
    ProvisioningState, RAW_PMK_LEN, SCHEMA_VERSION, Settings, SshSettings, StaticConfig,
    VlanConfig, WebAdminSettings, WifiApSettings, WifiClientSettings, WifiNetwork, WifiSettings,
    WireguardConfig, WireguardPeer, is_wpa_quotable, validate_wifi_psk,
};
pub use path::{json_path_get, path_segments, quote_path_segment};
pub use store::{DEFAULT_PATH, RollbackReport, Store};
