//! Integration tests for the mosd-settings public API.

use std::fs;

use serde_json::json;

use mosd_settings::{
    AccessSettings, ApMode, ApiToken, AuthorizedKey, BridgeConfig, ClaimChannel, ClaimSettings,
    ConsoleSettings, ContainerSettings, DEFAULT_PATH, DeviceCredentialSettings, IfaceKind,
    IfaceSettings, MigrateV0ToV1, MigrateV3ToV4, Migration, MigrationRegistry, MqttAuthSettings,
    MqttListenSettings, MqttSettings, NtpSettings, ProvisioningSettings, ProvisioningState,
    ResetSettings, ResetTier, SCHEMA_VERSION, Settings, SettingsError, SshSettings, StaticConfig,
    Store, TimeSettings, VlanConfig, WebAdminSettings, WifiApSettings, WifiClientSettings,
    WifiNetwork, WifiSettings, WireguardConfig, WireguardPeer, encode_base64_nopad, json_path_get,
    migrate, parse_authorized_key, validate_api_tokens, validate_authorized_keys,
};

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
    let store = Store::new(dir.path().join("settings.toml"));
    let settings = populated();
    store.save(&settings).unwrap();

    let text = fs::read_to_string(dir.path().join("settings.toml")).unwrap();
    let doc: toml::Table = text.parse().unwrap();
    assert_eq!(
        doc.get("schema_version"),
        Some(&toml::Value::Integer(i64::from(SCHEMA_VERSION)))
    );

    assert_eq!(store.load().unwrap(), settings);
}

#[test]
fn save_is_atomic_and_leaves_no_temp_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("settings.toml"));
    store.save(&Settings::default()).unwrap();

    let updated = Settings {
        hostname: "renamed".to_string(),
        ..Settings::default()
    };
    store.save(&updated).unwrap();

    let entries: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("settings.toml")]);
    assert_eq!(store.load().unwrap(), updated);
}

#[test]
fn load_missing_file_returns_defaults_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    let store = Store::new(&path);
    assert_eq!(store.load().unwrap(), Settings::default());
    assert!(!path.exists());
}

#[test]
fn default_path_is_the_state_location() {
    assert_eq!(DEFAULT_PATH, "/var/lib/mos/settings.toml");
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
        settings.set("schema_version", json!(3)),
        Err(SettingsError::ReadOnly(_))
    ));
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

    let mut wrong_version = serde_json::to_value(&before).unwrap();
    wrong_version["schema_version"] = json!(1);
    assert!(matches!(
        settings.set("", wrong_version),
        Err(SettingsError::ReadOnly(_))
    ));

    assert_eq!(settings, before);
}

// --- Migrations ------------------------------------------------------------

#[test]
fn v0_document_migrates_up_and_back_down() {
    let mut doc: toml::Table = "hostname = \"legacy\"".parse().unwrap();
    let original = doc.clone();

    migrate(&mut doc, 0, 1).unwrap();
    assert_eq!(doc.get("schema_version"), Some(&toml::Value::Integer(1)));
    assert_eq!(
        doc.get("hostname"),
        Some(&toml::Value::String("legacy".to_string()))
    );
    assert!(doc.contains_key("network"));

    migrate(&mut doc, 1, 0).unwrap();
    assert_eq!(doc, original);
}

#[test]
fn v1_document_migrates_up_to_v2() {
    let mut doc: toml::Table = "schema_version = 1\nhostname = \"legacy\"\n\n[network]\n"
        .parse()
        .unwrap();

    migrate(&mut doc, 1, 2).unwrap();
    let text = toml::to_string(&doc).unwrap();
    let settings: Settings = toml::from_str(&text).unwrap();
    // This step stops at v2; only `Store::load` walks all the way to v3.
    assert_eq!(settings.schema_version, 2);
    assert_eq!(settings.hostname, "legacy");
    assert!(settings.network.is_empty());
    assert!(settings.access.web_admin.is_none());
    assert_eq!(
        doc.get("access"),
        Some(&toml::Value::Table(toml::Table::new()))
    );
}

#[test]
fn v2_document_migrates_down_to_v1_dropping_access() {
    let mut doc: toml::Table = concat!(
        "schema_version = 2\n",
        "hostname = \"mos\"\n\n",
        "[network]\n\n",
        "[access.webAdmin]\n",
        "password_hash = \"$argon2id$v=19$m=19456,t=2,p=1$abc$def\"\n",
    )
    .parse()
    .unwrap();

    migrate(&mut doc, 2, 1).unwrap();
    assert_eq!(doc.get("schema_version"), Some(&toml::Value::Integer(1)));
    assert!(!doc.contains_key("access"));
}

#[test]
fn store_load_migrates_v1_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(
        &path,
        "schema_version = 1\nhostname = \"legacy\"\n\n[network]\n",
    )
    .unwrap();

    let settings = Store::new(&path).load().unwrap();
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert_eq!(settings.hostname, "legacy");
    assert!(settings.access.web_admin.is_none());
}

