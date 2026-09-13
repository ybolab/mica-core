use mica_deploy::deployments::{BootBackend, BootReceipt, DeploymentStore};
use std::fs;
use tempfile::TempDir;

fn uefi_esp(store: &DeploymentStore) -> &std::path::Path {
    match &store.boot {
        BootBackend::Uefi { esp } => esp,
        BootBackend::Fit { .. } => panic!("expected UEFI fixture"),
    }
}

fn fixture() -> (TempDir, DeploymentStore, BootReceipt) {
    let dir = TempDir::new().unwrap();
    let store = DeploymentStore::new(
        dir.path().join("system"),
        BootBackend::Uefi {
            esp: dir.path().join("esp"),
        },
        dir.path().join("meta"),
    );
    fs::create_dir_all(uefi_esp(&store).join("loader/entries")).unwrap();
    fs::create_dir_all(&store.meta).unwrap();
    let current = "a".repeat(64);
    let fallback = "b".repeat(64);
    for (id, suffix, generation) in [(&current, "+2-1", 2), (&fallback, "", 1)] {
        fs::write(
            uefi_esp(&store).join(format!("loader/entries/mos-{id}{suffix}.conf")),
            format!(
                "title MOS\nversion {generation}\nsort-key mos\nefi /EFI/mos/kernels/{}.efi\n",
                "c".repeat(64)
            ),
        )
        .unwrap();
    }
    let receipt = BootReceipt {
        deployment_id: current.clone(),
        entry: format!("mos-{current}.conf"),
        kernel_id: "c".repeat(64),
        rootfs_id: "d".repeat(64),
        content_verified: true,
        secure_boot: true,
        boot_verified: true,
        backend: mica_deploy::boot::BootKind::Uefi,
    };
    (dir, store, receipt)
}

#[test]
fn a_failed_confirmed_boot_is_retired_without_refilling_trials() {
    let (_dir, store, receipt) = fixture();
    assert!(
        !store
            .retire_failed_confirmed(&receipt.deployment_id)
            .unwrap()
    );
    assert_eq!(store.entries().unwrap()[0].tries_left, Some(2));
    store.confirm(&receipt).unwrap();
    assert!(
        store
            .retire_failed_confirmed(&receipt.deployment_id)
            .unwrap()
    );
    assert_eq!(store.entries().unwrap()[0].tries_left, Some(0));
    assert!(
        !store
            .retire_failed_confirmed(&receipt.deployment_id)
            .unwrap()
    );
    assert!(store.retire_failed_confirmed(&"b".repeat(64)).is_err());
}

#[test]
fn confirms_only_the_authenticated_running_trial_and_retains_fallback() {
    let (_dir, store, mut receipt) = fixture();
    receipt.content_verified = false;
    assert!(store.confirm(&receipt).is_err());
    assert!(
        uefi_esp(&store)
            .join(format!(
                "loader/entries/mos-{}+2-1.conf",
                receipt.deployment_id
            ))
            .exists()
    );
    receipt.content_verified = true;
    let state = store.confirm(&receipt).unwrap();
    assert_eq!(
        state.current.as_deref(),
        Some(receipt.deployment_id.as_str())
    );
    assert_eq!(state.fallback.as_deref(), Some("b".repeat(64).as_str()));
    assert!(
        uefi_esp(&store)
            .join(format!("loader/entries/mos-{}.conf", receipt.deployment_id))
            .exists()
    );
    assert_eq!(store.confirm(&receipt).unwrap().current, state.current);
}

#[test]
fn a_durable_confirmation_is_visible_before_its_data_state_write() {
    let (_dir, store, receipt) = fixture();
    fs::rename(
        uefi_esp(&store).join(format!(
            "loader/entries/mos-{}+2-1.conf",
            receipt.deployment_id
        )),
        uefi_esp(&store).join(format!("loader/entries/mos-{}.conf", receipt.deployment_id)),
    )
    .unwrap();
    let state = store.effective_state().unwrap();
    assert_eq!(
        state.current.as_deref(),
        Some(receipt.deployment_id.as_str())
    );
    assert_eq!(state.highest_generation, 2);
    assert!(state.candidate.is_none());
}

#[test]
fn rollback_exhausts_current_without_refilling_fallback_attempts() {
    let (_dir, store, receipt) = fixture();
    store.confirm(&receipt).unwrap();
    store.rollback(&receipt).unwrap();
    assert!(
        uefi_esp(&store)
            .join(format!(
                "loader/entries/mos-{}+0-3.conf",
                receipt.deployment_id
            ))
            .exists()
    );
    assert!(
        uefi_esp(&store)
            .join(format!("loader/entries/mos-{}.conf", "b".repeat(64)))
            .exists()
    );
    assert!(store.reject(&"b".repeat(64)).is_err());
}

