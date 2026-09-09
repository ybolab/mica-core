use mos_deploy::{
    deployments::{BootBackend, BootReceipt, DeploymentStore, SharedDataFailure},
    fit_env::{ENV_OFFSETS, Environment, Record, encode},
};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::PathBuf,
};
use tempfile::TempDir;

fn fixture() -> (TempDir, DeploymentStore, BootReceipt, PathBuf) {
    let dir = TempDir::new().unwrap();
    let firmware = dir.path().join("firmware.img");
    let mut file = fs::File::create(&firmware).unwrap();
    file.set_len(18 * 1048576 - 32768).unwrap();
    let records = vec![
        Record {
            id: "a".repeat(64),
            kernel_id: "c".repeat(64),
            generation: 2,
            tries_left: Some(2),
        },
        Record {
            id: "b".repeat(64),
            kernel_id: "c".repeat(64),
            generation: 1,
            tries_left: None,
        },
    ];
    for (slot, offset) in ENV_OFFSETS.into_iter().enumerate() {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&encode(&records, slot as u8).unwrap())
            .unwrap();
    }
    file.sync_all().unwrap();
    let store = DeploymentStore::new(
        dir.path().join("system"),
        BootBackend::Fit {
            firmware: firmware.clone(),
        },
        dir.path().join("meta"),
    );
    fs::create_dir_all(&store.meta).unwrap();
    let receipt = BootReceipt {
        deployment_id: "a".repeat(64),
        entry: format!("fit:{}", "a".repeat(64)),
        kernel_id: "c".repeat(64),
        rootfs_id: "d".repeat(64),
        content_verified: true,
        secure_boot: false,
        boot_verified: true,
        backend: mos_deploy::boot::BootKind::UbootFit,
    };
    (dir, store, receipt, firmware)
}

#[test]
fn fit_content_failure_retires_only_an_uncounted_boot_with_a_fallback() {
    let (_dir, store, receipt, firmware) = fixture();
    assert!(
        !store
            .retire_failed_confirmed(&receipt.deployment_id)
            .unwrap()
    );
    assert_eq!(
        Environment::load(&firmware).unwrap().records[0].tries_left,
        Some(2)
    );
    store.confirm(&receipt).unwrap();
    assert!(
        store
            .retire_failed_confirmed(&receipt.deployment_id)
            .unwrap()
    );
    let env = Environment::load(&firmware).unwrap();
    assert_eq!(env.records[0].tries_left, Some(0));
    assert_eq!(env.records[1].tries_left, None);
    assert!(store.retire_failed_confirmed(&"b".repeat(64)).is_err());
}

#[test]
fn fit_confirmation_and_rollback_preserve_the_loader_and_fallback_counter() {
    let (_dir, store, receipt, firmware) = fixture();
    let before = fs::read(&firmware).unwrap();
    let state = store.confirm(&receipt).unwrap();
    assert_eq!(state.current.as_ref(), Some(&receipt.deployment_id));
    assert_eq!(state.fallback, Some("b".repeat(64)));
    assert!(
        Environment::load(&firmware).unwrap().records[0]
            .tries_left
            .is_none()
    );
    store.rollback(&receipt).unwrap();
    let env = Environment::load(&firmware).unwrap();
    assert_eq!(env.records[0].tries_left, Some(0));
    assert_eq!(env.records[1].tries_left, None);
    assert!(store.rollback(&receipt).is_err());
    assert_eq!(
        before[..ENV_OFFSETS[0] as usize],
        fs::read(&firmware).unwrap()[..ENV_OFFSETS[0] as usize]
    );
}

#[test]
fn fit_confirmation_requires_the_running_kernel_and_consumed_attempt() {
    let (_dir, store, mut receipt, firmware) = fixture();
    receipt.kernel_id = "e".repeat(64);
    assert!(store.confirm(&receipt).is_err());
    receipt.kernel_id = "c".repeat(64);
    let mut env = Environment::load(&firmware).unwrap();
    env.records[0].tries_left = Some(3);
    env.save(&firmware).unwrap();
    assert!(store.confirm(&receipt).is_err());
    assert_eq!(
        Environment::load(&firmware).unwrap().records[0].tries_left,
        Some(3)
    );
}