#[test]
fn store_load_migrates_v0_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, "hostname = \"legacy\"\n").unwrap();

    let settings = Store::new(&path).load().unwrap();
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert_eq!(settings.hostname, "legacy");
    assert!(settings.network.is_empty());
}

#[test]
fn migrate_errors_on_missing_step() {
    let mut doc = toml::Table::new();
    // SCHEMA_VERSION + 1, not a literal. This asserted `migrate(0, 5)` errors
    // for want of a fifth step -- and the container switch ADDED that step, so
    // the literal now names a version the registry can reach and the test was
    // asserting the opposite of its name. Deriving the target means the next
    // schema addition cannot quietly turn this green for the wrong reason.
    assert!(matches!(
        migrate(&mut doc, 0, SCHEMA_VERSION + 1),
        Err(SettingsError::Migration(_))
    ));
}

#[test]
fn custom_registry_applies_migrations() {
    let registry = MigrationRegistry::new(vec![Box::new(MigrateV0ToV1)]);
    let mut doc: toml::Table = "hostname = \"legacy\"".parse().unwrap();
    registry.migrate(&mut doc, 0, 1).unwrap();
    assert_eq!(MigrateV0ToV1.target_version(), 1);
    assert_eq!(doc.get("schema_version"), Some(&toml::Value::Integer(1)));
    assert!(doc.contains_key("network"));
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

// --- Schema v3: access / provisioning / wifi --------------------------------

/// A realistic v2 document: non-default hostname, a static-addressed
/// interface, and an apid-written admin hash.
const V2_DOCUMENT: &str = concat!(
    "schema_version = 2\n",
    "hostname = \"edge-42\"\n\n",
    "[network.eth0]\n",
    "dhcp = false\n\n",
    "[network.eth0.static]\n",
    "address = \"10.0.0.7/24\"\n",
    "gateway = \"10.0.0.1\"\n",
    "dns = [\"10.0.0.1\", \"1.1.1.1\"]\n\n",
    "[access.webAdmin]\n",
    "password_hash = \"$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aGFzaGhhc2g\"\n",
);

const V2_PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHQ$aGFzaGhhc2g";

/// A v3 tree carrying a value in every subtree, used by the round-trip tests.
fn v3_populated() -> Settings {
    Settings {
        // A v3 tree, deliberately not SCHEMA_VERSION: this fixture is the
        // input to the v3 rollback tests, not a current document.
        schema_version: 3,
        hostname: "edge-42".to_string(),
        network: [(
            "eth0".to_string(),
            IfaceSettings {
                dhcp: false,
                static_: Some(StaticConfig {
                    address: "10.0.0.7/24".to_string(),
                    gateway: Some("10.0.0.1".to_string()),
                    dns: vec!["10.0.0.1".to_string(), "1.1.1.1".to_string()],
                }),
                ..IfaceSettings::default()
            },
        )]
        .into_iter()
        .collect(),
        access: AccessSettings {
            web_admin: Some(WebAdminSettings {
                password_hash: V2_PASSWORD_HASH.to_string(),
            }),
            // No claim record: this fixture is a v3 tree, and a v3 device that
            // carries a credential and no record is exactly what `ClaimSettings`
            // reads as a claim by provisioning document.
            claim: None,
            ssh: SshSettings {
                enabled: true,
                port: 2222,
                permit_root_login: false,
                password_authentication: false,
                listen_addresses: vec!["10.0.0.7".to_string()],
                authorized_keys: Vec::new(),
            },
            console: ConsoleSettings {
                shell_enabled: true,
            },
            device: DeviceCredentialSettings {
                password_hash: Some("$argon2id$v=19$m=19456,t=2,p=1$ZGV2$ZGV2aGFzaA".to_string()),
                generation: 4,
            },
            api_tokens: Vec::new(),
        },
        provisioning: ProvisioningSettings {
            state: ProvisioningState::Complete,
            device_id: Some("a1b2c3d4e5f6".to_string()),
            seeded_generation: 7,
            document: None,
        },
        wifi: WifiSettings {
            client: WifiClientSettings {
                enabled: true,
                interface: "wlan1".to_string(),
                networks: vec![WifiNetwork {
                    ssid: "site-ap".to_string(),
                    psk: Some("hunter2hunter2".to_string()),
                    hidden: true,
                    priority: 10,
                }],
            },
            ap: WifiApSettings {
                mode: ApMode::Always,
                interface: "wlan1".to_string(),
                ssid: Some("appliance-a1b2".to_string()),
                psk: Some("provisioning-pin".to_string()),
                channel: 11,
                country_code: "CN".to_string(),
                address: "10.42.0.1/24".to_string(),
                hold_down_seconds: 30,
                grace_seconds: 15,
            },
        },
        // Populated with the NON-default value on purpose. This fixture feeds
        // the v3 rollback tests, and MigrateV4ToV5::down discards the whole
        // `container` table; a fixture carrying `false` would round-trip
        // identically whether or not the migration dropped anything.
        container: ContainerSettings { enabled: true },
        // Non-default for the same reason `container` is: MigrateV5ToV6::down
        // discards the whole `mqtt` table, and a fixture holding the defaults
        // would round-trip identically whether or not it was dropped.
        mqtt: MqttSettings {
            enabled: true,
            listen: MqttListenSettings {
                address: "0.0.0.0".to_string(),
                port: 8883,
            },
            auth: MqttAuthSettings { enabled: true },
        },
        // Non-default for the reason `container` and `mqtt` are:
        // MigrateV8ToV9::down discards the whole `time` table, and a fixture
        // holding the defaults would round-trip identically either way.
        time: TimeSettings {
            ntp: NtpSettings {
                servers: vec!["0.pool.ntp.org".to_string()],
            },
            timezone: "Europe/Berlin".to_string(),
        },
        // No staged reset: this fixture is a v3 tree, and `MigrateV11ToV12::down`
        // drops the record on the way to one, so a populated value here would
        // assert nothing the round trip could keep.
        reset: None,
    }
}

/// A real v2 document keeps every v2 value across the upgrade and gains the
/// v3 subtrees at their documented defaults.
#[test]
fn real_v2_document_survives_the_upgrade_to_v3() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, V2_DOCUMENT).unwrap();

    let settings = Store::new(&path).load().unwrap();

    // Everything v2 could express is byte-identical to what went in.
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert_eq!(settings.hostname, "edge-42");
    assert_eq!(
        settings.network["eth0"],
        IfaceSettings {
            dhcp: false,
            static_: Some(StaticConfig {
                address: "10.0.0.7/24".to_string(),
                gateway: Some("10.0.0.1".to_string()),
                dns: vec!["10.0.0.1".to_string(), "1.1.1.1".to_string()],
            }),
            ..IfaceSettings::default()
        }
    );
    assert_eq!(settings.network.len(), 1);
    assert_eq!(
        settings.access.web_admin,
        Some(WebAdminSettings {
            password_hash: V2_PASSWORD_HASH.to_string(),
        })
    );

    // The v3 subtrees arrive at their documented defaults.
    assert_eq!(settings.access.ssh, SshSettings::default());
    assert!(!settings.access.ssh.enabled);
    assert_eq!(settings.access.ssh.port, 22);
    assert!(settings.access.ssh.permit_root_login);
    assert!(settings.access.ssh.password_authentication);
    assert!(settings.access.ssh.listen_addresses.is_empty());
    assert!(settings.access.ssh.authorized_keys.is_empty());
    assert!(!settings.access.console.shell_enabled);
    assert_eq!(settings.access.device.password_hash, None);
    assert_eq!(settings.access.device.generation, 0);
    assert_eq!(settings.provisioning.state, ProvisioningState::Pending);
    assert_eq!(settings.provisioning.device_id, None);
    assert_eq!(settings.provisioning.seeded_generation, 0);
    assert!(!settings.wifi.client.enabled);
    assert_eq!(settings.wifi.client.interface, "wlan0");
    assert!(settings.wifi.client.networks.is_empty());
    assert_eq!(settings.wifi.ap.mode, ApMode::Off);
    assert_eq!(settings.wifi.ap.interface, "wlan0");
    assert_eq!(settings.wifi.ap.ssid, None);
    assert_eq!(settings.wifi.ap.psk, None);
    assert_eq!(settings.wifi.ap.channel, 6);
    assert_eq!(settings.wifi.ap.country_code, "US");
    assert_eq!(settings.wifi.ap.address, "192.168.4.1/24");
    assert_eq!(settings.wifi.ap.hold_down_seconds, 120);
    assert_eq!(settings.wifi.ap.grace_seconds, 60);
}

