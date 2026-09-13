use base64::{Engine, engine::general_purpose::STANDARD};
use mica_deploy::firmware::{authenticate_firmware, parse_firmware, verify_installed};
use serde_json::{Value, json};

fn golden() -> Value {
    serde_json::from_str(include_str!("component-contracts/firmware.json")).unwrap()
}

#[test]
fn firmware_manifests_match_the_shared_signed_contract() {
    let fixture = golden();
    let key: [u8; 32] = STANDARD
        .decode(fixture["publicKey"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    for record in fixture["records"].as_array().unwrap() {
        let envelope = record["envelope"].as_str().unwrap().as_bytes();
        let firmware = authenticate_firmware(envelope, &[key]).unwrap();
        assert_eq!(serde_json::to_value(&firmware).unwrap(), record["manifest"]);
        assert!(authenticate_firmware(envelope, &[[0; 32]]).is_err());
        let mut bad: Value = serde_json::from_slice(envelope).unwrap();
        bad["signature"] = json!(STANDARD.encode([0; 64]));
        assert!(authenticate_firmware(&serde_json::to_vec(&bad).unwrap(), &[key]).is_err());
    }
}

#[test]
fn firmware_constraints_reject_out_of_range_and_cross_board_writes() {
    let fixture = golden();
    let original = fixture["records"][2]["manifest"].clone();
    for (field, value) in [
        ("generation", json!(0)),
        ("arch", json!("amd64")),
        ("board", json!("unknown")),
        ("online", json!(true)),
        (
            "artifact",
            json!({"bytes":16744449,"sha256":"a".repeat(64)}),
        ),
        (
            "target",
            json!({"format":"rockchip-loader","diskOffset":16777216,"maxBytes":65536}),
        ),
        (
            "target",
            json!({"format":"efi","partition":2,"path":"EFI/BOOT/BOOTAA64.EFI"}),
        ),
    ] {
        let mut manifest = original.clone();
        manifest[field] = value;
        manifest["id"] = json!(mica_deploy::components::component_id(&manifest).unwrap());
        assert!(
            parse_firmware(&serde_json::to_vec(&manifest).unwrap()).is_err(),
            "{field}"
        );
    }
    let mut payload = serde_json::to_vec(&original).unwrap();
    payload.push(b'\n');
    assert!(parse_firmware(&payload).is_err());
}

#[test]
fn firmware_readback_checks_only_the_bound_loader_bytes_and_never_changes_counters() {
    use mica_deploy::deployments::BootBackend;
    use std::fs;
    let directory = tempfile::TempDir::new().unwrap();
    let bytes = b"isolated loader";
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    for index in [0, 2] {
        let mut value = golden()["records"][index]["manifest"].clone();
        value["artifact"] = json!({"bytes":bytes.len(),"sha256":digest});
        value["id"] = json!(mica_deploy::components::component_id(&value).unwrap());
        let manifest = parse_firmware(&serde_json::to_vec(&value).unwrap()).unwrap();
        let (boot, path) = if index == 0 {
            let esp = directory.path().join("esp");
            let file = esp.join("EFI/BOOT/BOOTX64.EFI");
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, bytes).unwrap();
            (BootBackend::Uefi { esp }, file)
        } else {
            let file = directory.path().join("firmware.img");
            let mut image = bytes.to_vec();
            image.extend_from_slice(b"unchanged environment area");
            fs::write(&file, image).unwrap();
            (
                BootBackend::Fit {
                    layout: mica_deploy::fit_env::FitLayout::Cx3576,
                    firmware: file.clone(),
                },
                file,
            )
        };
        let before = fs::read(&path).unwrap();
        verify_installed(&manifest, &boot).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut damaged = before;
        damaged[0] ^= 1;
        fs::write(&path, damaged).unwrap();
        assert!(verify_installed(&manifest, &boot).is_err());
    }
}

#[test]
fn amlogic_receipt_checks_payload_after_the_vendor_header() {
    let directory = tempfile::tempdir().unwrap();
    let payload = b"signed S7D boot payload";
    let digest = hex::encode(ring::digest::digest(&ring::digest::SHA256, payload));
    let mut value = golden()["records"][2]["manifest"].clone();
    value["board"] = json!("s905x5m");
    value["target"] = json!({"format":"amlogic-boot0","payloadOffset":512,"maxBytes":4193792});
    value["artifact"] = json!({"bytes":payload.len(),"sha256":digest});
    value["id"] = json!(mica_deploy::components::component_id(&value).unwrap());
    let manifest = parse_firmware(&serde_json::to_vec(&value).unwrap()).unwrap();
    let path = directory.path().join("boot0");
    let mut bytes = vec![42; 512];
    bytes.extend_from_slice(payload);
    std::fs::write(&path, &bytes).unwrap();
    mica_deploy::firmware::verify_boot0_payload(&manifest, &path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    bytes[0] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    mica_deploy::firmware::verify_boot0_payload(&manifest, &path).unwrap();
    bytes[512] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    assert!(mica_deploy::firmware::verify_boot0_payload(&manifest, &path).is_err());
    for target in [
        json!({"format":"amlogic-boot0","payloadOffset":0,"maxBytes":4193792}),
        json!({"format":"amlogic-boot0","payloadOffset":512,"maxBytes":4194304}),
        json!({"format":"amlogic-boot1","payloadOffset":512,"maxBytes":4193792}),
    ] {
        value["target"] = target;
        value["id"] = json!(mica_deploy::components::component_id(&value).unwrap());
        assert!(parse_firmware(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
