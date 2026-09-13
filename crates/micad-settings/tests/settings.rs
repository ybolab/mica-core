//! Integration tests for the micad-settings public API.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::json;

use micad_settings::{
    ApMode, ApiToken, AuthorizedKey, BridgeConfig, CONFIG_DOCUMENTS, CONTAINER_DOCUMENT,
    ClaimChannel, ClaimSettings, DEFAULT_CONFIG_DIR, DEFAULT_PATH, DOCUMENT_MODE, IfaceKind,
    IfaceSettings, MQTT_DOCUMENT, NETWORK_DOCUMENT, NETWORK_SCHEMA_VERSION, ProvisioningState,
    ResetSettings, ResetTier, SSH_DOCUMENT, STATE_SCHEMA_VERSION, SYSTEM_DOCUMENT, Settings,
    SettingsError, StaticConfig, Store, TIME_DOCUMENT, VlanConfig, WIFI_DOCUMENT,
    WIFI_SCHEMA_VERSION, WebAdminSettings, WifiNetwork, WireguardConfig, WireguardPeer,
    configuration, document_subtrees, encode_base64_nopad, json_path_get, parse_authorized_key,
    validate_api_tokens, validate_authorized_keys,
};

/// A store over a temporary tree.
///
/// The `/mica/config/` namespace has to EXIST: an absent document is a default,
/// but an absent NAMESPACE is the DATA medium being gone, which the store
/// refuses rather than defaults (PLAN-070 §5.2.6). Every test that reaches the
/// store therefore creates it, exactly as `mica-data-layout` does on a device.
fn store_at(dir: &tempfile::TempDir) -> Store {
    let config = dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    Store::new(dir.path().join("settings.toml"), config)
}

/// One `/mica/config/` document, parsed.
fn config_document(dir: &tempfile::TempDir, name: &str) -> serde_json::Value {
    let text = fs::read_to_string(dir.path().join("config").join(name)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn populated() -> Settings {
    let mut settings = Settings::default();
    settings.network.insert(
        "eth0".to_string(),
        IfaceSettings {
            dhcp: false,
            static_: Some(StaticConfig {
                address: "192.168.1.10/24".to_string(),
                gateway: Some("192.168.1.1".to_string()),
                dns: vec!["1.1.1.1".to_string(), "9.9.9.9".to_string()],
            }),
            ..IfaceSettings::default()
        },
    );
    settings.network.insert(
        "wlan0".to_string(),
        IfaceSettings {
            dhcp: true,
            ..IfaceSettings::default()
        },
    );
    settings
}

// --- Store -----------------------------------------------------------------

#[test]
fn save_load_roundtrip_with_network() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    let settings = populated();
    store.save(&settings).unwrap();

    // The `network` subtree is one document with a version of its own.
    let doc = config_document(&dir, "network.json");
    assert_eq!(doc["schema_version"], json!(NETWORK_SCHEMA_VERSION));
    assert_eq!(doc["network"]["wlan0"]["dhcp"], json!(true));

    assert_eq!(store.load().unwrap(), settings);
}

#[test]
fn save_is_atomic_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    store.save(&Settings::default()).unwrap();

    let updated = Settings {
        hostname: "renamed".to_string(),
        ..Settings::default()
    };
    store.save(&updated).unwrap();

    for root in [dir.path().to_path_buf(), dir.path().join("config")] {
        for entry in fs::read_dir(&root).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            assert!(
                name == "config" || name == "settings.toml" || name.ends_with(".json"),
                "a temporary file survived the save: {name}"
            );
        }
    }
    assert_eq!(store.load().unwrap(), updated);
}

#[test]
fn load_missing_file_returns_defaults_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    assert_eq!(store.load().unwrap(), Settings::default());
    assert!(!dir.path().join("settings.toml").exists());
    assert!(
        fs::read_dir(dir.path().join("config"))
            .unwrap()
            .next()
            .is_none(),
        "an absent document is a default, and reading one writes nothing"
    );
}

#[test]
fn default_path_is_the_state_location() {
    assert_eq!(DEFAULT_PATH, "/var/lib/mica/settings.toml");
    assert_eq!(DEFAULT_CONFIG_DIR, "/mica/config");
    let _store = Store::default_path();
}

// --- Dot-path get ----------------------------------------------------------

#[test]
fn get_whole_tree_scalar_and_nested() {
    let settings = populated();
    let whole = settings.get("").unwrap();
    assert_eq!(whole, settings.get(".").unwrap());
    assert_eq!(whole["hostname"], json!("mica"));

    assert_eq!(settings.get("hostname").unwrap(), json!("mica"));
    assert_eq!(
        settings.get("network.eth0.static.address").unwrap(),
        json!("192.168.1.10/24")
    );
    assert_eq!(settings.get("network.wlan0.dhcp").unwrap(), json!(true));
}

#[test]
fn get_unknown_path_is_not_found() {
    let settings = Settings::default();
    assert!(matches!(
        settings.get("network.eth9.dhcp"),
        Err(SettingsError::NotFound(_))
    ));
    assert!(matches!(
        settings.get("hostname..x"),
        Err(SettingsError::NotFound(_))
    ));
    // `schema_version` stopped being a path of the tree when PLAN-070 §5.2.3
    // put the version on each document. It is what the READ route resolves,
    // so this is the leg that makes `GET /api/v1/settings/schema_version` the
    // same 404 every other absent root gets rather than the named 409 it was.
    assert!(matches!(
        settings.get("schema_version"),
        Err(SettingsError::NotFound(_))
    ));
}

// --- Dot-path set ----------------------------------------------------------

#[test]
fn set_scalar_and_create_intermediate_entries() {
    let mut settings = Settings::default();
    settings.set("hostname", json!("edge-1")).unwrap();
    assert_eq!(settings.hostname, "edge-1");

    settings.set("network.eth0.dhcp", json!(true)).unwrap();
    assert_eq!(
        settings.network["eth0"],
        IfaceSettings {
            dhcp: true,
            ..IfaceSettings::default()
        }
    );

    settings.set("network.eth0.dhcp", json!(false)).unwrap();
    settings
        .set("network.eth0.static.address", json!("10.0.0.2/24"))
        .unwrap();
    settings
        .set("network.eth0.static.gateway", json!("10.0.0.1"))
        .unwrap();
    settings
        .set("network.eth0.static.dns", json!(["10.0.0.1"]))
        .unwrap();
    assert_eq!(
        settings.network["eth0"].static_,
        Some(StaticConfig {
            address: "10.0.0.2/24".to_string(),
            gateway: Some("10.0.0.1".to_string()),
            dns: vec!["10.0.0.1".to_string()],
        })
    );
}

#[test]
fn set_and_get_web_admin_roundtrip() {
    let mut settings = Settings::default();
    assert!(matches!(
        settings.get("access.webAdmin"),
        Err(SettingsError::NotFound(_))
    ));

    settings
        .set("access.webAdmin", json!({"password_hash": "x"}))
        .unwrap();
    assert_eq!(
        settings.access.web_admin,
        Some(WebAdminSettings {
            password_hash: "x".to_string()
        })
    );
    assert_eq!(
        settings.get("access.webAdmin.password_hash").unwrap(),
        json!("x")
    );
}

#[test]
fn set_whole_tree_replaces_settings() {
    let mut settings = Settings::default();
    let replacement = populated();
    settings
        .set(".", serde_json::to_value(&replacement).unwrap())
        .unwrap();
    assert_eq!(settings, replacement);
}

#[test]
fn set_errors_leave_state_unchanged() {
    let mut settings = populated();
    let before = settings.clone();

    assert!(matches!(
        settings.set("bogus.path", json!(1)),
        Err(SettingsError::Validation { .. })
    ));
    assert!(matches!(
        settings.set("hostname.sub", json!("x")),
        Err(SettingsError::Validation { .. })
    ));
    assert!(matches!(
        settings.set("network.eth0.dhcp", json!("yes")),
        Err(SettingsError::Validation { .. })
    ));
    assert!(matches!(
        settings.set("hostname", json!(5)),
        Err(SettingsError::Validation { .. })
    ));

    // `schema_version` is not a key of the tree any more (§5.2.3 put the
    // version on each document), so a write naming it is an unknown field
    // rather than a read-only one -- and it still writes nothing.
    let mut stamped = serde_json::to_value(&before).unwrap();
    stamped["schema_version"] = json!(1);
    assert!(matches!(
        settings.set("", stamped),
        Err(SettingsError::Validation { .. })
    ));

    assert_eq!(settings, before);
}

// --- json_path_get ---------------------------------------------------------

#[test]
fn json_path_get_navigates_a_live_state_tree() {
    let tree = json!({
        "hostname": {"current": "mica"},
        "network": {"eth0": {"operstate": "up", "addresses": ["10.0.0.2/24"]}},
    });
    assert_eq!(json_path_get(&tree, ""), Some(&tree));
    assert_eq!(json_path_get(&tree, "."), Some(&tree));
    assert_eq!(
        json_path_get(&tree, "network.eth0.operstate"),
        Some(&json!("up"))
    );
    assert_eq!(json_path_get(&tree, "network.eth1"), None);
    assert_eq!(json_path_get(&tree, "hostname.current.deeper"), None);
    assert_eq!(json_path_get(&tree, "network..eth0"), None);
}