/// A v3 -> v2 -> v3 round trip keeps every v2-representable value and resets
/// the v3-only ones to their defaults.
#[test]
fn v3_document_round_trips_down_to_v2_and_back() {
    let original = v3_populated();
    let mut doc: toml::Table = toml::to_string(&original).unwrap().parse().unwrap();

    migrate(&mut doc, 3, 2).unwrap();
    assert_eq!(doc.get("schema_version"), Some(&toml::Value::Integer(2)));
    migrate(&mut doc, 2, 3).unwrap();

    let text = toml::to_string(&doc).unwrap();
    let restored: Settings = toml::from_str(&text).unwrap();

    // v2-representable values are unchanged.
    assert_eq!(restored.schema_version, original.schema_version);
    assert_eq!(restored.hostname, original.hostname);
    assert_eq!(restored.network, original.network);
    assert_eq!(restored.access.web_admin, original.access.web_admin);

    // v3-only values are back at their defaults, not at the pre-rollback ones.
    assert_eq!(restored.access.ssh, SshSettings::default());
    assert_eq!(restored.access.console, ConsoleSettings::default());
    assert_eq!(restored.access.device, DeviceCredentialSettings::default());
    assert_eq!(restored.provisioning, ProvisioningSettings::default());
    assert_eq!(restored.wifi, WifiSettings::default());
    assert_ne!(restored, original);
}