#[test]
fn rollback_requires_a_confirmed_running_deployment_and_no_candidate() {
    let (_dir, store, mut receipt) = fixture();
    assert!(store.rollback(&receipt).is_err());
    store.confirm(&receipt).unwrap();
    receipt.content_verified = false;
    assert!(store.rollback(&receipt).is_err());
    receipt.content_verified = true;
    let fallback = uefi_esp(&store).join(format!("loader/entries/mos-{}.conf", "b".repeat(64)));
    let fallback_bytes = fs::read(&fallback).unwrap();
    fs::remove_file(&fallback).unwrap();
    let candidate = uefi_esp(&store).join(format!("loader/entries/mos-{}+3.conf", "e".repeat(64)));
    fs::write(
        &candidate,
        format!(
            "title MOS\nversion 3\nsort-key mos\nefi /EFI/mos/kernels/{}.efi\n",
            "c".repeat(64)
        ),
    )
    .unwrap();
    assert!(store.rollback(&receipt).is_err());
    fs::remove_file(candidate).unwrap();
    fs::write(&fallback, fallback_bytes).unwrap();
    store.rollback(&receipt).unwrap();
    assert!(store.rollback(&receipt).is_err());
}

#[test]
fn refuses_ambiguous_entries_and_symbolic_state_files() {
    let (_dir, store, receipt) = fixture();
    let duplicate = uefi_esp(&store).join(format!(
        "loader/entries/mos-{}+1-2.conf",
        receipt.deployment_id
    ));
    fs::write(&duplicate, "version 2\n").unwrap();
    assert!(store.confirm(&receipt).is_err());
    fs::remove_file(duplicate).unwrap();
    std::os::unix::fs::symlink("outside", store.meta.join("deployments.json")).unwrap();
    assert!(store.confirm(&receipt).is_err());
}

#[test]
fn a_rollback_boot_can_confirm_the_last_usable_deployment() {
    let (_dir, store, mut receipt) = fixture();
    store.confirm(&receipt).unwrap();
    store.reject(&receipt.deployment_id).unwrap();
    receipt.deployment_id = "b".repeat(64);
    receipt.entry = format!("mos-{}.conf", receipt.deployment_id);
    let state = store.confirm(&receipt).unwrap();
    assert_eq!(state.current, Some(receipt.deployment_id));
    assert_eq!(state.fallback, None);
    assert_eq!(store.entries().unwrap().len(), 1);
}