/// Every new leaf is reachable through the dot-path API.
#[test]
fn dot_path_reaches_every_new_leaf() {
    let mut settings = Settings::default();

    let writes: Vec<(&str, serde_json::Value)> = vec![
        ("access.ssh.enabled", json!(true)),
        ("access.ssh.port", json!(2222)),
        ("access.ssh.permitRootLogin", json!(false)),
        ("access.ssh.passwordAuthentication", json!(false)),
        (
            "access.ssh.listenAddresses",
            json!(["10.0.0.7", "127.0.0.1"]),
        ),
        ("access.console.shellEnabled", json!(true)),
        ("access.device.passwordHash", json!("$argon2id$v=19$x$y$z")),
        ("access.device.generation", json!(4)),
        ("provisioning.state", json!("complete")),
        ("provisioning.deviceId", json!("a1b2c3d4e5f6")),
        ("provisioning.seededGeneration", json!(7)),
        ("wifi.client.enabled", json!(true)),
        ("wifi.client.interface", json!("wlan1")),
        (
            "wifi.client.networks",
            json!([
                {"ssid": "site-ap", "psk": "hunter2hunter2", "hidden": true, "priority": 10},
                {"ssid": "open-ap", "hidden": false, "priority": 0}
            ]),
        ),
        ("wifi.ap.mode", json!("provisioning")),
        ("wifi.ap.interface", json!("wlan1")),
        ("wifi.ap.ssid", json!("appliance-a1b2")),
        ("wifi.ap.psk", json!("provisioning-pin")),
        ("wifi.ap.channel", json!(11)),
        ("wifi.ap.countryCode", json!("CN")),
        ("wifi.ap.address", json!("10.42.0.1/24")),
        ("wifi.ap.holdDownSeconds", json!(30)),
        ("wifi.ap.graceSeconds", json!(15)),
    ];

    // Each write is read back verbatim. `psk` is left out of the second network
    // on purpose: an absent key is how an open network is spelled, and it stays
    // absent on the way out.
    for (path, value) in &writes {
        settings.set(path, value.clone()).unwrap();
        assert_eq!(
            &settings.get(path).unwrap(),
            value,
            "round trip at `{path}`"
        );
    }

    // The writes landed on the typed tree, not just on the JSON projection.
    assert_eq!(settings.access.ssh.port, 2222);
    assert!(settings.access.console.shell_enabled);
    assert_eq!(settings.access.device.generation, 4);
    assert_eq!(settings.provisioning.state, ProvisioningState::Complete);
    assert_eq!(settings.wifi.ap.mode, ApMode::Provisioning);
    assert_eq!(
        settings.wifi.client.networks,
        vec![
            WifiNetwork {
                ssid: "site-ap".to_string(),
                psk: Some("hunter2hunter2".to_string()),
                hidden: true,
                priority: 10,
            },
            WifiNetwork {
                ssid: "open-ap".to_string(),
                psk: None,
                hidden: false,
                priority: 0,
            },
        ]
    );
}

/// R3.5 rejection case: an out-of-domain enum value is refused and leaves the
/// tree untouched.
#[test]
fn set_rejects_unknown_ap_mode_and_leaves_settings_unchanged() {
    let mut settings = Settings::default();
    settings.set("wifi.ap.mode", json!("always")).unwrap();
    let before = settings.clone();

    let err = settings.set("wifi.ap.mode", json!("captive")).unwrap_err();
    assert!(
        matches!(&err, SettingsError::Validation { path, .. } if path == "wifi.ap.mode"),
        "expected a validation error at `wifi.ap.mode`, got {err:?}"
    );
    assert_eq!(settings, before);
    assert_eq!(settings.wifi.ap.mode, ApMode::Always);

    // Same for the other new enum, and for a mistyped scalar.
    assert!(matches!(
        settings.set("provisioning.state", json!("half")),
        Err(SettingsError::Validation { .. })
    ));
    assert!(matches!(
        settings.set("access.ssh.port", json!("22")),
        Err(SettingsError::Validation { .. })
    ));
    assert!(matches!(
        settings.set("wifi.client.networks", json!([{"psk": "x"}])),
        Err(SettingsError::Validation { .. })
    ));
    assert_eq!(settings, before);
}

// --- Schema v4: access.ssh.authorizedKeys ----------------------------------

