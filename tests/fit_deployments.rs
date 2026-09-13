use mica_deploy::{
    deployments::{BootBackend, BootReceipt, DeploymentStore, SharedDataFailure},
    fit_env::{Environment, Record, encode},
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
    for (slot, offset) in mica_deploy::fit_env::FitLayout::Cx3576
        .offsets()
        .into_iter()
        .enumerate()
    {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&encode(&records, slot as u8).unwrap())
            .unwrap();
    }
    file.sync_all().unwrap();
    let store = DeploymentStore::new(
        dir.path().join("system"),
        BootBackend::Fit {
            layout: mica_deploy::fit_env::FitLayout::Cx3576,
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
        backend: mica_deploy::boot::BootKind::UbootFit,
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
        Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576)
            .unwrap()
            .records[0]
            .tries_left,
        Some(2)
    );
    store.confirm(&receipt).unwrap();
    assert!(
        store
            .retire_failed_confirmed(&receipt.deployment_id)
            .unwrap()
    );
    let env = Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576).unwrap();
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
        Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576)
            .unwrap()
            .records[0]
            .tries_left
            .is_none()
    );
    store.rollback(&receipt).unwrap();
    let env = Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576).unwrap();
    assert_eq!(env.records[0].tries_left, Some(0));
    assert_eq!(env.records[1].tries_left, None);
    assert!(store.rollback(&receipt).is_err());
    assert_eq!(
        before[..mica_deploy::fit_env::FitLayout::Cx3576.offsets()[0] as usize],
        fs::read(&firmware).unwrap()
            [..mica_deploy::fit_env::FitLayout::Cx3576.offsets()[0] as usize]
    );
}

#[test]
fn fit_confirmation_requires_the_running_kernel_and_consumed_attempt() {
    let (_dir, store, mut receipt, firmware) = fixture();
    receipt.kernel_id = "e".repeat(64);
    assert!(store.confirm(&receipt).is_err());
    receipt.kernel_id = "c".repeat(64);
    let mut env = Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576).unwrap();
    env.records[0].tries_left = Some(3);
    env.save(&firmware).unwrap();
    assert!(store.confirm(&receipt).is_err());
    assert_eq!(
        Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576)
            .unwrap()
            .records[0]
            .tries_left,
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
        Environment::load(&firmware, mica_deploy::fit_env::FitLayout::Cx3576)
            .unwrap()
            .records[0]
            .tries_left,
        None
    );
    fs::remove_dir(store.meta.join("deployments.pending")).unwrap();
    assert_eq!(
        store.confirm(&receipt).unwrap().current,
        Some(receipt.deployment_id)
    );
}
