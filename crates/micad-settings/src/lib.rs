//! Settings library for micad.
//!
//! Provides the typed settings tree, a Venus-style dot-path get/set API, and
//! the split persistence PLAN-070 §5.2 decided: system configuration as one
//! JSON document per reconciler under `/mos/config/` on DATA, and what the
//! device mints or observes about itself as one TOML document on STATE. Each
//! document carries its own schema version.

#![forbid(unsafe_code)]

mod api_token;
mod audit;
mod authorized_key;
// A public module rather than a re-export: `/mos/config/` is a namespace with
// several documents and two processes reading it, so callers name the
// namespace (PLAN-070 §5.2). It carries the update policy document and the
// baked layer it overrides (§5.1).
pub mod configuration;
// The settings store's own occupants of that namespace, plus the STATE
// remainder (§5.2). Private, because the store is the only thing that reads
// or writes them: `configuration`'s documents have two processes and these
// have one.
mod documents;
mod error;
mod model;
mod path;
mod recovery;
mod store;
mod transaction;

pub use api_token::{MAX_TOKENS, is_api_token_id, validate_api_tokens};
pub use audit::{
    ACTOR_DEVICE, ACTOR_OPERATOR, ACTOR_POLICY, AUDIT_LOG, AUDIT_LOG_PREVIOUS, AUDIT_ROTATE_BYTES,
    DEFAULT_AUDIT_RING_DIR, REQUESTED, UPDATE_CHECK_EVENT, UPDATE_CONFIG_EVENT, UPDATE_FETCH_EVENT,
    UPDATE_INSTALL_EVENT, append_audit_line, audit_line, audit_ring_dir,
};
pub use authorized_key::{
    MAX_KEYS, decode_base64, encode_base64_nopad, parse_authorized_key, validate_authorized_keys,
};
pub use documents::{
    CONFIG_DIR_MODE, CONFIG_DOCUMENTS, CONTAINER_DOCUMENT, CONTAINER_SCHEMA_VERSION,
    ContainerDocument, DEFAULT_CONFIG_DIR, DOCUMENT_MODE, DOCUMENT_SUBTREES, MQTT_DOCUMENT,
    MQTT_SCHEMA_VERSION, MqttDocument, NETWORK_DOCUMENT, NETWORK_SCHEMA_VERSION, NetworkDocument,
    SSH_DOCUMENT, SSH_SCHEMA_VERSION, STATE_SCHEMA_VERSION, SYSTEM_DOCUMENT, SYSTEM_SCHEMA_VERSION,
    SshDocument, StateAccessSettings, StateDocument, SystemDocument, TIME_DOCUMENT,
    TIME_SCHEMA_VERSION, TimeDocument, WIFI_DOCUMENT, WIFI_SCHEMA_VERSION, WifiDocument,
    document_subtrees,
};
pub use error::SettingsError;
pub use model::{
    AccessSettings, ApMode, ApiToken, AuthorizedKey, BridgeConfig, ClaimChannel, ClaimSettings,
    ConsoleSettings, ContainerSettings, DEVICE_ID_LEN, DeviceCredentialSettings, IfaceKind,
    IfaceSettings, MAX_NTP_SERVERS, MAX_PASSPHRASE_LEN, MIN_ADMIN_PASSWORD_LEN, MIN_PASSPHRASE_LEN,
    MqttAuthSettings, MqttListenSettings, MqttSettings, NtpSettings, ProvisioningDocumentSettings,
    ProvisioningImport, ProvisioningSettings, ProvisioningState, RAW_PMK_LEN, ResetSettings,
    ResetTier, Settings, SshSettings, StaticConfig, TimeSettings, VlanConfig, WebAdminSettings,
    WifiApSettings, WifiClientSettings, WifiNetwork, WifiSettings, WireguardConfig, WireguardPeer,
    is_wpa_quotable, validate_device_id, validate_ntp_servers, validate_timezone_name,
    validate_wifi_psk,
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
pub use store::{DEFAULT_PATH, DocumentRefusal, LoadedStore, Store};