/// Rolling back to v2 removes exactly the v3-only keys and keeps
/// `access.webAdmin`.
#[test]
fn v3_document_migrates_down_to_v2_dropping_only_v3_keys() {
    let mut doc: toml::Table = toml::to_string(&v3_populated()).unwrap().parse().unwrap();
    assert!(doc.contains_key("provisioning"));
    assert!(doc.contains_key("wifi"));

    migrate(&mut doc, 3, 2).unwrap();

    assert_eq!(doc.get("schema_version"), Some(&toml::Value::Integer(2)));
    assert!(!doc.contains_key("provisioning"));
    assert!(!doc.contains_key("wifi"));

    let access = doc["access"].as_table().unwrap();
    assert!(!access.contains_key("ssh"));
    assert!(!access.contains_key("console"));
    assert!(!access.contains_key("device"));
    assert_eq!(
        access["webAdmin"]["password_hash"].as_str(),
        Some(V2_PASSWORD_HASH)
    );
    assert_eq!(
        doc.get("hostname"),
        Some(&toml::Value::String("edge-42".to_string()))
    );

    // The result is a document v2 software can actually deserialize: no
    // v3-only key is left behind for `deny_unknown_fields` to trip over.
    let text = toml::to_string(&doc).unwrap();
    assert!(!text.contains("provisioning"));
    assert!(!text.contains("wifi"));
    assert!(!text.contains("shellEnabled"));
}

/// `deny_unknown_fields` still rejects a typo inside a new subtree.
#[test]
fn v3_document_with_unknown_key_fails_to_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(
        &path,
        concat!(
            "schema_version = 3\n",
            "hostname = \"edge-42\"\n\n",
            "[network]\n\n",
            "[access.ssh]\n",
            "enabld = true\n",
        ),
    )
    .unwrap();

    let err = Store::new(&path).load().unwrap_err();
    let SettingsError::Parse(message) = &err else {
        panic!("expected a parse error, got {err:?}");
    };
    assert!(
        message.contains("unknown field `enabld`"),
        "error should name the offending key, got: {message}"
    );

    // The same document without the typo loads.
    fs::write(
        &path,
        concat!(
            "schema_version = 3\n",
            "hostname = \"edge-42\"\n\n",
            "[network]\n\n",
            "[access.ssh]\n",
            "enabled = true\n",
        ),
    )
    .unwrap();
    assert!(Store::new(&path).load().unwrap().access.ssh.enabled);
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

