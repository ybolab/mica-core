use mica_deploy::boot::shutdown::{Action, Budget, parse_mountinfo};
use mica_deploy::boot::shutdown::{
    Block, Device, LifecycleIo, LoopIdentity, Mapping, Operation, Ownership, Released, Snapshot,
    release,
};
use std::collections::BTreeSet;

fn device(major: u32, minor: u32) -> Device {
    Device { major, minor }
}
fn fixture() -> (Ownership, Fake) {
    let mounts=parse_mountinfo("1 0 0:1 / / rw - tmpfs tmpfs rw\n2 1 253:0 / /oldroot ro - squashfs /dev/dm-0 ro\n3 2 8:2 / /oldroot/mnt/system ro - ext4 /dev/sda2 ro\n4 2 8:3 / /oldroot/mnt/data rw - ext4 /dev/sda3 rw\n5 2 0:5 / /oldroot/containers rw - overlay overlay rw\n6 5 8:3 /storage /oldroot/containers/storage rw - ext4 /dev/sda3 rw\n").unwrap();
    let association = LoopIdentity {
        generation: 1,
        device: device(7, 0),
        backing: device(8, 2),
        inode: 42,
        offset: 0,
        size_limit: 0,
        flags: 1,
    };
    let mapping = Mapping {
        generation: 1,
        device: device(253, 0),
        name: "mica-root".into(),
        uuid: "CRYPT-VERITY-owned".into(),
        table: "0 8 verity 1 7:0 7:0 4096 4096 1 1 sha256 hash salt".into(),
    };
    let mut blocks = vec![
        Block {
            generation: 1,
            device: association.device,
            name: "loop0".into(),
            holders: BTreeSet::from([mapping.device]),
            slaves: BTreeSet::new(),
            association: Some(association.clone()),
            mapping: None,
        },
        Block {
            generation: 1,
            device: mapping.device,
            name: "dm-0".into(),
            holders: BTreeSet::new(),
            slaves: BTreeSet::from([association.device]),
            association: None,
            mapping: Some(mapping.clone()),
        },
    ];
    for minor in [2, 3] {
        blocks.push(Block {
            device: device(8, minor),
            generation: 1,
            name: format!("sda{minor}"),
            holders: BTreeSet::new(),
            slaves: BTreeSet::new(),
            association: None,
            mapping: None,
        });
    }
    let owner = Ownership {
        deployment: "a".repeat(64),
        backing_generations: vec![(device(8, 2), 1), (device(8, 3), 1)],
        backings: BTreeSet::from([device(8, 2), device(8, 3)]),
        loops: vec![association],
        mappings: vec![mapping],
        mounts: mounts[1..].to_vec(),
        allow_extra_loops: false,
    };
    (
        owner,
        Fake {
            state: Snapshot {
                mounts,
                blocks,
                processes: vec![],
                swaps: vec![],
                dirty_kib: 0,
            },
            now: 0,
            events: vec![],
            calls: vec![],
            actions: vec![],
            busy: 0,
            autoclear: false,
            sync_failure: false,
            reuse_after_scan: false,
            scans: 0,
            refusal: None,
            mutate_then_fail: false,
        },
    )
}
struct Fake {
    state: Snapshot,
    now: u64,
    events: Vec<String>,
    calls: Vec<Operation>,
    actions: Vec<Action>,
    busy: usize,
    autoclear: bool,
    sync_failure: bool,
    reuse_after_scan: bool,
    scans: usize,
    refusal: Option<&'static str>,
    mutate_then_fail: bool,
}
impl LifecycleIo for Fake {
    fn now_ms(&self) -> u64 {
        self.now
    }
    fn scan(&mut self, deadline: u64) -> anyhow::Result<Snapshot> {
        anyhow::ensure!(self.now < deadline, "deadline");
        self.now += 1;
        self.scans += 1;
        if self.reuse_after_scan && self.scans > 3 {
            self.state.blocks[0].association.as_mut().unwrap().inode = 99;
        }
        Ok(self.state.clone())
    }
    fn execute(&mut self, op: &Operation, deadline: u64) -> anyhow::Result<()> {
        anyhow::ensure!(self.now < deadline, "deadline");
        self.now += 1;
        self.calls.push(op.clone());
        let mutation = match op {
            Operation::Unmount(_) => Some("mount"),
            Operation::MoveBacking(_) => Some("move"),
            Operation::RemoveMapping(_) => Some("dm"),
            Operation::DetachLoop(_) => Some("loop"),
            _ => None,
        };
        if mutation.is_some() && mutation == self.refusal {
            anyhow::bail!("fixture operation refused");
        }
        match op {
            Operation::Unmount(m) => {
                if self.busy > 0 {
                    self.busy -= 1;
                    anyhow::bail!("EBUSY");
                }
                assert!(!self.state.mounts.iter().any(|child| child.parent == m.id));
                assert!(!self.state.blocks.iter().any(|b| {
                    b.association
                        .as_ref()
                        .is_some_and(|l| l.backing == m.device)
                }));
                self.state.mounts.retain(|x| x.id != m.id);
            }
            Operation::MoveBacking(m) => {
                let current = self.state.mounts.iter_mut().find(|x| x.id == m.id).unwrap();
                current.path = format!("/backing/{}", m.id);
                current.parent = 1;
            }
            Operation::RemoveMapping(m) => {
                assert!(
                    !self
                        .state
                        .mounts
                        .iter()
                        .any(|mount| mount.device == m.device)
                );
                assert!(
                    self.state
                        .blocks
                        .iter()
                        .find(|b| b.device == m.device)
                        .unwrap()
                        .holders
                        .is_empty()
                );
                self.state.blocks.retain(|b| b.device != m.device);
                for block in &mut self.state.blocks {
                    block.holders.remove(&m.device);
                }
            }
            Operation::DetachLoop(l) => {
                let block = self
                    .state
                    .blocks
                    .iter_mut()
                    .find(|b| b.device == l.device)
                    .unwrap();
                assert!(block.holders.is_empty());
                if !self.autoclear {
                    block.association = None;
                }
            }
            Operation::Sync | Operation::SyncMount(_) => {
                if self.sync_failure {
                    anyhow::bail!("sync failed");
                }
                self.state.dirty_kib = 0;
            }
            _ => {}
        }
        if self.mutate_then_fail && mutation.is_some() {
            anyhow::bail!("fixture mutated and failed");
        }
        Ok(())
    }
    fn event(&mut self, stage: &str, _detail: &str) {
        self.events.push(stage.into());
    }
    fn terminal(&mut self, action: Action, _released: Released) -> anyhow::Result<()> {
        assert!(
            self.state.mounts.len() == 1
                && self.state.blocks.iter().all(|b| b.association.is_none())
        );
        self.actions.push(action);
        Ok(())
    }
}

