//! Integration tests for the mosd-settings public API.

use std::fs;

use serde_json::json;

use mosd_settings::{
    ApMode, ApiToken, AuthorizedKey, BridgeConfig, ClaimChannel, ClaimSettings, DEFAULT_CONFIG_DIR,
    DEFAULT_PATH, IfaceKind, IfaceSettings, NETWORK_SCHEMA_VERSION, ProvisioningState,
    ResetSettings, ResetTier, STATE_SCHEMA_VERSION, Settings, SettingsError, StaticConfig, Store,
    VlanConfig, WebAdminSettings, WifiNetwork, WireguardConfig, WireguardPeer, encode_base64_nopad,
    json_path_get, parse_authorized_key, validate_api_tokens, validate_authorized_keys,
};

/// A store over a temporary tree.
///
/// The `/mos/config/` namespace has to EXIST: an absent document is a default,
/// but an absent NAMESPACE is the DATA medium being gone, which the store
/// refuses rather than defaults (PLAN-070 §5.2.6). Every test that reaches the
/// store therefore creates it, exactly as `mos-data-layout` does on a device.
fn store_at(dir: &tempfile::TempDir) -> Store {
    let config = dir.path().join("config");
    fs::create_dir_all(&config).unwrap();
    Store::new(dir.path().join("settings.toml"), config)
}

/// One `/mos/config/` document, parsed.
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
        fs::read_dir(dir.path().join("config")).unwrap().next().is_none(),
        "an absent document is a default, and reading one writes nothing"
    );
}

#[test]
fn default_path_is_the_state_location() {
    assert_eq!(DEFAULT_PATH, "/var/lib/mos/settings.toml");
    assert_eq!(DEFAULT_CONFIG_DIR, "/mos/config");
    let _store = Store::default_path();
}

// --- Dot-path get ----------------------------------------------------------

#[test]
fn get_whole_tree_scalar_and_nested() {
    let settings = populated();
    let whole = settings.get("").unwrap();
    assert_eq!(whole, settings.get(".").unwrap());
    assert_eq!(whole["hostname"], json!("mos"));

    assert_eq!(settings.get("hostname").unwrap(), json!("mos"));
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
        "hostname": {"current": "mos"},
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

    let (settings, reports) = store.load_with_report().unwrap();
    assert!(
        reports.is_empty(),
        "a document at this schema version is not a rollback"
    );
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
        doc["network"].as_object().unwrap().keys().collect::<Vec<_>>(),
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

    let (loaded, reports) = store.load_with_report().unwrap();
    assert!(reports.is_empty());
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

    let (settings, reports) = store.load_with_report().unwrap();

    assert!(reports.is_empty(), "a document at this version is not a rollback");
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
/// strings through `SetSettings("reset")` and mosd reads them back, so a
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
/// This is the transaction the claim flow needs: mosd turns one `SetSettings`
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