/// The three-step registry still walks a v0 and a v1 document all the way
/// to a valid v3 tree.
#[test]
fn v0_and_v1_documents_walk_all_the_way_to_v3() {
    let dir = tempfile::tempdir().unwrap();

    let v0 = dir.path().join("v0.toml");
    fs::write(&v0, "hostname = \"legacy\"\n").unwrap();
    let from_v0 = Store::new(&v0).load().unwrap();

    let v1 = dir.path().join("v1.toml");
    fs::write(
        &v1,
        "schema_version = 1\nhostname = \"legacy\"\n\n[network]\n",
    )
    .unwrap();
    let from_v1 = Store::new(&v1).load().unwrap();

    for settings in [&from_v0, &from_v1] {
        assert_eq!(settings.schema_version, SCHEMA_VERSION);
        assert_eq!(settings.hostname, "legacy");
        assert!(settings.network.is_empty());
        assert_eq!(settings.access, AccessSettings::default());
        assert_eq!(settings.provisioning, ProvisioningSettings::default());
        assert_eq!(settings.wifi, WifiSettings::default());
    }
    assert_eq!(from_v0, from_v1);

    // And the walked tree is a tree the store can write back and re-read.
    let out = dir.path().join("out.toml");
    let store = Store::new(&out);
    store.save(&from_v0).unwrap();
    assert_eq!(store.load().unwrap(), from_v0);
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

/// A v3 document carrying an `access.ssh` table, which is the shape the
/// upgrade meets on a device already carrying SSH settings.
const V3_DOCUMENT: &str = concat!(
    "schema_version = 3\n",
    "hostname = \"edge-42\"\n\n",
    "[network]\n\n",
    "[access.ssh]\n",
    "enabled = true\n",
    "port = 2222\n",
    "permitRootLogin = false\n",
    "passwordAuthentication = false\n",
    "listenAddresses = [\"10.0.0.7\"]\n",
);

/// A freshly built tree carries the key, and it is empty.
#[test]
fn default_settings_serialise_an_empty_authorized_key_list_at_the_current_schema() {
    let settings = Settings::default();
    assert!(settings.access.ssh.authorized_keys.is_empty());

    let text = toml::to_string(&settings).unwrap();
    let doc: toml::Table = text.parse().unwrap();
    assert_eq!(
        doc["schema_version"],
        toml::Value::Integer(i64::from(SCHEMA_VERSION))
    );
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

/// A v4 document with real keys survives a save/load round trip, and the
/// comment lives in its own field on disk.
#[test]
fn a_v4_document_round_trips_through_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().join("settings.toml"));

    let mut settings = Settings::default();
    settings.access.ssh.authorized_keys = vec![
        AuthorizedKey {
            key: key_line("ssh-ed25519"),
            comment: Some("alice@workstation".to_string()),
        },
        AuthorizedKey {
            key: key_line("ssh-rsa"),
            comment: None,
        },
    ];
    validate_authorized_keys(&settings.access.ssh.authorized_keys).unwrap();
    store.save(&settings).unwrap();

    let text = fs::read_to_string(dir.path().join("settings.toml")).unwrap();
    assert!(
        text.contains("alice@workstation"),
        "the comment is persisted: {text}"
    );
    assert!(
        text.lines()
            .all(|line| !(line.contains("ssh-ed25519") && line.contains("alice"))),
        "the comment must live on its own key, not inside the key line: {text}"
    );

    let loaded = store.load().unwrap();
    assert_eq!(loaded, settings);
    assert_eq!(loaded.access.ssh.authorized_keys.len(), 2);
    assert_eq!(
        loaded.access.ssh.authorized_keys[0].comment.as_deref(),
        Some("alice@workstation")
    );
    assert_eq!(loaded.access.ssh.authorized_keys[1].comment, None);
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

/// A v3 document without the key gains an empty array, and `up` is
/// idempotent over the result.
#[test]
fn v3_document_gains_an_empty_authorized_key_list_and_up_is_idempotent() {
    let mut doc: toml::Table = V3_DOCUMENT.parse().unwrap();
    assert!(
        !doc["access"]["ssh"]
            .as_table()
            .unwrap()
            .contains_key("authorizedKeys")
    );

    migrate(&mut doc, 3, 4).unwrap();
    assert_eq!(doc["schema_version"], toml::Value::Integer(4));
    assert_eq!(
        doc["access"]["ssh"]["authorizedKeys"],
        toml::Value::Array(Vec::new())
    );

    // Re-running `up` over the migrated document changes nothing at all.
    let once = doc.clone();
    MigrateV3ToV4.up(&mut doc).unwrap();
    assert_eq!(doc, once);

    // And an existing non-empty list is left exactly as it was.
    let key = toml::Value::Table(
        [(
            "key".to_string(),
            toml::Value::String(key_line("ssh-ed25519")),
        )]
        .into_iter()
        .collect(),
    );
    doc["access"]["ssh"]["authorizedKeys"] = toml::Value::Array(vec![key.clone()]);
    let populated = doc.clone();
    MigrateV3ToV4.up(&mut doc).unwrap();
    assert_eq!(doc, populated);
    assert_eq!(doc["access"]["ssh"]["authorizedKeys"][0], key);
}

/// `up` over a non-array value is an error naming the path and the type, not
/// a silent overwrite of whatever the operator hand-edited in.
#[test]
fn v3_to_v4_up_refuses_a_non_array_authorized_key_value() {
    for (literal, type_name) in [
        ("\"a string\"", "string"),
        ("42", "integer"),
        ("true", "boolean"),
        ("{ a = 1 }", "table"),
    ] {
        let text = format!("{V3_DOCUMENT}authorizedKeys = {literal}\n");
        let mut doc: toml::Table = text.parse().unwrap();
        let err = migrate(&mut doc, 3, 4).unwrap_err();
        let SettingsError::Migration(message) = &err else {
            panic!("expected a migration error, got {err:?}");
        };
        assert!(
            message.contains("access.ssh.authorizedKeys"),
            "message must name the path: {message}"
        );
        assert!(
            message.contains(type_name),
            "message must name the type found ({type_name}): {message}"
        );
    }
}

/// `down` removes the key, and a v3 -> v4 -> v3 round trip returns the
/// document it started from.
#[test]
fn v4_document_migrates_down_to_v3_and_round_trips() {
    let original: toml::Table = V3_DOCUMENT.parse().unwrap();
    let mut doc = original.clone();

    migrate(&mut doc, 3, 4).unwrap();
    migrate(&mut doc, 4, 3).unwrap();
    assert_eq!(doc, original, "v3 -> v4 -> v3 must be the identity");

    // And a non-empty list is discarded, deliberately: a v3 image has no
    // renderer for it, and `deny_unknown_fields` would refuse the document.
    let mut doc: toml::Table = V3_DOCUMENT.parse().unwrap();
    migrate(&mut doc, 3, 4).unwrap();
    doc["access"]["ssh"]["authorizedKeys"] = toml::Value::Array(vec![toml::Value::Table(
        [(
            "key".to_string(),
            toml::Value::String(key_line("ssh-ed25519")),
        )]
        .into_iter()
        .collect(),
    )]);
    migrate(&mut doc, 4, 3).unwrap();
    assert_eq!(doc["schema_version"], toml::Value::Integer(3));
    assert!(
        !doc["access"]["ssh"]
            .as_table()
            .unwrap()
            .contains_key("authorizedKeys")
    );
    let text = toml::to_string(&doc).unwrap();
    assert!(!text.contains("authorizedKeys"), "{text}");
    assert!(!text.contains("ssh-ed25519"), "{text}");
    assert_eq!(doc, original);
}

/// Both directions cope with `access` or `access.ssh` being absent.
#[test]
fn v3_to_v4_handles_a_document_with_no_access_table() {
    // `up` creates the intermediate tables.
    let mut doc: toml::Table = "schema_version = 3\nhostname = \"bare\"\n\n[network]\n"
        .parse()
        .unwrap();
    migrate(&mut doc, 3, 4).unwrap();
    assert_eq!(
        doc["access"]["ssh"]["authorizedKeys"],
        toml::Value::Array(Vec::new())
    );

    // `down` is a no-op when `access` is absent...
    let mut doc: toml::Table = "schema_version = 4\nhostname = \"bare\"\n\n[network]\n"
        .parse()
        .unwrap();
    MigrateV3ToV4.down(&mut doc).unwrap();
    assert_eq!(doc["schema_version"], toml::Value::Integer(3));
    assert!(!doc.contains_key("access"));

    // ...and when `access` exists but `access.ssh` does not.
    let mut doc: toml::Table = concat!(
        "schema_version = 4\n",
        "hostname = \"bare\"\n\n",
        "[network]\n\n",
        "[access.webAdmin]\n",
        "password_hash = \"x\"\n",
    )
    .parse()
    .unwrap();
    let before = doc.clone();
    MigrateV3ToV4.down(&mut doc).unwrap();
    assert_eq!(doc["schema_version"], toml::Value::Integer(3));
    assert_eq!(doc["access"], before["access"]);
}

/// The whole chain still walks, in both directions, with the new step on the
/// end.
#[test]
fn the_full_chain_walks_from_v0_to_the_current_schema_and_back_to_v0() {
    let mut doc: toml::Table = "hostname = \"legacy\"".parse().unwrap();
    let original = doc.clone();

    migrate(&mut doc, 0, SCHEMA_VERSION).unwrap();
    assert_eq!(
        doc["schema_version"],
        toml::Value::Integer(i64::from(SCHEMA_VERSION))
    );
    assert_eq!(
        doc["access"]["ssh"]["authorizedKeys"],
        toml::Value::Array(Vec::new())
    );
    // The walked document deserializes into a current tree.
    let settings: Settings = toml::from_str(&toml::to_string(&doc).unwrap()).unwrap();
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert_eq!(settings.hostname, "legacy");
    assert_eq!(settings.access.ssh, SshSettings::default());

    migrate(&mut doc, SCHEMA_VERSION, 0).unwrap();
    assert_eq!(doc, original, "the walk down must undo the walk up");
}

/// `Store::load` migrates a real v3 file on disk all the way to v4.
#[test]
fn store_load_migrates_a_v3_file_to_v4() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, V3_DOCUMENT).unwrap();

    let settings = Store::new(&path).load().unwrap();
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert!(settings.access.ssh.authorized_keys.is_empty());
    // Every v3 value survives.
    assert!(settings.access.ssh.enabled);
    assert_eq!(settings.access.ssh.port, 2222);
    assert!(!settings.access.ssh.permit_root_login);
    assert!(!settings.access.ssh.password_authentication);
    assert_eq!(settings.access.ssh.listen_addresses, vec!["10.0.0.7"]);
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