#[test]
fn reobserves_nested_moved_and_busy_graph_before_releasing_each_layer() {
    for verb in ["reboot", "poweroff", "halt"] {
        let (_action, (owner, mut io)) = (Action::parse(&[verb.into()]).unwrap(), fixture());
        io.busy = 2;
        release(&mut io, Budget::new(0, 60).unwrap(), &owner).unwrap();
        assert_eq!(io.events.last().unwrap(), "storage-released");
        assert!(
            io.scans
                > io.calls
                    .iter()
                    .filter(|op| matches!(
                        op,
                        Operation::Unmount(_)
                            | Operation::MoveBacking(_)
                            | Operation::RemoveMapping(_)
                            | Operation::DetachLoop(_)
                    ))
                    .count()
        );
        let dm = io
            .calls
            .iter()
            .position(|op| matches!(op, Operation::RemoveMapping(_)))
            .unwrap();
        let lp = io
            .calls
            .iter()
            .position(|op| matches!(op, Operation::DetachLoop(_)))
            .unwrap();
        let backing = io
            .calls
            .iter()
            .position(|op| matches!(op,Operation::Unmount(m) if m.device==device(8,2)))
            .unwrap();
        assert!(dm < lp && lp < backing);
    }
}

#[test]
fn autoclear_return_busy_devices_sync_failure_and_reuse_never_authorize_action() {
    for case in [
        "autoclear",
        "busy",
        "sync",
        "reuse",
        "namespace",
        "swap",
        "foreign-holder",
    ] {
        let (owner, mut io) = fixture();
        match case {
            "autoclear" => io.autoclear = true,
            "busy" => io.busy = 10000,
            "sync" => io.sync_failure = true,
            "reuse" => io.reuse_after_scan = true,
            "namespace" => io.state.processes.push(42),
            "swap" => io.state.swaps.push("/oldroot/swapfile".into()),
            "foreign-holder" => {
                io.state.blocks[1].holders.insert(device(253, 99));
            }
            _ => unreachable!(),
        }
        assert!(
            release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err(),
            "{case}"
        );
        assert!(!io.events.iter().any(|e| e == "storage-released"), "{case}");
    }
}

