//! Settings library for mosd.
//!
//! Provides the typed settings tree (schema v12), a Venus-style dot-path
//! get/set API, atomic TOML persistence on STATE, and a Bottlerocket-style
//! bidirectional migration framework.

#![forbid(unsafe_code)]

mod api_token;
mod audit;
mod authorized_key;
mod error;
mod migration;
mod model;
mod path;
mod recovery;
mod store;

pub use api_token::{MAX_TOKENS, is_api_token_id, validate_api_tokens};
pub use audit::{
    AUDIT_LOG, AUDIT_LOG_PREVIOUS, AUDIT_ROTATE_BYTES, DEFAULT_AUDIT_RING_DIR, append_audit_line,
    audit_line, audit_ring_dir,
};
pub use authorized_key::{
    MAX_KEYS, decode_base64, encode_base64_nopad, parse_authorized_key, validate_authorized_keys,
};
pub use error::SettingsError;
pub use migration::{
    MigrateV0ToV1, MigrateV1ToV2, MigrateV2ToV3, MigrateV3ToV4, MigrateV4ToV5, MigrateV5ToV6,
    MigrateV6ToV7, MigrateV7ToV8, MigrateV8ToV9, MigrateV9ToV10, MigrateV10ToV11, MigrateV11ToV12,
    Migration, MigrationRegistry, migrate,
};
pub use model::{
    AccessSettings, ApMode, ApiToken, AuthorizedKey, BridgeConfig, ClaimChannel, ClaimSettings,
    ConsoleSettings, ContainerSettings, DEVICE_ID_LEN, DeviceCredentialSettings, IfaceKind,
    IfaceSettings, MAX_NTP_SERVERS, MAX_PASSPHRASE_LEN, MIN_ADMIN_PASSWORD_LEN, MIN_PASSPHRASE_LEN,
    MqttAuthSettings, MqttListenSettings, MqttSettings, NtpSettings, ProvisioningDocumentSettings,
    ProvisioningImport, ProvisioningSettings, ProvisioningState, RAW_PMK_LEN, ResetSettings,
    ResetTier, SCHEMA_VERSION, Settings, SshSettings, StaticConfig, TimeSettings, VlanConfig,
    WebAdminSettings, WifiApSettings, WifiClientSettings, WifiNetwork, WifiSettings,
    WireguardConfig, WireguardPeer, is_wpa_quotable, validate_device_id, validate_ntp_servers,
    validate_timezone_name, validate_wifi_psk,
};
pub use path::{json_path_get, path_segments, quote_path_segment};
pub use recovery::{
    ACTIONS_KEY, CMDLINE_PATH_ENV, DECLARATION_PATH_ENV, DEFAULT_CMDLINE_PATH,
    DEFAULT_DECLARATION_PATH, DEFAULT_PRESENCE_MARKER_PATH, Declaration, INTENT_PARAMETER,
    INTENT_SOURCE, NoAction, PRESENCE_MARKER_PATH_ENV, PRESENCE_WINDOW_SECS, PresenceMarker,
    RECOVERY_ACTION_EVENT, REFUSED_MALFORMED_INTENT, RecoveryAction, TIER_NONE, cmdline_path,
    credential_recovery_event, declaration_path, intent_from_cmdline, presence_marker_path,
    recovery_action_event, refusal_outcome, reset_event,
};
pub use store::{DEFAULT_PATH, RollbackReport, Store};