/// Build a structurally valid blob for `key_type` out of the crate's own
/// encoder: the four-byte algorithm-name length, the name, then filler.
///
/// No key is pasted in from anywhere; the bytes are constructed so the test
/// depends on the format rather than on someone else's key material.
fn blob_for(key_type: &str) -> String {
    let mut bytes = Vec::new();
    let name = key_type.as_bytes();
    bytes.extend_from_slice(&u32::try_from(name.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(name);
    while bytes.len() < 64 {
        let index = u8::try_from(bytes.len()).unwrap();
        bytes.push(index.wrapping_mul(11).wrapping_add(5));
    }
    let mut encoded = encode_base64_nopad(&bytes);
    while !encoded.len().is_multiple_of(4) {
        encoded.push('=');
    }
    encoded
}

fn key_line(key_type: &str) -> String {
    format!("{key_type} {}", blob_for(key_type))
}

/// A freshly built tree carries the key, and it is empty.
#[test]
fn default_settings_serialise_an_empty_authorized_key_list_at_the_current_schema() {
    let settings = Settings::default();
    assert!(settings.access.ssh.authorized_keys.is_empty());

    let text = toml::to_string(&settings).unwrap();
    let doc: toml::Table = text.parse().unwrap();
    assert_eq!(
        doc["access"]["ssh"]["authorizedKeys"],
        toml::Value::Array(Vec::new()),
        "the key must be present and empty, not absent"
    );

    // The rest of the SSH policy is untouched by this schema step: another
    // task owns the default flip, and this one must not pre-empt it.
    assert!(!settings.access.ssh.enabled);
    assert_eq!(settings.access.ssh.port, 22);
    assert!(settings.access.ssh.permit_root_login);
    assert!(settings.access.ssh.password_authentication);
    assert!(settings.access.ssh.listen_addresses.is_empty());
}

/// The list is reachable and writable through the dot-path API, and an
/// entry with an unknown field is refused like every other typed write.
#[test]
fn dot_path_reaches_the_authorized_key_list() {
    let mut settings = Settings::default();
    assert_eq!(
        settings.get("access.ssh.authorizedKeys").unwrap(),
        json!([])
    );

    let line = key_line("ssh-ed25519");
    settings
        .set(
            "access.ssh.authorizedKeys",
            json!([{"key": line, "comment": "alice@workstation"}]),
        )
        .unwrap();
    assert_eq!(
        settings.access.ssh.authorized_keys,
        vec![AuthorizedKey {
            key: line.clone(),
            comment: Some("alice@workstation".to_string()),
        }]
    );

    // An absent comment stays absent rather than becoming an empty string.
    settings
        .set("access.ssh.authorizedKeys", json!([{"key": line}]))
        .unwrap();
    assert_eq!(settings.access.ssh.authorized_keys[0].comment, None);
    assert_eq!(
        settings.get("access.ssh.authorizedKeys").unwrap(),
        json!([{"key": line}])
    );

    let before = settings.clone();
    assert!(matches!(
        settings.set("access.ssh.authorizedKeys", json!([{"kye": line}])),
        Err(SettingsError::Validation { .. })
    ));
    assert!(matches!(
        settings.set("access.ssh.authorizedKeys", json!(["a string"])),
        Err(SettingsError::Validation { .. })
    ));
    assert_eq!(settings, before);
}

/// The typed tree is a container, not a validator. It will hold a key that
/// `validate_authorized_keys` refuses, which is exactly why the callers must
/// run the validator before rendering.
#[test]
fn the_typed_tree_holds_what_the_validator_would_refuse() {
    let mut settings = Settings::default();
    settings
        .set(
            "access.ssh.authorizedKeys",
            json!([{"key": "ssh-ed25519 not-base64"}]),
        )
        .unwrap();
    assert!(matches!(
        validate_authorized_keys(&settings.access.ssh.authorized_keys),
        Err(SettingsError::Validation { .. })
    ));
}

/// The parser is reachable from the public API and enforces its rules
/// there, so a consumer crate cannot get a weaker check by importing a
/// different symbol.
#[test]
fn the_public_parser_accepts_a_real_key_and_refuses_an_options_line() {
    let line = key_line("ssh-ed25519");
    let parsed = parse_authorized_key(&format!("{line} alice@workstation")).unwrap();
    assert_eq!(parsed.key, line);
    assert_eq!(parsed.comment.as_deref(), Some("alice@workstation"));

    for rejected in [
        format!("command=\"/bin/sh\" {line}"),
        format!("# {line}"),
        format!("{line}\nssh-rsa {}", blob_for("ssh-rsa")),
        String::new(),
    ] {
        assert!(
            parse_authorized_key(&rejected).is_err(),
            "must be rejected: {} bytes",
            rejected.len()
        );
    }

    validate_authorized_keys(std::slice::from_ref(&parsed)).unwrap();
    // The same key twice is one grant, not two.
    assert!(validate_authorized_keys(&[parsed.clone(), parsed]).is_err());
}

/// The tree the constant above describes, typed.
fn every_kind() -> Settings {
    Settings {
        hostname: "edge-1".to_string(),
        network: [
            (
                "eth0".to_string(),
                IfaceSettings {
                    dhcp: true,
                    ..IfaceSettings::default()
                },
            ),
            (
                "eth0.100".to_string(),
                IfaceSettings {
                    kind: IfaceKind::Vlan,
                    dhcp: false,
                    static_: Some(StaticConfig {
                        address: "192.168.100.2/24".to_string(),
                        gateway: None,
                        dns: Vec::new(),
                    }),
                    vlan: Some(VlanConfig {
                        parent: "eth0".to_string(),
                        id: 100,
                    }),
                    ..IfaceSettings::default()
                },
            ),
            (
                "br0".to_string(),
                IfaceSettings {
                    kind: IfaceKind::Bridge,
                    dhcp: true,
                    bridge: Some(BridgeConfig {
                        ports: vec!["eth1".to_string(), "eth2".to_string()],
                    }),
                    ..IfaceSettings::default()
                },
            ),
            (
                "wg0".to_string(),
                IfaceSettings {
                    kind: IfaceKind::Wireguard,
                    dhcp: false,
                    wireguard: Some(WireguardConfig {
                        listen_port: Some(51820),
                        peers: vec![WireguardPeer {
                            public_key: WG_PEER_PUBLIC_KEY.to_string(),
                            allowed_ips: vec!["10.8.0.0/24".to_string()],
                            endpoint: Some("vpn.example.net:51820".to_string()),
                            persistent_keepalive: Some(25),
                        }],
                    }),
                    ..IfaceSettings::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
        ..Settings::default()
    }
}

const WG_PEER_PUBLIC_KEY: &str = "AI9C8xytM2fi+RUcnV5RvMnSq4ZQffgDZ37h0vc0AU8=";

/// A tree of physical interfaces serializes exactly as v6 wrote it: no `kind`,
/// no empty blocks. This is what makes the v6 -> v7 bump additive, so it is
/// asserted on the bytes rather than inferred from the attributes.
#[test]
fn a_physical_tree_carries_no_trace_of_the_new_fields() {
    let text = toml::to_string(&populated()).unwrap();

    for key in ["kind", "vlan", "bridge", "wireguard"] {
        assert!(!text.contains(key), "{key} was written out: {text}");
    }
    // And an absent `kind` reads back as physical.
    let parsed: Settings = toml::from_str(&text).unwrap();
    assert_eq!(parsed.network["eth0"].kind, IfaceKind::Physical);
    assert_eq!(parsed, populated());
}

/// Every kind round-trips through the store: written, read back typed, and
/// spelled on disk the way the design spells it.
#[test]
fn all_three_kinds_round_trip_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    store.save(&every_kind()).unwrap();

    let settings = store.load().unwrap();
    assert_eq!(settings, every_kind());

    // Spelled on disk the way the design spells it, in the ONE document that
    // carries the subtree: the kind discriminant beside its block, and no
    // other document touched.
    let written = config_document(&dir, "network.json");
    assert_eq!(written["network"]["eth0.100"]["kind"], json!("vlan"));
    assert_eq!(
        written["network"]["eth0.100"]["vlan"],
        json!({"parent": "eth0", "id": 100})
    );
    assert_eq!(
        written["network"]["br0"]["bridge"]["ports"],
        json!(["eth1", "eth2"])
    );
    assert_eq!(
        written["network"]["wg0"]["wireguard"]["listenPort"],
        json!(51820)
    );
    assert_eq!(store.load().unwrap(), settings);
}

/// The dot-path accessor reaches into each new block, quoted segment and all.
#[test]
fn dot_paths_reach_the_new_interface_blocks() {
    let settings = every_kind();

    // A physical entry does not serialize `kind` at all, so there is no node
    // at that path: absent IS physical, and the accessor says so rather than
    // inventing a default the file does not carry.
    assert!(matches!(
        settings.get("network.eth0.kind"),
        Err(SettingsError::NotFound(_))
    ));
    assert_eq!(
        settings.get(r#"network."eth0.100".kind"#).unwrap(),
        json!("vlan")
    );
    assert_eq!(
        settings.get(r#"network."eth0.100".vlan.parent"#).unwrap(),
        json!("eth0")
    );
    assert_eq!(
        settings.get(r#"network."eth0.100".vlan.id"#).unwrap(),
        json!(100)
    );
    assert_eq!(
        settings.get("network.br0.bridge.ports").unwrap(),
        json!(["eth1", "eth2"])
    );
    assert_eq!(
        settings.get("network.wg0.wireguard.listenPort").unwrap(),
        json!(51820)
    );
    assert_eq!(
        settings.get("network.wg0.wireguard.peers").unwrap(),
        json!([{
            "publicKey": WG_PEER_PUBLIC_KEY,
            "allowedIps": ["10.8.0.0/24"],
            "endpoint": "vpn.example.net:51820",
            "persistentKeepalive": 25,
        }])
    );
}

/// A whole VLAN entry can be written through `set`, and an unknown key inside
/// a block is still refused: `deny_unknown_fields` survives the extension,
/// which is the reason the schema carries a `kind` field instead of a
/// serde-tagged enum.
#[test]
fn set_writes_a_vlan_entry_and_still_refuses_an_unknown_key() {
    let mut settings = Settings::default();
    settings
        .set(
            r#"network."eth0.100""#,
            json!({
                "kind": "vlan",
                "dhcp": false,
                "vlan": {"parent": "eth0", "id": 100},
            }),
        )
        .unwrap();
    assert_eq!(
        settings.network["eth0.100"],
        IfaceSettings {
            kind: IfaceKind::Vlan,
            dhcp: false,
            vlan: Some(VlanConfig {
                parent: "eth0".to_string(),
                id: 100,
            }),
            ..IfaceSettings::default()
        }
    );

    let before = settings.clone();
    let err = settings
        .set(r#"network."eth0.100".vlan.protocol"#, json!("802.1ad"))
        .unwrap_err();
    assert!(matches!(err, SettingsError::Validation { .. }), "{err:?}");
    assert_eq!(settings, before);

    // And a kind the schema does not name is a validation error, not a silent
    // fallback to physical.
    let err = settings
        .set(r#"network."eth0.100".kind"#, json!("macvlan"))
        .unwrap_err();
    assert!(matches!(err, SettingsError::Validation { .. }), "{err:?}");
    assert_eq!(settings, before);
}

/// **There is no private-key field in this schema.** The absence is the
/// mechanism — a field here is a value published to every bus client — so it
/// is asserted, not assumed.
#[test]
fn the_wireguard_subtree_holds_no_secret() {
    let text = toml::to_string(&every_kind()).unwrap();
    for secret in ["privateKey", "private_key", "presharedKey", "presharedkey"] {
        assert!(!text.contains(secret), "{secret} is in the tree: {text}");
    }
}

// --- Quoted path segments --------------------------------------------------

/// The reproduction as a fixture: the write of a VLAN-named entry
/// used to fail with `unknown field \`100\`` because the dot-path split the
/// key into two segments. Quoted, it lands on the key `eth0.100`, and every
/// layer — get, set of a leaf inside it, TOML persistence, reload — spells it
/// the same way.
#[test]
fn a_quoted_segment_round_trips_a_dotted_interface_key() {
    let mut settings = Settings::default();
    settings
        .set(
            r#"network."eth0.100""#,
            json!({"dhcp": false, "static": {"address": "192.168.100.2/24", "dns": []}}),
        )
        .unwrap();
    assert_eq!(
        settings.network.keys().collect::<Vec<_>>(),
        vec!["eth0.100"]
    );
    assert_eq!(
        settings
            .get(r#"network."eth0.100".static.address"#)
            .unwrap(),
        json!("192.168.100.2/24")
    );

    settings
        .set(r#"network."eth0.100".dhcp"#, json!(true))
        .unwrap();
    assert!(settings.network["eth0.100"].dhcp);

    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    store.save(&settings).unwrap();
    let doc = config_document(&dir, "network.json");
    assert_eq!(
        doc["network"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["eth0.100"],
        "persistence spells the key the way the path does: {doc}"
    );
    assert_eq!(store.load().unwrap(), settings);
}

/// The unquoted spelling still means what it always meant: two segments, so
/// `100` is looked up as a field of `IfaceSettings`.
#[test]
fn an_unquoted_dotted_key_is_still_a_field_of_the_interface() {
    let mut settings = Settings::default();
    let err = settings.set("network.eth0.100", json!({"dhcp": true}));
    assert!(
        matches!(&err, Err(SettingsError::Validation { message, .. }) if message.contains("unknown field `100`")),
        "{err:?}"
    );
    assert!(settings.network.is_empty());
    assert!(matches!(
        settings.get("network.eth0.100"),
        Err(SettingsError::NotFound(_))
    ));
}

/// Quoting is a grammar, not a string search: a segment is quoted whole or
/// not at all, and anything else is a malformed path rather than a key.
#[test]
fn malformed_quoting_is_a_path_error_on_both_sides() {
    let tree = json!({"network": {"eth0.100": {"dhcp": true}, "\"odd\"": 1}});
    assert_eq!(
        json_path_get(&tree, r#"network."eth0.100".dhcp"#),
        Some(&json!(true))
    );
    for path in [
        r#"network."eth0.100"#,   // unterminated
        r#"network."eth0.100"x"#, // trailing text after the closing quote
        r#"network.eth"0.100""#,  // a quote inside a bare segment
        r#"network."""#,          // an empty quoted segment
        r#"network.""#,           // a lone quote
    ] {
        assert_eq!(json_path_get(&tree, path), None, "{path}");
        let mut settings = Settings::default();
        assert!(
            matches!(
                settings.set(path, json!(true)),
                Err(SettingsError::NotFound(_))
            ),
            "{path}"
        );
    }
}

/// A `network` key the reconciler would refuse never reaches the tree: the
/// write is rejected, including the one key the path syntax cannot spell.
#[test]
fn set_rejects_a_network_key_that_is_not_an_interface_name() {
    let mut settings = Settings::default();
    for key in [
        "eth0 100",
        "eth0/100",
        "waytoolongiface016",
        ".",
        "..",
        "eth\"0",
    ] {
        let path = format!("network.\"{key}\"");
        let err = settings.set(&path, json!({"dhcp": true}));
        assert!(
            matches!(
                err,
                Err(SettingsError::Validation { .. }) | Err(SettingsError::NotFound(_))
            ),
            "{key}: {err:?}"
        );
    }
    assert!(settings.network.is_empty());
    settings
        .set(r#"network."eth0.100""#, json!({"dhcp": true}))
        .unwrap();
    settings
        .set("network.br-lan:0", json!({"dhcp": true}))
        .unwrap();
}

/// The rule is a property of the write, not of the tree: a key that a hand
/// edit put there before the rule existed still loads, and still does not
/// stand between an operator and an unrelated write.
#[test]
fn a_key_that_predates_the_rule_does_not_block_other_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    fs::write(
        dir.path().join("config/network.json"),
        format!(
            r#"{{"schema_version": {NETWORK_SCHEMA_VERSION}, "network": {{"eth0 100": {{"dhcp": true}}}}}}"#
        ),
    )
    .unwrap();
    let mut settings = store.load().unwrap();
    assert!(settings.network.contains_key("eth0 100"));

    settings.set("hostname", json!("edge-1")).unwrap();
    assert_eq!(settings.hostname, "edge-1");
    assert!(settings.network.contains_key("eth0 100"));

    // Touching that entry is a write, and the write is refused.
    assert!(matches!(
        settings.set(r#"network."eth0 100".dhcp"#, json!(false)),
        Err(SettingsError::Validation { .. })
    ));
}

// --- Bearer API tokens (schema v8) -----------------------------------------

/// One well-formed token, spelled the way §3.2 spells it.
fn api_token(id: &str, name: &str, digest: char) -> ApiToken {
    ApiToken {
        id: id.to_string(),
        name: name.to_string(),
        hash: digest.to_string().repeat(64),
        created: 1_700_000_000,
    }
}

/// The list round-trips through the store: written to TOML as an array of
/// tables under `access`, read back typed, and byte-stable across a re-save.
#[test]
fn the_token_list_round_trips_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let store = store_at(&dir);

    let mut settings = Settings::default();
    settings.access.api_tokens = vec![
        api_token("3f2a9c41", "ci-deploy", 'a'),
        api_token("9d4ec7b0", "backup runner", 'b'),
    ];
    validate_api_tokens(&settings.access.api_tokens).unwrap();
    store.save(&settings).unwrap();

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[[access.apiTokens]]"), "{text}");
    assert!(text.contains(r#"id = "3f2a9c41""#), "{text}");
    assert!(text.contains("created = 1700000000"), "{text}");

    let loaded = store.load().unwrap();
    assert_eq!(loaded, settings);

    // Order is the list's own and is not sorted underneath the caller: the id
    // is the identity, but the order is what a listing shows.
    assert_eq!(loaded.access.api_tokens[0].id, "3f2a9c41");
    assert_eq!(loaded.access.api_tokens[1].name, "backup runner");

    store.save(&loaded).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

/// A STATE document that carries no `apiTokens` key still loads, and loads
/// with an empty list rather than failing on the missing key. That is what
/// makes an additive bump additive (§5.2.3): two adjacent versions of one
/// document differ by the version integer alone.
#[test]
fn a_document_without_the_token_key_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    fs::write(
        dir.path().join("settings.toml"),
        format!(
            r#"schema_version = {STATE_SCHEMA_VERSION}

[access.webAdmin]
password_hash = "$argon2id$fake"
"#
        ),
    )
    .unwrap();

    let settings = store.load().unwrap();

    assert!(settings.access.api_tokens.is_empty());
    assert_eq!(
        settings.access.web_admin.unwrap().password_hash,
        "$argon2id$fake"
    );
}

/// Mint and revoke are read-modify-write of the whole array, because the
/// dot-path syntax has no array indexing. Both halves go through `set`, which
/// is the call apid makes.
#[test]
fn mint_and_revoke_are_whole_array_writes_through_the_dot_path() {
    let mut settings = Settings::default();

    let minted = vec![api_token("3f2a9c41", "ci-deploy", 'a')];
    settings
        .set("access.apiTokens", serde_json::to_value(&minted).unwrap())
        .unwrap();
    assert_eq!(settings.access.api_tokens, minted);

    let both = vec![
        api_token("3f2a9c41", "ci-deploy", 'a'),
        api_token("9d4ec7b0", "backup", 'b'),
    ];
    settings
        .set("access.apiTokens", serde_json::to_value(&both).unwrap())
        .unwrap();
    assert_eq!(settings.access.api_tokens.len(), 2);

    // Revocation is the same write with the entry removed, keyed on the id.
    let kept: Vec<ApiToken> = settings
        .access
        .api_tokens
        .iter()
        .filter(|token| token.id != "3f2a9c41")
        .cloned()
        .collect();
    settings
        .set("access.apiTokens", serde_json::to_value(&kept).unwrap())
        .unwrap();
    assert_eq!(settings.access.api_tokens.len(), 1);
    assert_eq!(settings.access.api_tokens[0].id, "9d4ec7b0");
}

/// The read-side shape apid sees: `GetSettings("access")` carries the list,
/// digests and all, which is exactly why `hash` is on the redaction denylist.
#[test]
fn the_access_subtree_carries_the_token_list_verbatim() {
    let mut settings = Settings::default();
    settings.access.api_tokens = vec![api_token("3f2a9c41", "ci-deploy", 'a')];

    let access = settings.get("access").unwrap();
    let entry = &access["apiTokens"].as_array().unwrap()[0];

    assert_eq!(entry["id"], json!("3f2a9c41"));
    assert_eq!(entry["hash"], json!("a".repeat(64)));
    assert!(entry.get("token").is_none(), "a plaintext field exists");
    assert!(entry.get("secret").is_none(), "a plaintext field exists");
}

/// A tree that carries a token still carries no plaintext anywhere: the entry
/// is a digest, a label, an id and a clock reading.
#[test]
fn a_stored_token_is_a_digest_and_nothing_else() {
    let mut settings = Settings::default();
    settings.access.api_tokens = vec![api_token("3f2a9c41", "ci-deploy", 'a')];

    let text = toml::to_string(&settings).unwrap();
    let entry: toml::Table = text.parse::<toml::Table>().unwrap()["access"]["apiTokens"]
        .as_array()
        .unwrap()[0]
        .as_table()
        .unwrap()
        .clone();
    let mut keys: Vec<&str> = entry.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["created", "hash", "id", "name"]);
}

/// The validator is reachable from outside the crate and refuses the three
/// shapes the model must not hold. The exhaustive cases live in the unit
/// tests; this asserts the export.
#[test]
fn the_token_validator_is_public_and_refuses_a_broken_list() {
    validate_api_tokens(&[]).unwrap();
    validate_api_tokens(&[api_token("3f2a9c41", "ci", 'a')]).unwrap();

    let duplicate_id = [
        api_token("3f2a9c41", "ci", 'a'),
        api_token("3f2a9c41", "cd", 'b'),
    ];
    assert!(validate_api_tokens(&duplicate_id).is_err());

    let duplicate_hash = [
        api_token("3f2a9c41", "ci", 'a'),
        api_token("9d4ec7b0", "cd", 'a'),
    ];
    assert!(validate_api_tokens(&duplicate_hash).is_err());

    let mut malformed = api_token("3f2a9c41", "ci", 'a');
    malformed.hash = "sha256:beef".to_string();
    assert!(validate_api_tokens(&[malformed]).is_err());
}

// --- The claim record (schema v11) -----------------------------------------

/// A device that has never been claimed carries no record at all, and the
/// serialized tree has no `claim` key to mistake for one.
///
/// The property §5.2.3's additive rule depends on: two adjacent versions of
/// the STATE document differ by the version integer alone until something
/// claims the device.
#[test]
fn an_unclaimed_tree_carries_no_claim_key() {
    let settings = Settings::default();
    assert_eq!(settings.access.claim, None);

    let text = toml::to_string(&settings).unwrap();
    assert!(!text.contains("claim"), "{text}");
}

/// The record round-trips through the store, and it lands under `access` — the
/// subtree apid's gate already reads, which is what lets a claim commit the
/// credential, the record and the minted token in one save.
#[test]
fn the_claim_record_round_trips_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let mut settings = Settings::default();
    settings.access.web_admin = Some(WebAdminSettings {
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2".to_string(),
    });
    settings.access.claim = Some(ClaimSettings {
        via: ClaimChannel::Setup,
        at: 1_700_000_000,
        rotation_required: false,
    });

    let store = store_at(&dir);
    store.save(&settings).unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(loaded.access.claim, settings.access.claim);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[access.claim]"), "{text}");
}

/// The wire spelling both channels serialize to, pinned: apid reads these two
/// strings out of `GetSettings("access")` and a rename here would silently
/// turn every claimed device into an unreadable record.
#[test]
fn the_claim_channels_have_kebab_case_wire_names() {
    let mut settings = Settings::default();
    for (channel, expected) in [
        (ClaimChannel::Setup, "setup"),
        (ClaimChannel::ProvisioningDocument, "provisioning-document"),
    ] {
        settings.access.claim = Some(ClaimSettings {
            via: channel,
            at: 0,
            rotation_required: true,
        });
        let access = settings.get("access").unwrap();
        assert_eq!(access["claim"]["via"], json!(expected));
        assert_eq!(access["claim"]["rotationRequired"], json!(true));
    }
}

// --- The staged reset intent (schema v12) ----------------------------------

/// A device with no reset staged carries no `reset` key at all, and the
/// serialized tree has none to mistake for one.
///
/// The property §5.2.3's additive rule depends on: two adjacent versions of
/// the STATE document differ by the version integer alone until a reset is
/// staged.
#[test]
fn a_tree_with_no_reset_staged_carries_no_reset_key() {
    let settings = Settings::default();
    assert_eq!(settings.reset, None);

    let text = toml::to_string(&settings).unwrap();
    assert!(!text.contains("reset"), "{text}");
}

/// The record round-trips through the store, so an intent committed before a
/// power loss is still there for the boot that applies it. That survival is
/// the whole mechanism `docs/design/recovery.md` §2.2 asks for.
#[test]
fn the_reset_intent_round_trips_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let settings = Settings {
        reset: Some(ResetSettings {
            tier: ResetTier::FullFactory,
            requested: 1_700_000_000,
            presence: Some("console-attach".to_string()),
        }),
        ..Settings::default()
    };

    let store = store_at(&dir);
    store.save(&settings).unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(loaded.reset, settings.reset);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[reset]"), "{text}");
}

/// The wire spelling every tier serializes to, pinned: apid writes these three
/// strings through `SetSettings("reset")` and micad reads them back, so a
/// rename here would strand an intent a device already committed.
///
/// **`secure-wipe` is not among them, and the assertion is that no spelling of
/// it parses.** Tier 4 is not implemented anywhere (`docs/design/recovery.md`
/// §2 footnote `[^wipe]`, §7), and a tier a device could accept but not keep
/// is worse than one it refuses.
#[test]
fn the_reset_tiers_have_kebab_case_wire_names_and_there_is_no_fourth() {
    let mut settings = Settings::default();
    for (tier, expected) in [
        (ResetTier::Configuration, "configuration"),
        (ResetTier::ApplicationData, "application-data"),
        (ResetTier::FullFactory, "full-factory"),
    ] {
        settings.reset = Some(ResetSettings {
            tier,
            requested: 0,
            presence: None,
        });
        let staged = settings.get("reset").unwrap();
        assert_eq!(staged["tier"], json!(expected));
        assert!(staged.get("presence").is_none(), "{staged}");
    }

    for spelling in ["secure-wipe", "secureWipe", "secure_wipe", "wipe"] {
        let err = settings
            .set("reset", json!({ "tier": spelling, "requested": 0 }))
            .unwrap_err();
        assert!(
            matches!(err, SettingsError::Validation { .. }),
            "`{spelling}` was accepted as a tier: {err:?}"
        );
    }
}

/// The intent is one dot-path write of one subtree, and clearing it is
/// another — which is what makes an apply's commit a single `Store::save`.
#[test]
fn a_reset_intent_is_staged_and_cleared_by_one_write_each() {
    let mut settings = Settings::default();

    settings
        .set(
            "reset",
            json!({ "tier": "configuration", "requested": 1_700_000_000_u64 }),
        )
        .unwrap();
    assert_eq!(
        settings.reset,
        Some(ResetSettings {
            tier: ResetTier::Configuration,
            requested: 1_700_000_000,
            presence: None,
        })
    );

    settings.set("reset", json!(null)).unwrap();
    assert_eq!(settings.reset, None);
}

/// The whole `access` subtree is writable in ONE dot-path write carrying the
/// credential, the claim record and the token list together.
///
/// This is the transaction the claim flow needs: micad turns one `SetSettings`
/// into one `Store::save`, so a claim that reaches the bus as one write cannot
/// leave a device half-claimed on a power loss.
#[test]
fn the_whole_access_subtree_is_one_write() {
    let mut settings = Settings::default();

    settings
        .set(
            "access",
            json!({
                "webAdmin": { "password_hash": "$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2" },
                "claim": { "via": "setup", "at": 7, "rotationRequired": false },
                "apiTokens": [{
                    "id": "3f2a9c41",
                    "name": "first-run setup",
                    "hash": "a".repeat(64),
                    "created": 7,
                }],
            }),
        )
        .unwrap();

    assert!(settings.access.web_admin.is_some());
    assert_eq!(
        settings.access.claim,
        Some(ClaimSettings {
            via: ClaimChannel::Setup,
            at: 7,
            rotation_required: false,
        })
    );
    assert_eq!(settings.access.api_tokens.len(), 1);
}

/// A claim record with a key the schema does not know is refused, and the
/// write leaves the tree untouched — the discipline every other subtree has.
#[test]
fn an_unknown_claim_key_is_refused_and_writes_nothing() {
    let mut settings = Settings::default();
    let before = settings.clone();

    let err = settings
        .set(
            "access.claim",
            json!({ "via": "setup", "at": 0, "rotationRequired": false, "expiresAt": 9 }),
        )
        .unwrap_err();

    assert!(matches!(err, SettingsError::Validation { .. }), "{err:?}");
    assert_eq!(settings, before);
}

// --- The documents: fail-closed, the tolerant rollback load, containment ----
//
// PLAN-070 §5.2 split the store into one document per reconciler and deleted
// the `V0→V12` chain. Two properties that still hold went down with the tests
// that carried them — the fail-closed rule for a key the schema does not know,
// and the tolerant newer-schema load that is the A/B rollback path. These
// restore both in the shape the split gave them, and add the property the
// split itself created and nothing asserted: a rollback in one document costs
// one document.

/// Write raw bytes as one `/mica/config/` document, bypassing the store.
///
/// Every fixture here is written by hand rather than produced by a `save`,
/// because what is under test is what the reader does with a document this
/// build did not write.
fn write_config(dir: &tempfile::TempDir, name: &str, text: &str) {
    fs::write(dir.path().join("config").join(name), text).unwrap();
}

/// Every document one device holds, `/mica/config/` and STATE alike.
fn document_paths(dir: &tempfile::TempDir) -> Vec<PathBuf> {
    CONFIG_DOCUMENTS
        .iter()
        .map(|name| dir.path().join("config").join(name))
        .chain([dir.path().join("settings.toml")])
        .collect()
}

/// Bytes and inode of every document, keyed by file name.
///
/// The inode is half the claim. "The others are byte-identical" is what a
/// reader cares about; "the others were not rewritten at all" is what the
/// store actually promises, and only the inode separates them.
fn fingerprints(dir: &tempfile::TempDir) -> BTreeMap<String, (Vec<u8>, u64)> {
    document_paths(dir)
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = fs::read(&path).unwrap();
            let ino = fs::metadata(&path).unwrap().ino();
            (name, (bytes, ino))
        })
        .collect()
}

fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A device with something set in every one of the eight documents, so that a
/// test about one document's loss can see the other seven survive.
fn configured() -> Settings {
    let mut settings = populated();
    settings.hostname = "edge-42".to_string();
    settings.access.console.shell_enabled = !settings.access.console.shell_enabled;
    settings.access.ssh.enabled = true;
    settings.access.web_admin = Some(WebAdminSettings {
        password_hash: "$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2".to_string(),
    });
    settings.wifi.ap.mode = ApMode::Always;
    settings.wifi.ap.ssid = Some("mica-ap".to_string());
    settings.mqtt.enabled = true;
    settings.time.timezone = "Europe/Berlin".to_string();
    settings.container.enabled = !settings.container.enabled;
    settings
}

/// **Fail-closed on a key the schema does not know, now per document.**
///
/// The rule is unchanged by the move — a document that parses but carries an
/// unknown key is refused rather than loaded with the key ignored — and the
/// move adds a requirement to it: with seven documents, `unknown field
/// `enabld`` on its own does not say which file to fix, so the refusal has to
/// name the document.
///
/// `wifi.json` is spelled out because it is the document the namespace's
/// `0600` exists for, and the key is nested inside the subtree the document
/// carries rather than beside `schema_version`: the rule is `deny_unknown_fields`
/// on the model types, which the document wrapper only inherits.
#[test]
fn a_document_with_an_unknown_key_fails_to_load_and_the_refusal_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);

    write_config(
        &dir,
        WIFI_DOCUMENT,
        &json!({
            "schema_version": WIFI_SCHEMA_VERSION,
            "wifi": { "ap": { "mode": "always", "enabld": true } },
        })
        .to_string(),
    );

    let err = store.load().unwrap_err();
    let SettingsError::Parse(message) = &err else {
        panic!("expected a parse error, got {err:?}");
    };
    assert!(
        message.contains("unknown field `enabld`"),
        "the refusal must name the offending key: {message}"
    );
    assert!(
        message.starts_with(WIFI_DOCUMENT),
        "the refusal must name the document it came from: {message}"
    );

    // The same document without the typo loads, so what was refused is the key
    // and not the fixture.
    write_config(
        &dir,
        WIFI_DOCUMENT,
        &json!({
            "schema_version": WIFI_SCHEMA_VERSION,
            "wifi": { "ap": { "mode": "always" } },
        })
        .to_string(),
    );
    assert_eq!(store.load().unwrap().wifi.ap.mode, ApMode::Always);
}

/// The reason the other six share, asserted rather than argued.
///
/// One case per document would be seven fixtures shaped by seven schemas; the
/// rule they actually share is one line of `Store` — every document is read
/// through the same `read_document`, which parses with the same
/// `deny_unknown_fields` discipline and wraps the failure with the same
/// document name. This drives all seven through it at their top level, which
/// is the part every document does have in common.
#[test]
fn every_config_document_is_fail_closed_on_an_unknown_key() {
    for document in CONFIG_DOCUMENTS {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        write_config(&dir, document, r#"{"schema_version": 1, "bogus": true}"#);

        let err = store.load().unwrap_err();
        let SettingsError::Parse(message) = &err else {
            panic!("{document}: expected a parse error, got {err:?}");
        };
        assert!(
            message.contains("unknown field `bogus`"),
            "{document}: {message}"
        );
        assert!(
            message.starts_with(document),
            "{document}: the refusal must name the document: {message}"
        );
    }
}

/// **A document that exists and does not parse is a refusal, not a default.**
///
/// The asymmetry the move rests on: an absent document is a subsystem nobody
/// configured, so it takes its schema default; a document that is there and
/// unreadable is a device whose configuration cannot be read, and rendering a
/// schema default for it would configure the device the way nobody chose.
/// Four ways to be unreadable, each of which has to name the document.
#[test]
fn a_document_that_does_not_parse_refuses_the_load_and_names_itself() {
    for (text, needle) in [
        // Not JSON at all. The wording after the document name is serde_json's
        // and is not asserted; that it is a refusal and names `mqtt.json` is.
        ("{ this is not json", None),
        // Parses, but carries no version stamp, so nothing says which schema
        // it is written against.
        (r#"{"mqtt": {"enabled": true}}"#, Some("no schema_version")),
        // A stamp that is not an integer.
        (
            r#"{"schema_version": "1", "mqtt": {}}"#,
            Some("schema_version must be an integer"),
        ),
        // A document whose top level is not a table at all.
        ("[]", Some("the top level is not a table")),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        write_config(&dir, MQTT_DOCUMENT, text);

        let err = store.load().unwrap_err();
        let SettingsError::Parse(message) = &err else {
            panic!("{text}: expected a parse error, got {err:?}");
        };
        assert!(
            message.starts_with(MQTT_DOCUMENT),
            "{text}: the refusal must name the document: {message}"
        );
        if let Some(needle) = needle {
            assert!(message.contains(needle), "{text}: {message}");
        }

        // And the contrast that makes the refusal mean something: remove the
        // file and the very same store loads, because absence IS a default.
        fs::remove_file(dir.path().join("config").join(MQTT_DOCUMENT)).unwrap();
        assert_eq!(store.load().unwrap(), Settings::default());
    }
}

/// A document from a build one schema version behind this one.
///
/// Unreachable while every document is at v1 except through the stamp `0`, and
/// worth pinning because it is the arm the deleted migration registry used to
/// serve: there is no chain any more, so the honest answer is a refusal that
/// names the document rather than a silent default.
#[test]
fn an_older_document_is_refused_by_name_because_there_is_no_migration() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    write_config(&dir, MQTT_DOCUMENT, r#"{"schema_version": 0, "mqtt": {}}"#);

    let err = store.load().unwrap_err();
    let SettingsError::SchemaVersion(message) = &err else {
        panic!("expected a schema version error, got {err:?}");
    };
    assert!(message.starts_with(MQTT_DOCUMENT), "{message}");
    assert!(message.contains("this build requires"), "{message}");
}

/// A future schema that must be refused without rewriting its fields.
fn newer_wifi_document() -> String {
    json!({
        "schema_version": WIFI_SCHEMA_VERSION + 1,
        "wifi": {
            "ap": { "mode": "always", "ssid": "mica-ap", "channel": 11, "band": "6ghz" },
            "client": {
                "enabled": true,
                "interface": "wlan0",
                "networks": [{ "ssid": "site", "psk": "hunter2hunter2", "priority": 3 }],
            },
        },
    })
    .to_string()
}

/// **A key written to one document leaves the others byte-identical** — the
/// claim the split is made of, checked on the inodes as well as on the bytes:
/// an unchanged document is not rewritten at all.
#[test]
fn a_write_to_one_document_leaves_the_others_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    let settings = configured();
    store.save(&settings).unwrap();
    let before = fingerprints(&dir);

    let changed = Settings {
        mqtt: micad_settings::MqttSettings {
            enabled: !settings.mqtt.enabled,
            ..settings.mqtt.clone()
        },
        ..settings.clone()
    };
    store.save(&changed).unwrap();

    let after = fingerprints(&dir);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>()
    );
    for (name, fingerprint) in &before {
        if name == MQTT_DOCUMENT {
            assert_ne!(
                &after[name], fingerprint,
                "the document that was written must have changed"
            );
        } else {
            assert_eq!(
                &after[name], fingerprint,
                "{name} must be byte-identical, inode included"
            );
        }
    }
    assert_eq!(store.load().unwrap(), changed);
}

/// **Every document is written `0600`, including over a laxer one.**
///
/// The namespace is credential material — `wifi.json` carries the site's WPA2
/// pre-shared key — so a document reachable under its final name at `0644` has
/// already published it.
///
/// The second half is what the mode-before-rename ordering is for: a document
/// that REPLACES a laxer one lands `0600` rather than inheriting the mode of
/// the file it replaced. Note what this does and does not pin. The ordering
/// itself is not observable from outside the process — a mode set after the
/// rename would leave the same end state — so this asserts the consequence,
/// and the ordering is `write_document`'s to keep.
#[test]
fn every_document_is_written_0600_including_over_a_laxer_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    store.save(&Settings::default()).unwrap();

    let paths = document_paths(&dir);
    assert_eq!(paths.len(), CONFIG_DOCUMENTS.len() + 1);
    for path in &paths {
        assert!(path.exists(), "{} was not written", path.display());
        assert_eq!(mode_of(path), DOCUMENT_MODE, "{}", path.display());
    }

    // Publish every one of them, then write a settings tree that changes all
    // eight — an unchanged document is deliberately not rewritten, so a
    // narrower edit would leave most of them at 0666 for a reason that is not
    // this test's subject.
    for path in &paths {
        fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(mode_of(path), 0o666);
    }
    store.save(&configured()).unwrap();

    for path in &paths {
        assert_eq!(
            mode_of(path),
            DOCUMENT_MODE,
            "{} kept the mode of the file it replaced",
            path.display()
        );
    }
}

/// **A namespace that is not there is a refusal, and the refusal names the
/// mount.** Both directions: nothing is loaded on schema defaults and nothing
/// is written.
#[test]
fn a_missing_configuration_namespace_refuses_and_names_the_mount() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("settings.toml");
    // Deliberately NOT created: this is the DATA medium being gone, not a
    // document that was never written.
    let store = Store::new(&state, dir.path().join("mica").join("config"));

    for err in [
        store.load().unwrap_err(),
        store.save(&Settings::default()).unwrap_err(),
    ] {
        let SettingsError::Unavailable { directory, mount } = &err else {
            panic!("expected an unavailable error, got {err:?}");
        };
        assert!(directory.ends_with("/mica/config"), "{directory}");
        assert!(mount.ends_with("/mica"), "{mount}");
        let message = err.to_string();
        assert!(
            message.contains(mount) && message.contains("not mounted"),
            "the refusal must name the mount an operator has to fix: {message}"
        );
    }
    assert!(!state.exists(), "a refused save must write nothing");
}

/// The two spellings of one directory agree.
///
/// `configuration::DEFAULT_UPDATES_PATH` and `DEFAULT_CONFIG_DIR` are both
/// literals, and a `const &str` cannot be built from another without a macro
/// crate — so nothing but this line makes them move together when the
/// namespace relocates. The separator is part of the assertion: without it
/// `/mica/configuration/…` would satisfy it.
#[test]
fn the_update_document_lives_inside_the_configuration_namespace() {
    assert!(
        configuration::DEFAULT_UPDATES_PATH.starts_with(&format!("{DEFAULT_CONFIG_DIR}/")),
        "{} is not inside {DEFAULT_CONFIG_DIR}",
        configuration::DEFAULT_UPDATES_PATH
    );
}

// --- F6g: the pour (PLAN-070 §5.2.7) ---------------------------------------

/// A hand-written, valid document per reconciler, with the value that proves
/// it reached the addressed tree.
///
/// **Hand-written and not `Store::save` output**, because that is the whole
/// subject: the pour is §5.2.7's one exception to machine-written-never-
/// hand-edited, and a fixture round-tripped through the writer would assert
/// that micad can read what micad wrote, which was never in doubt. Every text
/// below is what an integrator types with a partition mounted on a laptop —
/// minimal, with only the keys they care about, so `serde`'s per-field
/// defaults are exercised at the same time.
fn poured_namespace() -> Vec<Poured> {
    let poured = |document, text: String, adopted: fn(&Settings) -> bool| Poured {
        document,
        text,
        adopted,
    };
    vec![
        poured(
            SYSTEM_DOCUMENT,
            r#"{"schema_version": 1, "hostname": "poured-edge-1"}"#.to_string(),
            |settings| settings.hostname == "poured-edge-1",
        ),
        poured(
            NETWORK_DOCUMENT,
            r#"{"schema_version": 1, "network": {"eth0": {"dhcp": false}}}"#.to_string(),
            |settings| settings.network.get("eth0").is_some_and(|eth| !eth.dhcp),
        ),
        poured(
            WIFI_DOCUMENT,
            format!(
                r#"{{"schema_version": 1, "wifi": {{"ap": {{"mode": "always", "ssid": "poured-ap", "psk": "{POURED_PSK}"}}}}}}"#
            ),
            |settings| settings.wifi.ap.ssid.as_deref() == Some("poured-ap"),
        ),
        poured(
            SSH_DOCUMENT,
            r#"{"schema_version": 1, "ssh": {"enabled": true, "port": 2222}}"#.to_string(),
            |settings| settings.access.ssh.port == 2222,
        ),
        poured(
            MQTT_DOCUMENT,
            r#"{"schema_version": 1, "mqtt": {"enabled": true}}"#.to_string(),
            |settings| settings.mqtt.enabled,
        ),
        poured(
            TIME_DOCUMENT,
            r#"{"schema_version": 1, "time": {"timezone": "Europe/Berlin"}}"#.to_string(),
            |settings| settings.time.timezone == "Europe/Berlin",
        ),
        poured(
            CONTAINER_DOCUMENT,
            r#"{"schema_version": 1, "container": {"enabled": true}}"#.to_string(),
            |settings| settings.container.enabled,
        ),
    ]
}

/// One hand-written document, and the reading that proves it was adopted.
struct Poured {
    document: &'static str,
    text: String,
    adopted: fn(&Settings) -> bool,
}

/// The site key an integrator pours into `wifi.json`.
///
/// A value with no other reason to appear anywhere, so a test that greps a
/// served record for it is asserting about this key and not about a word that
/// happens to be common.
const POURED_PSK: &str = "poured-site-key-9d4ec7b0";

/// Pour the whole namespace by hand and assert every document is adopted.
///
/// **Clause 1 of the F6g gate**: a hand-written document that parses and
/// validates is adopted. Seven documents at once rather than one, because the
/// pour is a namespace operation — an integrator writes the configuration, not
/// a file — and because a per-document test would not catch a loader that read
/// the first document and stopped.
#[test]
fn a_hand_written_namespace_is_adopted_whole() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    for entry in poured_namespace() {
        write_config(&dir, entry.document, &entry.text);
    }

    let loaded = store.load_with_refusals().unwrap();

    assert!(
        loaded.refusals.is_empty(),
        "a valid pour refuses nothing: {:?}",
        loaded.refusals
    );
    for entry in poured_namespace() {
        assert!(
            (entry.adopted)(&loaded.settings),
            "{} parsed and validated and was not adopted",
            entry.document
        );
    }
    // Not written back on the way through: the pour is a read, and a loader
    // that normalised what it found would have rewritten the integrator's file
    // before anybody could look at it.
    assert_eq!(
        fs::read_to_string(dir.path().join("config").join(SYSTEM_DOCUMENT)).unwrap(),
        r#"{"schema_version": 1, "hostname": "poured-edge-1"}"#
    );
}

/// **Clause 2, both directions, for every document in the namespace.**
///
/// The refused document refuses — one refusal, naming that file and no other —
/// and its six neighbours are adopted from the same pour. Driven per document
/// rather than argued once, because the claim "a poured typo costs exactly what
/// that document configures" is a claim about each of the seven and the loader
/// is the only thing that makes it true of all of them.
///
/// The negative half is the one that would rot silently: a loader that refused
/// the whole namespace on one bad file would still pass an assertion that only
/// checked the refusal.
#[test]
fn one_unparseable_document_refuses_itself_and_nothing_else() {
    for broken in poured_namespace().iter().map(|entry| entry.document) {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        for entry in poured_namespace() {
            if entry.document == broken {
                write_config(
                    &dir,
                    entry.document,
                    r#"{"schema_version": 1, "typo": true}"#,
                );
            } else {
                write_config(&dir, entry.document, &entry.text);
            }
        }

        let loaded = store.load_with_refusals().unwrap();

        let names: Vec<&str> = loaded
            .refusals
            .iter()
            .map(|refusal| refusal.document.as_str())
            .collect();
        assert_eq!(names, vec![broken], "exactly one document is refused");

        let refusal = &loaded.refusals[0];
        // Names the file, which the gate requires in as many words: the person
        // who poured it has no other way to learn which one it was.
        assert!(
            refusal
                .message
                .contains(&refusal.path.display().to_string()),
            "the refusal must name the file: {}",
            refusal.message
        );
        assert!(refusal.path.ends_with(broken), "{:?}", refusal.path);
        assert!(
            refusal.detail.contains("unknown field `typo`"),
            "{}",
            refusal.detail
        );
        assert_eq!(refusal.subtrees, document_subtrees(broken));
        assert!(!refusal.subtrees.is_empty());

        // The other six are adopted, from the same load.
        for entry in poured_namespace() {
            if entry.document == broken {
                continue;
            }
            assert!(
                (entry.adopted)(&loaded.settings),
                "{broken} was refused and took {} down with it",
                entry.document
            );
        }
    }
}

/// **A refusal is not a schema default with a log line.**
///
/// The refused subtree does sit at its schema default in the addressed tree —
/// the tree is total and the dot-path API has no shape for a hole — so the
/// thing that has to be asserted is that the two states are still
/// distinguishable to every caller: a refused `wifi.json` and an absent one
/// produce the same subtree and a different `refusals`. A loader that returned
/// no refusal would make "the operator poured nothing" and "the operator poured
/// something this build cannot read" the same fact, which is exactly what
/// §5.2.7's *a parse error is not absence* forbids.
#[test]
fn a_refused_document_is_distinguishable_from_an_absent_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    write_config(&dir, WIFI_DOCUMENT, "{ not json");

    let refused = store.load_with_refusals().unwrap();
    assert_eq!(refused.refusals.len(), 1);
    assert_eq!(refused.settings.wifi, Settings::default().wifi);

    fs::remove_file(dir.path().join("config").join(WIFI_DOCUMENT)).unwrap();
    let absent = store.load_with_refusals().unwrap();
    assert!(absent.refusals.is_empty());

    assert_eq!(refused.settings, absent.settings);
}

/// Every way a `/mica/config/` document can fail reaches the same refusal.
///
/// The four the hard-failing loader already covers, plus the two the move
/// introduced — a version older than this build reads, and a document the
/// process cannot read at all. Named as a set because "fail closed for every
/// document" is a claim about the failure modes as much as about the files:
/// a loader that refused a parse error and let an unreadable file through
/// would fail open on the one an integrator produces with a bad `chmod`.
#[test]
fn every_way_a_document_fails_is_a_refusal_and_not_a_default() {
    for text in [
        "{ this is not json",
        r#"{"mqtt": {"enabled": true}}"#,
        r#"{"schema_version": "1", "mqtt": {}}"#,
        "[]",
        r#"{"schema_version": 0, "mqtt": {}}"#,
        r#"{"schema_version": 1, "mqtt": {"enabld": true}}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        write_config(&dir, MQTT_DOCUMENT, text);

        let loaded = store.load_with_refusals().unwrap();
        assert_eq!(
            loaded.refusals.len(),
            1,
            "{text}: expected one refusal, got {:?}",
            loaded.refusals
        );
        assert_eq!(loaded.refusals[0].document, MQTT_DOCUMENT);
        assert!(
            loaded.refusals[0]
                .message
                .contains(&loaded.refusals[0].path.display().to_string()),
            "{text}: {}",
            loaded.refusals[0].message
        );
        assert!(!loaded.settings.mqtt.enabled, "{text}: adopted anyway");
    }

    // A document the process cannot read. Skipped when the tests run as root,
    // which ignores the mode -- and that is stated rather than silently
    // passing, because a check that cannot fail is not a check.
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    let path = dir.path().join("config").join(MQTT_DOCUMENT);
    write_config(&dir, MQTT_DOCUMENT, r#"{"schema_version": 1, "mqtt": {}}"#);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_to_string(&path).is_ok() {
        eprintln!("running as root: the unreadable-document leg asserts nothing here");
    } else {
        let loaded = store.load_with_refusals().unwrap();
        assert_eq!(loaded.refusals.len(), 1);
        assert_eq!(loaded.refusals[0].document, MQTT_DOCUMENT);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// **The two rules the pour does NOT relax.**
///
/// F6f's start refusal and the STATE document's are different rules and stay
/// hard failures, which is the distinction the F6g gate turns on: "refuses its
/// subsystem" is not "refuses to start", and it is also not "nothing refuses to
/// start any more". A device that cannot reach its configuration must not
/// render a different one; a STATE document that does not parse carries the
/// device identity and the administrator credential, and degrading it would let
/// first-boot provisioning mint fresh ones over the top.
#[test]
fn the_medium_and_the_state_document_still_refuse_the_whole_load() {
    let dir = tempfile::tempdir().unwrap();
    let absent = Store::new(dir.path().join("settings.toml"), dir.path().join("config"));
    assert!(matches!(
        absent.load_with_refusals().unwrap_err(),
        SettingsError::Unavailable { .. }
    ));

    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    fs::write(dir.path().join("settings.toml"), "this is not toml = [").unwrap();
    assert!(matches!(
        store.load_with_refusals().unwrap_err(),
        SettingsError::Parse(_)
    ));
}

/// **A later save must not overwrite a refused document with the default it
/// was refused in favour of.**
///
/// The refusal's own consequence, and the one that would arrive a save later
/// than anybody was looking. The refused subtree is at its schema default in
/// the addressed tree; `save` writes every document out of that tree; so an
/// operator setting the hostname — or first-boot provisioning minting a device
/// identity, on the very boot that found the pour — would replace the
/// integrator's `wifi.json` with defaults, and the boot after that would come
/// up clean on a configuration nobody chose. Both directions: the preserved
/// document is byte identical, and the rest of the namespace was still written.
#[test]
fn a_save_leaves_a_preserved_document_exactly_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    let broken = r#"{"schema_version": 1, "wifi": {"ap": {"mode": "alwys"}}}"#;
    write_config(&dir, WIFI_DOCUMENT, broken);

    let loaded = store.load_with_refusals().unwrap();
    assert_eq!(loaded.refusals.len(), 1);
    let guarded = store.preserving(&[WIFI_DOCUMENT]);

    let mut settings = loaded.settings;
    settings.hostname = "edge-1".to_string();
    guarded.save(&settings).unwrap();

    assert_eq!(
        fs::read_to_string(dir.path().join("config").join(WIFI_DOCUMENT)).unwrap(),
        broken,
        "the refused document must not be rewritten from a schema default"
    );
    assert_eq!(
        config_document(&dir, SYSTEM_DOCUMENT)["hostname"],
        json!("edge-1"),
        "the write the operator asked for must still land"
    );

    // And the repair: the same write through a store that does NOT preserve it
    // is the authenticated edit §5.2.7 sanctions, and it replaces the bytes.
    settings.wifi.ap.ssid = Some("repaired".to_string());
    store.save(&settings).unwrap();
    assert_eq!(
        config_document(&dir, WIFI_DOCUMENT)["wifi"]["ap"]["ssid"],
        json!("repaired")
    );
    assert!(store.load_with_refusals().unwrap().refusals.is_empty());
}

/// A preserved name whose file is gone is written.
///
/// Tier 1 empties `/mica/config/` and then saves the re-seeded tree
/// (`reset.rs`). If preservation were by name alone, the refused document would
/// be the one occupant a factory reset failed to restore — and the namespace
/// would come back one document short of what the reset is defined to produce.
#[test]
fn a_preserved_document_that_no_longer_exists_is_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir).preserving(&[WIFI_DOCUMENT]);
    write_config(
        &dir,
        WIFI_DOCUMENT,
        r#"{"schema_version": 1, "typo": true}"#,
    );

    store.save(&Settings::default()).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("config").join(WIFI_DOCUMENT)).unwrap(),
        r#"{"schema_version": 1, "typo": true}"#
    );

    fs::remove_file(dir.path().join("config").join(WIFI_DOCUMENT)).unwrap();
    store.save(&Settings::default()).unwrap();
    assert_eq!(
        config_document(&dir, WIFI_DOCUMENT)["schema_version"],
        json!(WIFI_SCHEMA_VERSION)
    );
}

/// A plain store preserves nothing, asserted rather than assumed: every
/// existing caller goes through it and the pour must not have changed what they
/// do.
#[test]
fn a_store_preserves_nothing_unless_it_was_narrowed() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    write_config(
        &dir,
        WIFI_DOCUMENT,
        r#"{"schema_version": 1, "typo": true}"#,
    );

    let settings = Settings {
        hostname: "edge-1".to_string(),
        ..Settings::default()
    };
    store.save(&settings).unwrap();

    assert_eq!(
        config_document(&dir, WIFI_DOCUMENT)["schema_version"],
        json!(WIFI_SCHEMA_VERSION)
    );
    assert!(store.load_with_refusals().unwrap().refusals.is_empty());
}