// --- Tolerant load of a NEWER schema (the A/B rollback path) ---------------
//
// docs/design/api.md §10.3 item 5: the down-migrations were dead code in
// production because Store::load refused any schema_version above its own
// BEFORE migrate() was reached, and the older binary cannot carry the future
// down-step in any case. The accepted resolution is a tolerant load whose
// semantics are the ones docs/design/mosd.md §5.2 already prices: keys the
// newer schema added are dropped; a reshaped document costs every setting.

/// Today's tree plus a key this schema does not know (an `expiresAt` on an
/// API token, which §3.2 refuses to add while the device has no trusted wall
/// clock) and a version stamp one ahead of ours.
///
/// A document from a build one schema AHEAD of this one -- the A/B rollback
/// path. Its version tracks SCHEMA_VERSION + 1 and has had to move nine
/// times, to 6 when the container switch landed, to 7 for the mqtt switch, to
/// 8 for the interface kinds, to 9 for the API token list, to 10 for the time
/// subtree, to 11 for the provisioning-document record, to 12 for the claim
/// record and to 13 for the staged reset: left behind, it stops
/// being "newer", the strip path stops running, and the test goes on passing
/// while asserting nothing about rollback. Hence the assertion below that the
/// stamp really is ahead of us.
///
/// The unknown key moved with it. It used to be `access.apiTokens` itself,
/// which this schema now knows, so the fixture would have asserted nothing:
/// the whole subtree would have loaded rather than been stripped.
fn newer_additive_document() -> String {
    assert_eq!(SCHEMA_VERSION + 1, 13, "the fixture stamp must stay ahead");
    r#"schema_version = 13
hostname = "rolled-back"

[network.eth0]
dhcp = true

[access.webAdmin]
password_hash = "$argon2id$fake"

[[access.apiTokens]]
id = "3f2a9c41"
name = "ci"
hash = "0000000000000000000000000000000000000000000000000000000000000001"
created = 1700000000
expiresAt = 1800000000
"#
    .to_string()
}