#[test]
fn record_cannot_authorize_foreign_mapping_names() {
    let (mut owner, mut io) = fixture();
    owner.mappings[0].name = "foreign".into();
    io.state.blocks[1].mapping = Some(owner.mappings[0].clone());
    assert!(release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err());
    assert!(
        !io.calls
            .iter()
            .any(|op| matches!(op, Operation::RemoveMapping(_)))
    );
}

#[test]
fn exact_actions_and_bounded_systemd_metadata() {
    for verb in ["reboot", "poweroff", "halt"] {
        assert_eq!(Action::parse(&[verb.into()]).unwrap().as_str(), verb);
        assert!(
            Action::parse(&[
                verb.into(),
                "--log-level=info".into(),
                "--log-target=console".into()
            ])
            .is_ok()
        );
    }
    for args in [
        vec![],
        vec!["kexec"],
        vec![""],
        vec!["reboot", "--force"],
        vec!["halt", "extra"],
        vec!["poweroff", "--timeout=0"],
    ] {
        assert!(Action::parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err());
    }
}

#[test]
fn watchdog_readback_clamps_one_absolute_budget() {
    for (timeout, total, cleanup) in [
        (120, 60000, 58000),
        (60, 50000, 48000),
        (30, 20000, 18000),
        (20, 10000, 8000),
    ] {
        let budget = Budget::new(1234, timeout).unwrap();
        assert_eq!(budget.deadline_ms, 1234 + total);
        assert_eq!(budget.cleanup_deadline_ms, 1234 + cleanup);
        assert_eq!(budget.operation_deadline(1234).unwrap(), 6234);
        assert_eq!(
            budget.operation_deadline(1234 + cleanup - 10).unwrap(),
            1234 + cleanup
        );
        assert!(budget.operation_deadline(1234 + cleanup).is_err());
    }
    for timeout in [0, 1, 15, 19, u64::MAX] {
        assert!(Budget::new(0, timeout).is_err());
    }
}