/// **Every occupant of `/mica/config/` fails closed, and every refusal names its
/// file.**
///
/// The namespace has two readers and they are in different modules: `Store`
/// reads the seven settings documents, and `configuration::load_updates` reads
/// `updates.json`, which is an occupant of this directory rather than a member
/// of the settings tree. F6g's rule is the namespace's and not one reader's, so
/// it is asserted over the namespace — the alternative is a slice that hardens
/// the reader it was thinking about and leaves the other one to be found later,
/// which is exactly the shape §5.2.7 warns about.
///
/// The clause each side has to satisfy is the same in both: given bytes that do
/// not become configuration, refuse, name the file, and produce nothing a
/// caller could mistake for a value the operator chose.
#[test]
fn every_occupant_of_the_namespace_fails_closed_and_names_its_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    for document in CONFIG_DOCUMENTS {
        write_config(&dir, document, "{ not a document");
    }
    let loaded = store.load_with_refusals().unwrap();
    let mut refused: Vec<&str> = loaded
        .refusals
        .iter()
        .map(|refusal| refusal.document.as_str())
        .collect();
    refused.sort_unstable();
    let mut expected: Vec<&str> = CONFIG_DOCUMENTS.to_vec();
    expected.sort_unstable();
    assert_eq!(refused, expected, "every document refuses on its own terms");
    for refusal in &loaded.refusals {
        assert!(
            refusal
                .message
                .contains(&refusal.path.display().to_string()),
            "{}",
            refusal.message
        );
    }
    // Nothing was adopted, and nothing was invented either: the tree is the
    // schema default, which is the only total value there is — and the refusal
    // list beside it is what stops that from being read as configuration.
    assert_eq!(loaded.settings, Settings::default());

    // The eighth occupant, through the other reader. Its refusal is an `Err`
    // rather than an entry, because the update policy is not part of the
    // addressed tree; what it shares is that it names the file and that it
    // never resolves to a value (RFCT-313's `LoadedPolicy` carries the error
    // beside a policy with no selection at all).
    let updates = dir.path().join("config").join("updates.json");
    fs::write(&updates, "{ not a document").unwrap();
    let err = configuration::load_updates(&updates).unwrap_err();
    assert!(
        err.to_string().contains(&updates.display().to_string()),
        "the update document's refusal must name the file too: {err}"
    );
}

