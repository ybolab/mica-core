use base64::{Engine, engine::general_purpose::STANDARD};
use mos_deploy::catalog::{CatalogCheckpoint, CatalogRequest, verify_catalog};
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};

fn signer() -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap()
}
fn signed(value: &Value) -> Vec<u8> {
    let key = signer();
    let payload = serde_json::to_vec(value).unwrap();
    format!(
        "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
        hex::encode(ring::digest::digest(&ring::digest::SHA256, key.public_key().as_ref())),
        STANDARD.encode(&payload), STANDARD.encode(key.sign(&payload).as_ref())
    ).into_bytes()
}
fn catalog() -> Value {
    let deployment: Value = serde_json::from_str(include_str!(
        "../../../tests/component-contracts/deployment.json"
    ))
    .unwrap();
    let mut objects = std::collections::BTreeMap::new();
    for pointer in [
        "/kernel/boot/artifact",
        "/kernel/support/image",
        "/kernel/support/signature",
        "/rootfs/content/image",
        "/rootfs/content/signature",
    ] {
        let artifact = deployment.pointer(pointer).unwrap();
        objects.insert(artifact["sha256"].as_str().unwrap(), json!({"sha256":artifact["sha256"],"bytes":artifact["bytes"],"url":format!("https://updates.test/v1/objects/{}",artifact["sha256"].as_str().unwrap())}));
    }
    json!({"schema":"mos/catalog/v1","revision":2,"issuedAt":"2026-09-09T00:00:00.000Z","expiresAt":"2026-09-10T00:00:00.000Z",
        "channels":[{"board":"x64","channel":"stable","releaseId":"release-1","generation":1}],
        "releases":[{"id":"release-1","channel":"stable","notes":"Test","deployment":String::from_utf8(signed(&deployment)).unwrap(),"objects":objects.values().collect::<Vec<_>>()}]})
}
fn verify(
    value: &Value,
    checkpoint: Option<&CatalogCheckpoint>,
    highest: u64,
) -> anyhow::Result<mos_deploy::catalog::VerifiedCatalog> {
    let key: [u8; 32] = signer().public_key().as_ref().try_into().unwrap();
    verify_catalog(
        &signed(value),
        &[key],
        &CatalogRequest {
            source: "https://updates.test/v1/manifest.json",
            board: "x64",
            arch: "amd64",
            channel: "stable",
            now: 1788915600,
            checkpoint,
            highest_generation: highest,
        },
    )
}

#[test]
fn authenticates_catalog_and_selects_only_a_new_exact_board_channel_deployment() {
    let value = catalog();
    let verified = verify(&value, None, 0).unwrap();
    assert_eq!(verified.selected.unwrap().deployment.generation, 1);
    assert!(
        verify(&value, Some(&verified.checkpoint), 1)
            .unwrap()
            .selected
            .is_none()
    );
    let key: [u8; 32] = signer().public_key().as_ref().try_into().unwrap();
    assert!(
        verify_catalog(
            &signed(&value),
            &[key],
            &CatalogRequest {
                source: "https://updates.test/v1/manifest.json",
                board: "virt-arm64",
                arch: "arm64",
                channel: "stable",
                now: 1788915600,
                checkpoint: None,
                highest_generation: 0
            }
        )
        .unwrap()
        .selected
        .is_none()
    );
}

#[test]
fn rejects_expiry_clock_rollback_equivocation_and_untrusted_signatures() {
    let value = catalog();
    let verified = verify(&value, None, 0).unwrap();
    for (field, replacement) in [
        ("expiresAt", json!("2026-09-08T00:00:00.000Z")),
        ("issuedAt", json!("2026-09-11T00:00:00.000Z")),
        ("revision", json!(1)),
        ("schema", json!("mos/updates/v1")),
    ] {
        let mut changed = value.clone();
        changed[field] = replacement;
        assert!(
            verify(&changed, Some(&verified.checkpoint), 0).is_err(),
            "{field}"
        );
    }
    let mut changed = value.clone();
    changed["releases"][0]["notes"] = json!("changed without a new revision");
    assert!(verify(&changed, Some(&verified.checkpoint), 0).is_err());
    assert!(
        verify_catalog(
            &signed(&value),
            &[[0; 32]],
            &CatalogRequest {
                source: "https://updates.test/v1/manifest.json",
                board: "x64",
                arch: "amd64",
                channel: "stable",
                now: 1788915600,
                checkpoint: None,
                highest_generation: 0
            }
        )
        .is_err()
    );
}

#[test]
fn rejects_missing_extra_substituted_objects_redirect_origins_and_inconsistent_heads() {
    for (pointer, replacement) in [
        ("/releases/0/objects/0/bytes", json!(1)),
        (
            "/releases/0/objects/0/url",
            json!("https://foreign.test/object"),
        ),
        ("/releases/0/objects/0/url", json!("file:///etc/passwd")),
        ("/releases/0/deployment", json!("{}")),
        ("/channels/0/generation", json!(2)),
        ("/channels/0/releaseId", json!("missing")),
        ("/channels/0/board", json!("virt-arm64")),
    ] {
        let mut value = catalog();
        *value.pointer_mut(pointer).unwrap() = replacement;
        assert!(verify(&value, None, 0).is_err(), "{pointer}");
    }
    let mut value = catalog();
    value["releases"][0]["objects"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert!(verify(&value, None, 0).is_err());
    let mut value = catalog();
    let duplicate = value["releases"][0]["objects"][0].clone();
    value["releases"][0]["objects"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(verify(&value, None, 0).is_err());
}
