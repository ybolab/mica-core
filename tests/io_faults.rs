//! Process-interruption and ENOSPC acceptance for the real transaction implementation.
use base64::{Engine, engine::general_purpose::STANDARD};
use mos_deploy::{
    boot::BootKind,
    components::{authenticate_deployment, component_id},
    deployments::{BootBackend, BootReceipt, DeploymentStore, State},
    fit_env::{Record, encode},
};
use ring::{
    digest,
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

fn store(root: &Path, fit: bool) -> DeploymentStore {
    DeploymentStore::new(
        root.join("store/system"),
        if fit {
            BootBackend::Fit {
                layout: mos_deploy::fit_env::FitLayout::Cx3576,
                firmware: root.join("store/firmware.img"),
            }
        } else {
            BootBackend::Uefi {
                esp: root.join("store/esp"),
            }
        },
        root.join("store/meta"),
    )
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes))
}
fn write(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn receipt(d: &Value, fit: bool) -> BootReceipt {
    let id = component_id(d).unwrap();
    BootReceipt {
        backend: if fit {
            BootKind::UbootFit
        } else {
            BootKind::Uefi
        },
        entry: if fit {
            format!("fit:{id}")
        } else {
            format!("mos-{id}.conf")
        },
        deployment_id: id,
        kernel_id: d["kernel"]["id"].as_str().unwrap().into(),
        rootfs_id: d["rootfs"]["id"].as_str().unwrap().into(),
        content_verified: true,
        boot_verified: true,
        secure_boot: !fit,
    }
}
fn paths(root: &Path, d: &Value, fit: bool) -> Vec<PathBuf> {
    let kernel = d["kernel"]["id"].as_str().unwrap();
    let rootfs = d["rootfs"]["id"].as_str().unwrap();
    vec![
        if fit {
            root.join(format!("store/system/kernels/{kernel}/boot.itb"))
        } else {
            root.join(format!("store/esp/EFI/mos/kernels/{kernel}.efi"))
        },
        root.join(format!("store/system/kernels/{kernel}/support.img")),
        root.join(format!(
            "store/system/kernels/{kernel}/support.roothash.p7s"
        )),
        root.join(format!("store/system/roots/{rootfs}/rootfs.img")),
        root.join(format!("store/system/roots/{rootfs}/rootfs.roothash.p7s")),
    ]
}
fn seed(root: &Path, fit: bool, operation: &str) {
    let store = store(root, fit);
    let key = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
            .unwrap()
            .as_ref(),
    )
    .unwrap();
    let public: [u8; 32] = key.public_key().as_ref().try_into().unwrap();
    let mut deployments = Vec::new();
    let mut envelopes = Vec::new();
    for generation in 1..=3 {
        let payload = vec![
            if generation == 3 && operation == "reuse" {
                40
            } else {
                39 + generation as u8
            };
            12288
        ];
        let artifact = json!({"bytes":payload.len(),"sha256":hash(&payload)});
        let mut d: Value = serde_json::from_slice(include_bytes!(
            "../../../tests/component-contracts/deployment.json"
        ))
        .unwrap();
        if fit {
            d["board"] = json!("cx3576");
            d["arch"] = json!("arm64");
            d["kernel"]["board"] = json!("cx3576");
            d["kernel"]["arch"] = json!("arm64");
            d["kernel"]["boot"]["format"] = json!("fit");
            d["rootfs"]["arch"] = json!("arm64");
        }
        for path in [
            "/kernel/boot/artifact",
            "/kernel/support/image",
            "/kernel/support/signature",
            "/rootfs/content/image",
            "/rootfs/content/signature",
        ] {
            *d.pointer_mut(path).unwrap() = artifact.clone();
        }
        d["generation"] = json!(generation);
        d["kernel"]["id"] = json!(component_id(&d["kernel"]).unwrap());
        d["rootfs"]["id"] = json!(component_id(&d["rootfs"]).unwrap());
        let bytes = serde_json::to_vec(&d).unwrap();
        let envelope = format!(
            "{{\"schema\":\"mos/update-envelope/v1\",\"keyId\":\"{}\",\"payload\":\"{}\",\"signature\":\"{}\"}}",
            hash(&public), STANDARD.encode(&bytes), STANDARD.encode(key.sign(&bytes))
        ).into_bytes();
        write(&root.join("objects").join(hash(&payload)), &payload);
        if generation < 3 {
            for path in paths(root, &d, fit) {
                write(&path, &payload);
            }
            for (namespace, name, component) in [
                ("kernels", "support", "kernel"),
                ("roots", "rootfs", "rootfs"),
            ] {
                let id = d[component]["id"].as_str().unwrap();
                write(
                    &store
                        .system
                        .join(format!("{namespace}/{id}/{name}.roothash")),
                    "a".repeat(64).as_bytes(),
                );
            }
            write(
                &store
                    .system
                    .join(format!("deployments/{}.json", component_id(&d).unwrap())),
                &envelope,
            );
        }
        deployments.push(d);
        envelopes.push(envelope);
    }
    let records: Vec<_> = deployments[..2]
        .iter()
        .rev()
        .map(|d| Record {
            id: component_id(d).unwrap(),
            kernel_id: d["kernel"]["id"].as_str().unwrap().into(),
            generation: d["generation"].as_u64().unwrap(),
            tries_left: None,
        })
        .collect();
    if fit {
        let mut file = fs::File::create(root.join("store/firmware.img")).unwrap();
        file.set_len(18 * 1048576 - 32768).unwrap();
        for (i, offset) in mos_deploy::fit_env::FitLayout::Cx3576
            .offsets()
            .into_iter()
            .enumerate()
        {
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&encode(&records, i as u8).unwrap()).unwrap();
        }
    } else {
        for record in &records {
            write(
                &root.join(format!("store/esp/loader/entries/mos-{}.conf", record.id)),
                format!(
                    "title MOS\nversion {}\nsort-key mos\nefi /EFI/mos/kernels/{}.efi\n",
                    record.generation, record.kernel_id
                )
                .as_bytes(),
            );
        }
    }
    fs::create_dir_all(&store.meta).unwrap();
    store
        .save_state(&State {
            current: Some(records[0].id.clone()),
            fallback: Some(records[1].id.clone()),
            highest_generation: 2,
            ..State::default()
        })
        .unwrap();
    write(&root.join("public"), &public);
    write(&root.join("candidate.json"), &envelopes[2]);
    write(
        &root.join("candidate-receipt.json"),
        &serde_json::to_vec(&receipt(&deployments[2], fit)).unwrap(),
    );
    write(&root.join("retained-id"), records[0].id.as_bytes());
    write(&root.join("retired-id"), records[1].id.as_bytes());
    write(
        &root.join("current-receipt.json"),
        &serde_json::to_vec(&receipt(&deployments[1], fit)).unwrap(),
    );
    if !["install", "reuse"].contains(&operation) {
        store
            .install(
                &envelopes[2],
                &[public],
                if fit { "cx3576" } else { "x64" },
                if fit { "arm64" } else { "amd64" },
                &root.join("objects"),
                &serde_json::from_slice(&fs::read(root.join("current-receipt.json")).unwrap())
                    .unwrap(),
            )
            .unwrap();
    }
    if !["install", "reuse"].contains(&operation) {
        if let BootBackend::Fit { firmware, .. } = &store.boot {
            let mut environment = mos_deploy::fit_env::Environment::load(
                firmware,
                mos_deploy::fit_env::FitLayout::Cx3576,
            )
            .unwrap();
            environment.records[0].tries_left = Some(2);
            environment.save(firmware).unwrap();
        } else {
            let id = component_id(&deployments[2]).unwrap();
            fs::rename(
                root.join(format!("store/esp/loader/entries/mos-{id}+3.conf")),
                root.join(format!("store/esp/loader/entries/mos-{id}+2-1.conf")),
            )
            .unwrap();
        }
    }
    if operation == "gc" {
        store.confirm(&receipt(&deployments[2], fit)).unwrap();
        write(
            &store
                .system
                .join(format!("roots/{}/rootfs.img", "f".repeat(64))),
            b"unreachable",
        );
    }
}
fn run_operation(root: &Path, fit: bool, operation: &str) {
    let store = store(root, fit);
    let public: [u8; 32] = fs::read(root.join("public")).unwrap().try_into().unwrap();
    let receipt =
        serde_json::from_slice(&fs::read(root.join("candidate-receipt.json")).unwrap()).unwrap();
    match operation {
        "install" => {
            store
                .install(
                    &fs::read(root.join("candidate.json")).unwrap(),
                    &[public],
                    if fit { "cx3576" } else { "x64" },
                    if fit { "arm64" } else { "amd64" },
                    &root.join("objects"),
                    &serde_json::from_slice(&fs::read(root.join("current-receipt.json")).unwrap())
                        .unwrap(),
                )
                .unwrap();
        }
        "confirm" => {
            store.confirm(&receipt).unwrap();
        }
        "gc" => {
            store.collect(&receipt, &[public]).unwrap();
        }
        _ => panic!("unknown operation"),
    }
}
fn validate(root: &Path, fit: bool) {
    let store = store(root, fit);
    let public: [u8; 32] = fs::read(root.join("public")).unwrap().try_into().unwrap();
    let mut entries = store.entries().unwrap();
    assert!(entries.len() <= 2, "a third deployment was published");
    let retained = fs::read_to_string(root.join("retained-id")).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.id == retained && e.tries_left != Some(0))
    );
    if let BootBackend::Fit { firmware, .. } = &store.boot {
        let bytes = fs::read(firmware).unwrap();
        for offset in mos_deploy::fit_env::FitLayout::Cx3576.offsets() {
            // Either CRC-valid redundant copy must refer only to complete
            // deployments, even if a later read loses the newest copy.
            let mut isolated = tempfile::NamedTempFile::new().unwrap();
            isolated.as_file().set_len(18 * 1048576 - 32768).unwrap();
            isolated.seek(SeekFrom::Start(offset)).unwrap();
            isolated
                .write_all(&bytes[offset as usize..offset as usize + mos_deploy::fit_env::ENV_SIZE])
                .unwrap();
            if let Ok(environment) = mos_deploy::fit_env::Environment::load(
                isolated.path(),
                mos_deploy::fit_env::FitLayout::Cx3576,
            ) {
                let records = environment.records;
                for record in records {
                    if !entries.iter().any(|entry| entry.id == record.id) {
                        entries.push(mos_deploy::deployments::Entry {
                            id: record.id,
                            file: String::new(),
                            generation: record.generation,
                            tries_left: record.tries_left,
                        });
                    }
                }
            }
        }
    }
    for entry in &entries {
        let d = authenticate_deployment(
            &fs::read(store.system.join(format!("deployments/{}.json", entry.id))).unwrap(),
            &[public],
        )
        .unwrap();
        let value = serde_json::to_value(&d).unwrap();
        assert_eq!(component_id(&value).unwrap(), entry.id);
        let expected = [
            &d.kernel.boot.artifact,
            &d.kernel.support.image,
            &d.kernel.support.signature,
            &d.rootfs.content.image,
            &d.rootfs.content.signature,
        ];
        for (path, artifact) in paths(root, &value, fit).iter().zip(expected) {
            let bytes = fs::read(path).unwrap();
            assert_eq!(bytes.len() as u64, artifact.bytes);
            assert_eq!(hash(&bytes), artifact.sha256);
        }
    }
    store.effective_state().unwrap();
}