#[test]
fn collection_keeps_two_bootable_deployments_and_discards_stale_state_references() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use mica_deploy::components::component_id;
    use mica_deploy::deployments::State;
    use ring::{
        digest,
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };
    use serde_json::{Value, json};
    let (dir, store, mut receipt) = fixture();
    fs::remove_dir_all(uefi_esp(&store).join("loader/entries")).unwrap();
    fs::create_dir(uefi_esp(&store).join("loader/entries")).unwrap();
    fs::create_dir_all(store.system.join("deployments")).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    let public: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    let mut records = Vec::new();
    for generation in 1..=4 {
        let mut d: Value =
            serde_json::from_slice(include_bytes!("component-contracts/deployment.json")).unwrap();
        d["generation"] = json!(generation);
        d["rootfs"]["version"] = json!(format!("root-{generation}"));
        d["rootfs"]["id"] = json!(component_id(&d["rootfs"]).unwrap());
        let id = component_id(&d).unwrap();
        let bytes = serde_json::to_vec(&d).unwrap();
        let envelope = format!(
            "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
            hex::encode(digest::digest(&digest::SHA256, key.public_key().as_ref())),
            STANDARD.encode(&bytes),
            STANDARD.encode(key.sign(&bytes))
        );
        fs::write(
            store.system.join(format!("deployments/{id}.json")),
            &envelope,
        )
        .unwrap();
        let root_id = d["rootfs"]["id"].as_str().unwrap().to_string();
        let kernel_id = d["kernel"]["id"].as_str().unwrap().to_string();
        fs::create_dir_all(store.system.join(format!("roots/{root_id}"))).unwrap();
        fs::write(
            store.system.join(format!("roots/{root_id}/rootfs.img")),
            "immutable root",
        )
        .unwrap();
        fs::create_dir_all(store.system.join(format!("kernels/{kernel_id}"))).unwrap();
        fs::write(
            store
                .system
                .join(format!("kernels/{kernel_id}/support.img")),
            "shared support",
        )
        .unwrap();
        fs::create_dir_all(uefi_esp(&store).join("EFI/mos/kernels")).unwrap();
        fs::write(
            uefi_esp(&store).join(format!("EFI/mos/kernels/{kernel_id}.efi")),
            "shared kernel",
        )
        .unwrap();
        if generation == 2 || generation == 3 {
            fs::write(
                uefi_esp(&store).join(format!("loader/entries/mos-{id}.conf")),
                format!("version {generation}\n"),
            )
            .unwrap();
        }
        records.push((id, root_id, kernel_id, envelope));
    }
    store
        .save_state(&State {
            highest_generation: 3,
            current: Some(records[1].0.clone()),
            fallback: Some(records[0].0.clone()),
            candidate: Some(records[2].0.clone()),
            failed: vec![],
        })
        .unwrap();
    receipt.deployment_id = records[2].0.clone();
    receipt.entry = format!("mos-{}.conf", receipt.deployment_id);
    receipt.rootfs_id = records[2].1.clone();
    receipt.kernel_id = records[2].2.clone();
    let fallback = store
        .system
        .join(format!("deployments/{}.json", records[1].0));
    fs::write(&fallback, "corrupt").unwrap();
    assert!(store.collect(&receipt, &[public]).is_err());
    let garbage = store.system.join(format!("roots/{}", records[3].1));
    assert!(
        garbage.exists(),
        "collection deleted before validating all retained references"
    );
    fs::write(&fallback, &records[1].3).unwrap();
    store.collect(&receipt, &[public]).unwrap();
    assert!(!garbage.exists());
    for record in records.iter().skip(1).take(2) {
        assert!(
            store
                .system
                .join(format!("roots/{}/rootfs.img", record.1))
                .exists()
        );
        assert!(
            uefi_esp(&store)
                .join(format!("EFI/mos/kernels/{}.efi", record.2))
                .exists()
        );
    }
    assert!(
        store
            .system
            .join(format!("kernels/{}/support.img", records[0].2))
            .exists()
    );
    assert_eq!(store.collect(&receipt, &[public]).unwrap(), 0);
    let interrupted = [
        store
            .system
            .join(format!("deployments/{}.pending", records[3].0)),
        uefi_esp(&store).join(format!("EFI/mos/kernels/{}.partial", records[0].2)),
        uefi_esp(&store).join(format!("loader/entries/mos-{}+3.pending", records[3].0)),
        store.meta.join("deployments.pending"),
    ];
    for path in &interrupted {
        fs::write(path, b"interrupted transaction").unwrap();
    }
    assert_eq!(
        store.collect(&receipt, &[public]).unwrap(),
        interrupted.len()
    );
    assert!(interrupted.iter().all(|path| !path.exists()));
    assert_eq!(store.collect(&receipt, &[public]).unwrap(), 0);
    assert!(dir.path().exists());
}

#[test]
fn fallback_confirmation_retires_an_exhausted_candidate() {
    let (_dir, store, receipt) = fixture();
    let mut state = store.confirm(&receipt).unwrap();
    fs::remove_file(uefi_esp(&store).join(format!("loader/entries/mos-{}.conf", "b".repeat(64))))
        .unwrap();
    state.fallback = None;
    let candidate = "e".repeat(64);
    state.candidate = Some(candidate.clone());
    store.save_state(&state).unwrap();
    fs::write(
        uefi_esp(&store).join(format!("loader/entries/mos-{candidate}+0-3.conf")),
        "version 3\n",
    )
    .unwrap();
    let state = store.confirm(&receipt).unwrap();
    assert_eq!(state.candidate, None);
    assert!(state.failed.contains(&candidate));
    assert_eq!(state.highest_generation, 3);
    assert_eq!(store.entries().unwrap().len(), 1);
}

#[test]
fn data_metadata_failure_is_classified_after_durable_confirmation() {
    use mica_deploy::deployments::SharedDataFailure;
    let (_dir, store, receipt) = fixture();
    fs::create_dir(store.meta.join("deployments.pending")).unwrap();
    let error = store.confirm(&receipt).unwrap_err();
    assert!(error.is::<SharedDataFailure>());
    assert!(
        uefi_esp(&store)
            .join(format!("loader/entries/mos-{}.conf", receipt.deployment_id))
            .is_file()
    );
    fs::remove_dir(store.meta.join("deployments.pending")).unwrap();
    assert_eq!(
        store.confirm(&receipt).unwrap().current,
        Some(receipt.deployment_id)
    );
    fs::write(store.meta.join("deployments.json"), "broken").unwrap();
    assert!(store.state().unwrap_err().is::<SharedDataFailure>());
}