#[test]
fn newer_additive_document_loads_with_unknown_keys_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, newer_additive_document()).unwrap();

    let (settings, report) = Store::new(&path).load_with_report().unwrap();

    // Everything v4 understands survives — the admin credential above all,
    // because losing it is what puts the device back in setup mode.
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
    assert_eq!(settings.hostname, "rolled-back");
    assert!(settings.network["eth0"].dhcp);
    assert_eq!(
        settings.access.web_admin.as_ref().unwrap().password_hash,
        "$argon2id$fake"
    );

    // The token list is a key this schema DOES know, so it survives whole --
    // only the field a newer schema added to its entries is stripped.
    assert_eq!(settings.access.api_tokens.len(), 1);
    assert_eq!(settings.access.api_tokens[0].id, "3f2a9c41");
    assert_eq!(settings.access.api_tokens[0].created, 1_700_000_000);
    validate_api_tokens(&settings.access.api_tokens).unwrap();

    // The report names what rollback cost, for mosd to log.
    let report = report.expect("a newer document must produce a report");
    assert_eq!(report.from, SCHEMA_VERSION + 1);
    assert_eq!(report.dropped_keys, vec!["expiresAt".to_string()]);
    assert!(!report.defaulted);
}

#[test]
fn newer_reshaped_document_falls_back_to_defaults_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    // A future schema that RESHAPED an existing key: hostname became a table.
    // No amount of unknown-key stripping can make v4 parse this.
    fs::write(
        &path,
        "schema_version = 13\n\n[hostname]\nname = \"x\"\n\n[network]\n",
    )
    .unwrap();

    let (settings, report) = Store::new(&path).load_with_report().unwrap();

    // The written acceptance: everything is abandoned, the daemon still runs.
    assert_eq!(settings, Settings::default());
    let report = report.expect("a newer document must produce a report");
    assert_eq!(report.from, SCHEMA_VERSION + 1);
    assert!(report.defaulted);
}

#[test]
fn newer_document_never_errors_but_current_and_older_semantics_are_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");

    // Newer: tolerated (proved above). Current-version documents keep the
    // strict contract: an unknown key is still a load error, because on the
    // non-rollback path silently dropping a key would hide corruption.
    fs::write(
        &path,
        format!("schema_version = {SCHEMA_VERSION}\nhostname = \"h\"\nbogus = 1\n\n[network]\n"),
    )
    .unwrap();
    assert!(matches!(
        Store::new(&path).load(),
        Err(SettingsError::Parse(_))
    ));

    // And a malformed version stamp is still an error, newer-looking or not.
    fs::write(&path, "schema_version = \"5\"\n").unwrap();
    assert!(matches!(
        Store::new(&path).load(),
        Err(SettingsError::Parse(_))
    ));
}

#[test]
fn tolerated_document_saves_back_at_this_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(&path, newer_additive_document()).unwrap();

    let store = Store::new(&path);
    let (settings, report) = store.load_with_report().unwrap();
    assert!(report.is_some());
    store.save(&settings).unwrap();

    // The persisted file is now a clean document at this schema: reloading is
    // the normal path (no report), and the newer schema's key is gone from
    // disk — mosd.md §5.2's "rolling forward again restores the defaults, not
    // the values". The token list itself stays: this schema knows it, so only
    // the `expiresAt` a newer schema added to its entries was stripped.
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(&format!("schema_version = {SCHEMA_VERSION}")));
    assert!(!text.contains("expiresAt"));
    assert!(text.contains("[[access.apiTokens]]"));
    let (reloaded, report) = store.load_with_report().unwrap();
    assert_eq!(reloaded, settings);
    assert!(report.is_none());
}

#[test]
fn stripping_is_recursive_and_drops_same_named_keys_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    // The same unknown key at two depths. The strip is by name, everywhere:
    // both go, and the report records the name once per strip pass. The stamp
    // has to stay one ahead of us or the tolerant path never runs.
    assert_eq!(SCHEMA_VERSION + 1, 13, "the fixture stamp must stay ahead");
    fs::write(
        &path,
        r#"schema_version = 13
hostname = "h"
extra = "top"

[network.eth0]
dhcp = true
extra = "nested"
"#,
    )
    .unwrap();

    let (settings, report) = Store::new(&path).load_with_report().unwrap();
    assert_eq!(settings.hostname, "h");
    assert!(settings.network["eth0"].dhcp);
    let report = report.expect("report");
    assert_eq!(report.dropped_keys, vec!["extra".to_string()]);
    assert!(!report.defaulted);
}