#[test]
#[ignore = "requires tests/file-ab-faults/run.sh and its isolated IO fault shim"]
fn transactions_survive_each_boundary() {
    use std::os::unix::process::ExitStatusExt;
    if let Ok(root) = std::env::var("MOS_FAULT_WORK") {
        run_operation(
            Path::new(&root),
            std::env::var("MOS_FAULT_FIT").unwrap() == "1",
            &std::env::var("MOS_FAULT_OPERATION").unwrap(),
        );
        return;
    }
    let shim = std::env::var("MOS_TEST_FAULT_SHIM").expect("run tests/file-ab-faults/run.sh");
    let evidence = PathBuf::from(std::env::var("MOS_TEST_FAULT_EVIDENCE").unwrap());
    let mut summaries = Vec::new();
    for fit in [false, true] {
        for operation in ["install", "confirm", "gc"] {
            let invoke = |root: &Path, at: usize, phase: &str, enospc: bool| {
                let mut cmd = Command::new(std::env::current_exe().unwrap());
                cmd.args([
                    "--ignored",
                    "--exact",
                    "transactions_survive_each_boundary",
                    "--test-threads=1",
                ])
                .env("LD_PRELOAD", &shim)
                .env("MOS_FAULT_WORK", root)
                .env("MOS_FAULT_OPERATION", operation)
                .env("MOS_FAULT_FIT", if fit { "1" } else { "0" })
                .env("MOS_FAULT_PREFIX", root.join("store"))
                .env("MOS_FAULT_LOG", root.join("trace"))
                .env("MOS_FAULT_AT", at.to_string())
                .env("MOS_FAULT_WHEN", phase);
                if enospc {
                    cmd.env("MOS_FAULT_ENOSPC", "1");
                }
                cmd.output().unwrap()
            };
            let baseline = TempDir::new_in(&evidence).unwrap();
            seed(baseline.path(), fit, operation);
            let result = invoke(baseline.path(), 0, "before", false);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
            validate(baseline.path(), fit);
            let trace = fs::read_to_string(baseline.path().join("trace")).unwrap();
            assert!(
                trace.contains("fsync"),
                "missing persistence instrumentation"
            );
            if operation == "install" {
                assert!(trace.contains("copy_file_range") || trace.contains("write"));
                assert!(trace.contains("rename"));
            }
            if operation == "gc" {
                assert!(trace.contains("unlink") && trace.contains("rmdir"));
            }
            let count = trace.lines().count();
            fs::write(evidence.join(format!("{fit}-{operation}.trace")), trace).unwrap();
            for at in 1..=count {
                for (phase, enospc) in [("before", false), ("after", false), ("before", true)] {
                    let case = TempDir::new_in(&evidence).unwrap();
                    seed(case.path(), fit, operation);
                    let result = invoke(case.path(), at, phase, enospc);
                    let check = std::panic::catch_unwind(|| {
                        if enospc {
                            assert!(result.status.success() || result.status.code() == Some(101));
                        } else {
                            assert_eq!(
                                result.status.signal(),
                                Some(9),
                                "boundary {at}/{count}: {phase} {operation}"
                            );
                        }
                        validate(case.path(), fit);
                        // Restart the interrupted transaction with its durable
                        // state; an already published candidate is reconciled.
                        let restarted = store(case.path(), fit);
                        if operation != "install"
                            || restarted.effective_state().unwrap().candidate.is_none()
                        {
                            run_operation(case.path(), fit, operation);
                        } else {
                            let receipt = serde_json::from_slice(
                                &fs::read(case.path().join("current-receipt.json")).unwrap(),
                            )
                            .unwrap();
                            restarted.confirm(&receipt).unwrap();
                        }
                        validate(case.path(), fit);
                    });
                    if let Err(error) = check {
                        eprintln!("failed fixture retained: {}", case.keep().display());
                        std::panic::resume_unwind(error);
                    }
                }
            }
            let summary = format!(
                "fit={fit} {operation}: {count} IO boundaries, {} interrupted/error cases",
                count * 3
            );
            println!("{summary}");
            summaries.push(summary);
        }
    }
    fs::write(evidence.join("results.txt"), summaries.join("\n")).unwrap();
}

