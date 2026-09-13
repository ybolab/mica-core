use mica_deploy::boot::{selected_entry, utf16_variable};

#[test]
fn normalizes_only_native_deployment_entries() {
    let id = "a".repeat(64);
    for suffix in ["", "+3", "+2-1", "+1-2", "+0-3"] {
        assert_eq!(
            selected_entry(&format!("mica-{id}{suffix}.conf")).unwrap(),
            id
        );
    }
    for suffix in ["+4", "+3-3", "+-1", "+0", "+2-01", "/../x", "+1-2.conf"] {
        assert!(selected_entry(&format!("mica-{id}{suffix}.conf")).is_err());
    }
    assert!(selected_entry(&format!("mica-{}.conf", "A".repeat(64))).is_err());
    assert!(selected_entry("../deployment.json").is_err());
}

#[test]
fn reads_efi_attributes_and_strict_utf16_termination() {
    let mut bytes = vec![7, 0, 0, 0];
    for c in "mica-entry.conf\0".encode_utf16() {
        bytes.extend(c.to_le_bytes());
    }
    assert_eq!(utf16_variable(&bytes).unwrap(), "mica-entry.conf");
    assert!(utf16_variable(&bytes[..bytes.len() - 1]).is_err());
    assert!(utf16_variable(&bytes[..bytes.len() - 2]).is_err());
    bytes.extend([0, 0]);
    assert!(utf16_variable(&bytes).is_err());
}

#[test]
fn preserves_only_the_bounded_exitrd_closure() {
    use mica_deploy::boot::copy_exitrd;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let source = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let mut payload = [0_u8; 128];
    payload[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    payload[16] = 3;
    payload[18] = if cfg!(target_arch = "x86_64") {
        62
    } else {
        183
    };
    payload[20] = 1;
    payload[52] = 64;
    payload[32] = 64;
    payload[54] = 56;
    payload[56] = 1;
    payload[64] = 1;
    payload[96] = 128;
    let path = source.path().join("shutdown");
    fs::write(&path, payload).unwrap();
    fs::set_permissions(
        source.path().join("shutdown"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    copy_exitrd(source.path(), target.path(), "shutdown\n").unwrap();
    assert_eq!(fs::read(target.path().join("shutdown")).unwrap(), payload);
    assert_eq!(
        fs::metadata(target.path().join("shutdown"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert!(target.path().join("etc/initrd-release").is_file());
    assert!(target.path().join("oldroot").is_dir());
    for manifest in [
        "../shutdown\n",
        "/shutdown\n",
        "\n",
        &"shutdown\n".repeat(129),
    ] {
        assert!(copy_exitrd(source.path(), target.path(), manifest).is_err());
    }
    symlink("shutdown", source.path().join("link")).unwrap();
    assert!(copy_exitrd(source.path(), target.path(), "link\n").is_err());
    let oversized = fs::File::create(source.path().join("large")).unwrap();
    oversized.set_len(32 * 1024 * 1024 + 1).unwrap();
    assert!(copy_exitrd(source.path(), target.path(), "large\n").is_err());
    assert!(!target.path().join("large").exists());
}

#[test]
fn machine_identity_is_durable_and_never_regenerated_from_invalid_state() {
    use mica_deploy::boot::persistent_machine_id;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let id = "0123456789abcdef0123456789abcdef";
    assert_eq!(persistent_machine_id(&state, || Ok(id.into())).unwrap(), id);
    assert_eq!(
        persistent_machine_id(&state, || panic!("identity regenerated")).unwrap(),
        id
    );
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::write(state.join("machine-id"), "corrupt").unwrap();
    assert!(persistent_machine_id(&state, || panic!("corruption replaced")).is_err());
    fs::write(state.join("machine-id"), "0".repeat(32)).unwrap();
    assert!(persistent_machine_id(&state, || panic!("zero identity replaced")).is_err());
    fs::remove_file(state.join("machine-id")).unwrap();
    fs::write(dir.path().join("outside"), id).unwrap();
    symlink(dir.path().join("outside"), state.join("machine-id")).unwrap();
    assert!(persistent_machine_id(&state, || panic!("symlink followed")).is_err());
    symlink(&state, dir.path().join("alias")).unwrap();
    assert!(
        persistent_machine_id(&dir.path().join("alias"), || panic!(
            "state symlink followed"
        ))
        .is_err()
    );
}
#[test]
fn fit_selection_accepts_only_one_nul_terminated_deployment_digest() {
    use mica_deploy::boot::{BootKind, fit_selected};
    let id = "a".repeat(64);
    assert_eq!(BootKind::for_board("cx3576").unwrap(), BootKind::UbootFit);
    assert_eq!(BootKind::for_board("s905x5m").unwrap(), BootKind::UbootFit);
    assert_eq!(BootKind::for_board("virt-arm64").unwrap(), BootKind::Uefi);
    assert!(BootKind::for_board("unknown").is_err());
    assert_eq!(fit_selected(format!("{id}\0").as_bytes()).unwrap(), id);
    for bytes in [
        id.into_bytes(),
        vec![0; 65],
        format!("{}\0", "A".repeat(64)).into_bytes(),
        format!("{}\0\0", "a".repeat(64)).into_bytes(),
    ] {
        assert!(fit_selected(&bytes).is_err());
    }
}