// --- Interface kinds (schema v7) -------------------------------------------

/// A v7 document carrying one entry of every kind, spelled the way section 2.2
/// of the design spells it.
const V7_EVERY_KIND: &str = r#"schema_version = 7
hostname = "edge-1"

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
endpoint = "vpn.example.net:51820"
persistentKeepalive = 25
"#;

/// A key only a schema AFTER v12 could carry, appended to the fixture above to
/// make it a genuine rollback document rather than a re-stamped one.
const V13_ONLY_KEY: &str = r#"
[network.wg0.wireguard.obfuscation]
mode = "none"
"#;

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
    assert_eq!(
        text.parse::<toml::Table>().unwrap()["schema_version"],
        toml::Value::Integer(i64::from(SCHEMA_VERSION))
    );
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
    let path = dir.path().join("settings.toml");
    fs::write(&path, V7_EVERY_KIND).unwrap();
    let store = Store::new(&path);

    let (settings, report) = store.load_with_report().unwrap();
    assert!(
        report.is_none(),
        "a document at or below this schema is migrated up, not rolled back"
    );
    assert_eq!(settings, every_kind());

    // Written back, the `network` subtree is the one that came in -- the rest
    // of the file gains the serialized defaults of a saved tree, as any save
    // does.
    store.save(&settings).unwrap();
    let written: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    let fixture: toml::Table = V7_EVERY_KIND.parse().unwrap();
    assert_eq!(written["network"]["br0"], fixture["network"]["br0"]);
    assert_eq!(written["network"]["wg0"], fixture["network"]["wg0"]);
    assert_eq!(
        written["network"]["eth0.100"]["vlan"],
        fixture["network"]["eth0.100"]["vlan"]
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

/// The A/B rollback path, read from the other side: a document one schema
/// AHEAD that still carries the v7 kinds. The unknown key goes; every v7
/// interface — VLAN, bridge and WireGuard, blocks and peers included — is kept
/// intact, because the strip is by name and takes only what serde named.
#[test]
fn a_newer_document_keeps_every_v7_interface_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    assert_eq!(SCHEMA_VERSION + 1, 13, "the fixture stamp must stay ahead");
    fs::write(
        &path,
        V7_EVERY_KIND.replace("schema_version = 7", "schema_version = 13") + V13_ONLY_KEY,
    )
    .unwrap();

    let (settings, report) = Store::new(&path).load_with_report().unwrap();

    let report = report.expect("a newer document must produce a report");
    assert_eq!(report.from, SCHEMA_VERSION + 1);
    assert_eq!(report.dropped_keys, vec!["obfuscation".to_string()]);
    assert!(!report.defaulted);
    assert_eq!(settings, every_kind());
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
    let store = Store::new(dir.path().join("settings.toml"));
    store.save(&settings).unwrap();
    let text = fs::read_to_string(dir.path().join("settings.toml")).unwrap();
    assert!(
        text.contains(r#"[network."eth0.100"]"#),
        "persistence spells the key in the same syntax: {text}"
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
    let path = dir.path().join("settings.toml");
    fs::write(
        &path,
        format!("schema_version = {SCHEMA_VERSION}\nhostname = \"mos\"\n\n[network.\"eth0 100\"]\ndhcp = true\n"),
    )
    .unwrap();
    let store = Store::new(path);
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
    let store = Store::new(&path);

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

    let (loaded, report) = store.load_with_report().unwrap();
    assert!(report.is_none());
    assert_eq!(loaded, settings);

    // Order is the list's own and is not sorted underneath the caller: the id
    // is the identity, but the order is what a listing shows.
    assert_eq!(loaded.access.api_tokens[0].id, "3f2a9c41");
    assert_eq!(loaded.access.api_tokens[1].name, "backup runner");

    store.save(&loaded).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

/// A settings file written before this schema existed still loads, and loads
/// with an empty list rather than failing on the missing key. That is the
/// whole cost of the additive bump.
#[test]
fn a_document_without_the_token_key_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.toml");
    fs::write(
        &path,
        r#"schema_version = 7
hostname = "mos"

[network]

[access.webAdmin]
password_hash = "$argon2id$fake"
"#,
    )
    .unwrap();

    let (settings, report) = Store::new(&path).load_with_report().unwrap();

    assert!(report.is_none(), "migrating up is not a rollback");
    assert_eq!(settings.schema_version, SCHEMA_VERSION);
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
/// The property `MigrateV10ToV11` depends on: a v10 document and its v11 form
/// differ by the version integer alone until something claims the device.
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

    let store = Store::new(&path);
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
/// The property `MigrateV11ToV12` depends on: a v11 document and its v12 form
/// differ by the version integer alone until a reset is staged.
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

    let store = Store::new(&path);
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