#[test]
fn install_replaces_the_inactive_deployment_and_preserves_shared_objects() {
    for fit in [false, true] {
        for operation in ["install", "reuse"] {
            let root = TempDir::new().unwrap();
            seed(root.path(), fit, operation);
            let store = store(root.path(), fit);
            let old = fs::read_to_string(root.path().join("retired-id")).unwrap();
            let old_descriptor = store.system.join(format!("deployments/{old}.json"));
            let public: [u8; 32] = fs::read(root.path().join("public"))
                .unwrap()
                .try_into()
                .unwrap();
            let old_deployment =
                authenticate_deployment(&fs::read(&old_descriptor).unwrap(), &[public]).unwrap();
            if operation == "reuse" {
                // Acquisition omitted an object already verified on SYSTEM.
                fs::remove_file(
                    root.path()
                        .join("objects")
                        .join(&old_deployment.rootfs.content.image.sha256),
                )
                .unwrap();
            }
            run_operation(root.path(), fit, "install");
            validate(root.path(), fit);
            assert!(!old_descriptor.exists());
            assert_eq!(store.state().unwrap().fallback, None);
            assert_eq!(store.entries().unwrap().len(), 2);
            assert_eq!(
                store
                    .system
                    .join(format!("roots/{}", old_deployment.rootfs.id))
                    .exists(),
                operation == "reuse"
            );
            assert_eq!(
                fs::read_dir(store.system.join("deployments"))
                    .unwrap()
                    .count(),
                2
            );
        }
    }
}