#[test]
fn mount_graph_preserves_ids_and_decodes_only_kernel_escapes() {
    let mounts = parse_mountinfo("1 0 0:1 / / rw - tmpfs tmpfs rw\n2 1 253:0 / /oldroot ro - squashfs /dev/dm-0 ro\n3 2 8:2 / /oldroot/mnt/sys\\040tem ro shared:1 - ext4 /dev/sda2 ro\n4 2 8:3 / /oldroot/mnt/data rw - ext4 /dev/sda3 rw\n").unwrap();
    assert_eq!(mounts[2].path, "/oldroot/mnt/sys tem");
    assert_eq!(mounts[2].parent, 2);
    for bad in [
        "",
        "bad",
        "1 1 0:1 / /bad rw - tmpfs tmpfs rw\n",
        "1 2 0:1 / / rw - tmpfs tmpfs rw\n2 1 8:2 / /x rw - ext4 x rw\n",
        "1 0 0:1 / /x\\041 rw - tmpfs tmpfs rw\n",
    ] {
        assert!(parse_mountinfo(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn unknown_active_storage_and_shared_propagation_refuse_terminal_authorization() {
    for case in [
        "foreign-loop",
        "foreign-mapping",
        "shared",
        "released-loop-holder",
    ] {
        let (owner, mut io) = fixture();
        match case {
            "foreign-loop" => {
                let mut b = io.state.blocks[0].clone();
                b.device = device(7, 99);
                b.holders.clear();
                let l = b.association.as_mut().unwrap();
                l.device = b.device;
                l.backing = device(8, 99);
                io.state.blocks.push(b);
            }
            "foreign-mapping" => {
                let mut b = io.state.blocks[1].clone();
                b.device = device(253, 99);
                b.holders.clear();
                b.slaves.clear();
                b.mapping.as_mut().unwrap().name = "foreign".into();
                io.state.blocks.push(b);
            }
            "shared" => io.state.mounts[0].propagation.push("shared:1".into()),
            "released-loop-holder" => {
                io.state.mounts.truncate(1);
                io.state.blocks.truncate(1);
                io.state.blocks[0].association = None;
            }
            _ => unreachable!(),
        }
        assert!(
            release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err(),
            "{case}"
        );
        assert!(!io.events.iter().any(|e| e == "storage-released"), "{case}");
    }
}

#[test]
fn partial_startup_acquisitions_and_absent_devices_are_idempotent() {
    for stage in [
        "before-loop",
        "after-loop",
        "after-dm",
        "after-mount",
        "after-move",
    ] {
        let (mut owner, mut io) = fixture();
        io.state.mounts.retain(|m| [1, 3, 4].contains(&m.id));
        for mount in &mut io.state.mounts {
            if mount.id != 1 {
                mount.parent = 1;
                mount.path = if mount.id == 3 { "/system" } else { "/data" }.into();
            }
        }
        if stage == "before-loop" {
            io.state.blocks.retain(|b| b.device.major == 8);
            owner.loops.clear();
            owner.mappings.clear();
        }
        if stage == "after-loop" {
            io.state.blocks.retain(|b| b.device.major != 253);
            io.state.blocks[0].holders.clear();
            owner.mappings.clear();
        }
        if ["after-mount", "after-move"].contains(&stage) {
            let path = if stage == "after-move" {
                "/elsewhere/root"
            } else {
                "/newroot"
            };
            io.state.mounts.push(parse_mountinfo(&format!("20 1 253:0 / {path} ro - squashfs /dev/dm-0 ro\n1 0 0:1 / / rw - tmpfs tmpfs rw\n")).unwrap().remove(0));
        }
        release(&mut io, Budget::new(0, 60).unwrap(), &owner).unwrap();
    }
    let (owner, mut io) = fixture();
    io.state.mounts.truncate(1);
    io.state.blocks.clear();
    release(&mut io, Budget::new(0, 60).unwrap(), &owner).unwrap();
}

#[test]
fn ambiguous_failed_attachment_is_not_adopted_by_backing_filename() {
    let (mut owner, mut io) = fixture();
    owner.loops.clear();
    owner.mappings.clear();
    io.state.blocks.truncate(1);
    io.state.blocks[0].holders.clear();
    io.state.mounts.retain(|m| [1, 3, 4].contains(&m.id));
    for m in &mut io.state.mounts {
        if m.id != 1 {
            m.parent = 1;
        }
    }
    assert!(release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err());
    assert!(
        !io.calls
            .iter()
            .any(|op| matches!(op, Operation::DetachLoop(_)))
    );
}

#[test]
fn expired_absolute_budget_prevents_any_operation() {
    for timeout in [20, 30, 60, 120] {
        let (owner, mut io) = fixture();
        let budget = Budget::new(0, timeout).unwrap();
        io.now = budget.cleanup_deadline_ms;
        assert!(release(&mut io, budget, &owner).is_err());
        assert!(io.calls.is_empty());
    }
}

#[test]
fn accepts_the_pinned_systemd_exec_argv() {
    let args = [
        "poweroff",
        "--timeout=90000000us",
        "--log-level",
        "info",
        "--log-target=kmsg",
        "--log-color",
        "--log-location",
        "--log-time",
        "--exit-code=0",
    ]
    .map(String::from);
    assert_eq!(Action::parse(&args).unwrap(), Action::Poweroff);
}

#[test]
fn exact_final_actions_require_release_and_returned_calls_fail() {
    for action in [Action::Reboot, Action::Poweroff, Action::Halt] {
        let (owner, mut io) = fixture();
        let error = mica_deploy::boot::shutdown::finish(
            &mut io,
            Budget::new(0, 60).unwrap(),
            &owner,
            action,
        )
        .unwrap_err();
        assert!(error.to_string().contains("terminal action returned"));
        assert_eq!(io.actions, vec![action]);
        let (owner, mut io) = fixture();
        io.autoclear = true;
        assert!(
            mica_deploy::boot::shutdown::finish(
                &mut io,
                Budget::new(0, 60).unwrap(),
                &owner,
                action
            )
            .is_err()
        );
        assert!(io.actions.is_empty());
    }
}

#[test]
fn initial_namespace_root_has_its_own_parent_id() {
    let mounts = parse_mountinfo(
        "1 1 0:1 / / rw - rootfs rootfs rw\n2 1 0:2 / /dev rw - devtmpfs devtmpfs rw\n",
    )
    .unwrap();
    assert_eq!(mounts[0].id, mounts[0].parent);
}

#[test]
fn a_reconfigured_loop_with_the_same_backing_inode_is_still_reused() {
    let (owner, mut io) = fixture();
    let mut state = serde_json::to_value(&io.state).unwrap();
    state["blocks"][0]["association"]["generation"] = 2.into();
    io.state = serde_json::from_value(state).unwrap();
    assert!(release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err());
    assert!(
        !io.calls
            .iter()
            .any(|op| matches!(op, Operation::DetachLoop(_)))
    );
}

#[test]
fn refused_operations_remain_in_the_graph_and_mutated_failures_are_reobserved() {
    for refused in ["mount", "move", "dm", "loop"] {
        let (owner, mut io) = fixture();
        io.refusal = Some(refused);
        assert!(
            release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err(),
            "{refused}"
        );
        assert!(!io.events.iter().any(|e| e == "storage-released"));
        assert!(io.scans < 512);
    }
    let (owner, mut io) = fixture();
    io.mutate_then_fail = true;
    release(&mut io, Budget::new(0, 60).unwrap(), &owner).unwrap();
    assert!(io.events.iter().any(|e| e == "operation-refused"));
    assert!(io.events.iter().any(|e| e == "release-observed"));
}

#[test]
fn backing_and_mapping_generation_changes_are_never_authorized() {
    for device in [device(8, 2), device(253, 0)] {
        let (owner, mut io) = fixture();
        let block = io
            .state
            .blocks
            .iter_mut()
            .find(|b| b.device == device)
            .unwrap();
        block.generation += 1;
        if let Some(mapping) = &mut block.mapping {
            mapping.generation += 1;
        }
        assert!(release(&mut io, Budget::new(0, 60).unwrap(), &owner).is_err());
        assert!(!io.calls.iter().any(|op| matches!(
            op,
            Operation::Unmount(_) | Operation::DetachLoop(_) | Operation::RemoveMapping(_)
        )));
    }
}