/// **A served refusal names the file and quotes nothing the document carried.**
///
/// The F6g gate's third clause reaches the *failing* case too, and this is the
/// leg that is easy to miss. A parser's sentence echoes what it choked on —
/// serde prints `invalid type: string "…", expected u8` with the value in it —
/// so a poured `wifi.json` whose site key landed in a numeric field would
/// publish that key through its own refusal: into `configuration.refused`,
/// which `GET /api/v1/state/` serves, past a redactor that keys on field names
/// and has no reason to look at one called `message`.
///
/// The split is the answer and this is what pins it: `message` is served and
/// says the file and the class, `detail` is the parser's words and goes to the
/// journal. The second assertion is the one that keeps the first honest — if
/// `detail` did not carry the key either, the fixture would not be reproducing
/// the hazard.
#[test]
fn a_refusal_names_the_file_and_quotes_nothing_the_document_carried() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    write_config(
        &dir,
        WIFI_DOCUMENT,
        &format!(r#"{{"schema_version": 1, "wifi": {{"ap": {{"channel": "{POURED_PSK}"}}}}}}"#),
    );

    let loaded = store.load_with_refusals().unwrap();
    assert_eq!(loaded.refusals.len(), 1);
    let refusal = &loaded.refusals[0];

    assert!(
        refusal
            .message
            .contains(&refusal.path.display().to_string()),
        "{}",
        refusal.message
    );
    assert!(
        refusal.message.contains("did not parse"),
        "{}",
        refusal.message
    );
    assert!(
        !refusal.message.contains(POURED_PSK),
        "the served refusal quoted the document: {}",
        refusal.message
    );
    assert!(
        refusal.detail.contains(POURED_PSK),
        "the fixture must actually reproduce the hazard, or the assertion above \
         asserts nothing: {}",
        refusal.detail
    );
}