#[test]
fn invalid_update_preserves_both_installed_deployments() {
    for fit in [false, true] {
        let root = TempDir::new().unwrap();
        seed(root.path(), fit, "install");
        let store = store(root.path(), fit);
        let public: [u8; 32] = fs::read(root.path().join("public"))
            .unwrap()
            .try_into()
            .unwrap();
        let envelope = fs::read(root.path().join("candidate.json")).unwrap();
        let candidate = authenticate_deployment(&envelope, &[public]).unwrap();
        fs::write(
            root.path()
                .join("objects")
                .join(&candidate.rootfs.content.image.sha256),
            b"corrupt",
        )
        .unwrap();
        let before = serde_json::to_vec(&store.entries().unwrap()).unwrap();
        assert!(
            store
                .install(
                    &envelope,
                    &[public],
                    if fit { "cx3576" } else { "x64" },
                    if fit { "arm64" } else { "amd64" },
                    &root.path().join("objects"),
                    &serde_json::from_slice(
                        &fs::read(root.path().join("current-receipt.json")).unwrap()
                    )
                    .unwrap()
                )
                .is_err()
        );
        assert_eq!(
            before,
            serde_json::to_vec(&store.entries().unwrap()).unwrap()
        );
        validate(root.path(), fit);
    }
}

