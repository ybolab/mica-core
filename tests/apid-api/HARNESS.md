# The apid API harness

`pkgs/mosd/tests/apid-api/run.sh` boots the x64 image in QEMU, exposes
apid's HTTP and HTTPS listeners, waits for the daemon, and runs the Bun suite
against the guest.

```sh
make os-apid-api-test
bash pkgs/mosd/tests/apid-api/run.sh --dry-run
```

The harness builds nothing. `_out/x64/x64-mos-latest.img` is an input; a
missing image is refused with the commands that produce it.

## Network path

The guest is never reached at `127.0.0.1`. QEMU's user-mode `hostfwd` binds
inside the QEMU container, that container publishes the port, and the suite
runs in another container. `run.sh` discovers the Docker network shared by the
current runner, finds the QEMU container by its bind mount on the run directory,
and uses that container's address on the shared network.

The run directory is `_out/x64/.qemu`. Two runs cannot safely share its
`disk.img`, so the harness resolves mount paths and refuses to start while a
running container already binds that directory. This also protects worktrees
whose `_out` is a symlink to another checkout.

## One prepared disk, one boot

The current SPA/API contract suite is non-destructive and uses one boot:

1. `src/qemu.ts --prepare-only` copies and grows the image and applies the
   kernel append.
2. `tools/qemu-seed-state.sh` places the independent kernel-network smoke
   script on STATE.
3. QEMU boots the prepared disk once.
4. The harness waits for both the `APID_LISTENING` console marker and a 200
   from `/healthz`.
5. The eight ordered phases test the SPA boundary, JSON sessions and CSRF,
   API-only management, live network observation, kernel link support, the
   claim and provisioning records, the rollback verdict, and the reset tiers
   with the physical-presence gate.

No custom UI fixture is seeded. The boot therefore proves the default device
root enters the embedded UI at `/ui`. Rust route tests separately install a
real custom bundle and prove that it owns `/` without shadowing `/ui`.

## Console and readiness

mos keeps journald volatile, so every boot is captured to
`_out/x64/apid-api/console-boot1.log` and
`systemd.journald.forward_to_console=1` makes the daemon's readiness marker
observable. The readiness deadline defaults to 900 seconds because TCG speed
varies significantly under contention. Progress is reported every 15 seconds.

`/healthz` is the listener-health contract. The probe runs from the same pinned
Bun image and Docker network as the suite, so it also proves the QEMU forward
and container routing are usable.

## Knobs

| variable | default | purpose |
| --- | --- | --- |
| `MOS_APID_PHASES` | all eight registered phases | restrict the ordered phase list; partial runs are reported loudly |
| `MOS_APID_READY_TIMEOUT` | `900` | deadline for apid readiness |
| `MOS_APID_CONTAINER_TIMEOUT` | `240` | deadline to find the QEMU container |
| `MOS_APID_KEEP_DISK` | `0` | retain the prepared `disk.img` after the run |
| `MOS_QEMU_HTTPS_PORT` / `MOS_QEMU_HTTP_PORT` | `18443` / `18080` | forwarded listeners |
| `MOS_QEMU_RUN_SECONDS` / `MOS_QEMU_TIMEOUT` | `2400` / `2700` | QEMU backstops |
| `APID_NEGATIVE` | — | invert the first matching check to prove the live suite can go red |

## Artifacts and reporting

The console log, suite log, phase result, and merged `result.json` land under
`_out/x64/apid-api/`. The merged result embeds the exact image identity and the
suite's machine-readable result.

Reporting follows the repository's `PASS:` / `FAIL:` register with a dynamic
`RESULT: PASS|FAIL (n/m checks)`. A run with zero checks is a failure, and a
phase after a failed prerequisite is skipped rather than reported as passing.

For a worktree without its own image, point `_out` at the checkout that built
one. The harness resolves that symlink both for collision detection and for the
mount it gives the suite container.