/// The three classes a served refusal may say, each from its own trigger.
///
/// Chosen by this build rather than supplied by the parser, so the set is
/// closed: a reader can act on all three — fix the bytes, fix the version, fix
/// the permissions — and none of them can carry a byte of the document.
#[test]
fn a_served_refusal_says_which_of_three_things_went_wrong() {
    for (text, class) in [
        (
            r#"{"schema_version": 1, "mqtt": {"typo": true}}"#,
            "did not parse",
        ),
        (
            r#"{"schema_version": 0, "mqtt": {}}"#,
            "has an unsupported schema version",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        write_config(&dir, MQTT_DOCUMENT, text);
        let loaded = store.load_with_refusals().unwrap();
        assert_eq!(loaded.refusals.len(), 1, "{text}");
        assert!(
            loaded.refusals[0].message.contains(class),
            "{text}: {}",
            loaded.refusals[0].message
        );
    }
}

#[test]
fn rejects_future_settings_without_stripping_or_defaulting_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_at(&dir);
    let original = newer_wifi_document();
    write_config(&dir, WIFI_DOCUMENT, &original);
    assert!(store.load().is_err(), "future schema was silently adopted");
    let loaded = store.load_with_refusals().unwrap();
    assert_eq!(loaded.refusals.len(), 1);
    assert_eq!(loaded.refusals[0].document, WIFI_DOCUMENT);
    assert_eq!(
        fs::read_to_string(store.config_dir().join(WIFI_DOCUMENT)).unwrap(),
        original
    );
}