#[test]
fn install_requires_the_confirmed_running_receipt_and_reconciles_activation() {
    for fit in [false, true] {
        let root = TempDir::new().unwrap();
        seed(root.path(), fit, "install");
        let store = store(root.path(), fit);
        let public: [u8; 32] = fs::read(root.path().join("public"))
            .unwrap()
            .try_into()
            .unwrap();
        let envelope = fs::read(root.path().join("candidate.json")).unwrap();
        let mut current: BootReceipt =
            serde_json::from_slice(&fs::read(root.path().join("current-receipt.json")).unwrap())
                .unwrap();
        let install = |receipt: &BootReceipt| {
            store.install(
                &envelope,
                &[public],
                if fit { "cx3576" } else { "x64" },
                if fit { "arm64" } else { "amd64" },
                &root.path().join("objects"),
                receipt,
            )
        };
        current.content_verified = false;
        assert!(
            install(&current)
                .unwrap_err()
                .to_string()
                .contains("authenticated")
        );
        current.content_verified = true;
        let candidate_receipt: BootReceipt =
            serde_json::from_slice(&fs::read(root.path().join("candidate-receipt.json")).unwrap())
                .unwrap();
        assert!(install(&candidate_receipt).is_err());
        let pending = install(&current).unwrap();
        let candidate = pending.candidate.unwrap();
        let mut interrupted = store.state().unwrap();
        interrupted.candidate = None;
        interrupted.highest_generation = 2;
        store.save_state(&interrupted).unwrap();
        let recovered = store.effective_state().unwrap();
        assert_eq!(recovered.candidate.as_deref(), Some(candidate.as_str()));
        assert_eq!(recovered.highest_generation, 3);
        assert_eq!(
            store.confirm(&current).unwrap().candidate,
            Some(candidate.clone())
        );
        assert_eq!(store.describe(&[public]).unwrap().len(), 2);
        assert!(install(&current).is_err());
        assert!(
            store.confirm(&candidate_receipt).is_err(),
            "unlaunched trial was confirmed"
        );
        if let BootBackend::Fit { firmware, .. } = &store.boot {
            let mut env = mos_deploy::fit_env::Environment::load(
                firmware,
                mos_deploy::fit_env::FitLayout::Cx3576,
            )
            .unwrap();
            env.records[0].tries_left = Some(2);
            env.save(firmware).unwrap();
        } else {
            fs::rename(
                root.path()
                    .join(format!("store/esp/loader/entries/mos-{candidate}+3.conf")),
                root.path()
                    .join(format!("store/esp/loader/entries/mos-{candidate}+2-1.conf")),
            )
            .unwrap();
        }
        assert!(install(&candidate_receipt).is_err());
        let confirmed = store.confirm(&candidate_receipt).unwrap();
        assert_eq!(confirmed.current, Some(candidate));
        assert_eq!(confirmed.fallback, Some(current.deployment_id));
        assert!(confirmed.candidate.is_none());
        validate(root.path(), fit);
    }
}