#[test]
fn fit_confirmation_reconciles_data_failure_without_refilling_attempts() {
    let (_dir, store, receipt, firmware) = fixture();
    fs::create_dir(store.meta.join("deployments.pending")).unwrap();
    assert!(
        store
            .confirm(&receipt)
            .unwrap_err()
            .is::<SharedDataFailure>()
    );
    assert_eq!(
        Environment::load(&firmware).unwrap().records[0].tries_left,
        None
    );
    fs::remove_dir(store.meta.join("deployments.pending")).unwrap();
    assert_eq!(
        store.confirm(&receipt).unwrap().current,
        Some(receipt.deployment_id)
    );
}

#[test]
fn fit_installation_commits_verified_system_objects_before_the_environment() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use mos_deploy::components::component_id;
    use ring::{
        digest,
        signature::{Ed25519KeyPair, KeyPair},
    };
    use serde_json::{Value, json};
    let (dir, store, receipt, firmware) = fixture();
    store.confirm(&receipt).unwrap();
    let mut deployment: Value = serde_json::from_slice(include_bytes!(
        "../../../tests/component-contracts/deployment.json"
    ))
    .unwrap();
    deployment["board"] = json!("cx3576");
    deployment["arch"] = json!("arm64");
    deployment["kernel"]["board"] = json!("cx3576");
    deployment["kernel"]["arch"] = json!("arm64");
    deployment["rootfs"]["arch"] = json!("arm64");
    deployment["kernel"]["boot"]["format"] = json!("fit");
    deployment["generation"] = json!(3);
    let payload = vec![42_u8; 12288];
    let hash = hex::encode(digest::digest(&digest::SHA256, &payload));
    for path in [
        "/kernel/boot/artifact",
        "/kernel/support/image",
        "/kernel/support/signature",
        "/rootfs/content/image",
        "/rootfs/content/signature",
    ] {
        *deployment.pointer_mut(path).unwrap() = json!({"bytes":payload.len(),"sha256":hash});
    }
    deployment["kernel"]["id"] = json!(component_id(&deployment["kernel"]).unwrap());
    deployment["rootfs"]["id"] = json!(component_id(&deployment["rootfs"]).unwrap());
    let id = component_id(&deployment).unwrap();
    let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
    let public: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    let bytes = serde_json::to_vec(&deployment).unwrap();
    let envelope = format!(
        "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
        hex::encode(digest::digest(&digest::SHA256, &public)),
        STANDARD.encode(&bytes), STANDARD.encode(key.sign(&bytes)),
    ).into_bytes();
    let objects = dir.path().join("objects");
    fs::create_dir(&objects).unwrap();
    fs::write(objects.join(&hash), b"corrupt").unwrap();
    let before = fs::read(&firmware).unwrap();
    assert!(
        store
            .install(&envelope, &[public], "cx3576", "arm64", &objects)
            .is_err()
    );
    assert_eq!(before, fs::read(&firmware).unwrap());
    fs::write(objects.join(&hash), &payload).unwrap();
    // DATA failure after environment activation must retain the candidate.
    fs::create_dir(store.meta.join("deployments.pending")).unwrap();
    let failure = store
        .install(&envelope, &[public], "cx3576", "arm64", &objects)
        .unwrap_err();
    assert!(failure.is::<SharedDataFailure>(), "{failure:#}");
    fs::remove_dir(store.meta.join("deployments.pending")).unwrap();
    assert_eq!(store.effective_state().unwrap().candidate, Some(id.clone()));
    assert_eq!(store.confirm(&receipt).unwrap().candidate, Some(id.clone()));
    let env = Environment::load(&firmware).unwrap();
    assert_eq!(env.records.len(), 3);
    assert_eq!(env.records[0].id, id);
    assert_eq!(env.records[0].tries_left, Some(3));
    assert_eq!(
        fs::read(
            store
                .system
                .join(format!("kernels/{}/boot.itb", env.records[0].kernel_id))
        )
        .unwrap(),
        payload
    );
    assert_eq!(
        before[..ENV_OFFSETS[0] as usize],
        fs::read(&firmware).unwrap()[..ENV_OFFSETS[0] as usize]
    );
    assert!(
        store
            .install(&envelope, &[public], "cx3576", "arm64", &objects)
            .is_err()
    );
}