#[test]
#[ignore = "requires the bounded tmpfs provided by tests/file-ab-faults/run.sh"]
fn replacement_capacity_uses_reclaimed_blocks_without_a_third_version() {
    use mos_deploy::components::authenticate_deployment;
    let parent = std::env::var("MOS_TEST_SPACE_ROOT").unwrap();
    for insufficient in [false, true] {
        let root = TempDir::new_in(&parent).unwrap();
        seed(root.path(), true, "install");
        let store = store(root.path(), true);
        let public: [u8; 32] = fs::read(root.path().join("public"))
            .unwrap()
            .try_into()
            .unwrap();
        let envelope = fs::read(root.path().join("candidate.json")).unwrap();
        let receipt =
            serde_json::from_slice(&fs::read(root.path().join("current-receipt.json")).unwrap())
                .unwrap();
        let d = authenticate_deployment(&envelope, &[public]).unwrap();
        let needed: u64 = store.object_paths(&d).iter().map(|(_, a)| a.bytes).sum();
        let reserve = 128 * 1048576;
        let target = if insufficient {
            reserve - 32768
        } else {
            reserve + 16384
        };
        let available = rustix::fs::statvfs(&store.system).unwrap();
        let filler = available.f_bavail * available.f_frsize - target;
        fs::write(root.path().join("filler"), vec![0_u8; filler as usize]).unwrap();
        let before = rustix::fs::statvfs(&store.system).unwrap();
        assert!(before.f_bavail * before.f_frsize < needed + reserve);
        let entries = serde_json::to_vec(&store.entries().unwrap()).unwrap();
        let result = store.install(
            &envelope,
            &[public],
            "cx3576",
            "arm64",
            &root.path().join("objects"),
            &receipt,
        );
        if insufficient {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("insufficient destination space")
            );
            assert_eq!(
                entries,
                serde_json::to_vec(&store.entries().unwrap()).unwrap()
            );
        } else {
            result.unwrap();
            let after = rustix::fs::statvfs(&store.system).unwrap();
            assert!(after.f_bavail * after.f_frsize >= reserve);
        }
        validate(root.path(), true);
    }
}

#[test]
fn an_unconfirmed_running_trial_cannot_retire_the_other_deployment() {
    for fit in [false, true] {
        let root = TempDir::new().unwrap();
        seed(root.path(), fit, "install");
        let store = store(root.path(), fit);
        let current: BootReceipt =
            serde_json::from_slice(&fs::read(root.path().join("current-receipt.json")).unwrap())
                .unwrap();
        if let BootBackend::Fit { firmware, .. } = &store.boot {
            let mut env = mos_deploy::fit_env::Environment::load(
                firmware,
                mos_deploy::fit_env::FitLayout::Cx3576,
            )
            .unwrap();
            env.records
                .iter_mut()
                .for_each(|entry| entry.tries_left = Some(2));
            env.save(firmware).unwrap();
        } else {
            for entry in store.entries().unwrap() {
                fs::rename(
                    root.path()
                        .join("store/esp/loader/entries")
                        .join(entry.file),
                    root.path().join(format!(
                        "store/esp/loader/entries/mos-{}+2-1.conf",
                        entry.id
                    )),
                )
                .unwrap();
            }
        }
        let public: [u8; 32] = fs::read(root.path().join("public"))
            .unwrap()
            .try_into()
            .unwrap();
        let envelope = fs::read(root.path().join("candidate.json")).unwrap();
        let entries = serde_json::to_vec(&store.entries().unwrap()).unwrap();
        let error = store
            .install(
                &envelope,
                &[public],
                if fit { "cx3576" } else { "x64" },
                if fit { "arm64" } else { "amd64" },
                &root.path().join("objects"),
                &current,
            )
            .unwrap_err();
        assert!(error.to_string().contains("confirm the running deployment"));
        assert_eq!(
            entries,
            serde_json::to_vec(&store.entries().unwrap()).unwrap()
        );
        validate(root.path(), fit);
    }
}
