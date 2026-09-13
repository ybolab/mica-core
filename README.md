# mica-deploy

Native boot and update tools for current signed file deployments. This workspace
contains `mica-runkit` (reached as `init` and `shutdown`) and `mica-deploy`; it accepts no earlier
disk, metadata or package format. It is a repository of its own, standing on
the `mica-build-env` substrate fetched at its pin into `build-env/`:

```
make deps            # build-env/ at deps/sources/mica-build-env.json
make build-env       # the builder images
make check           # lint, the Rust gate, the shutdown suite, the IO fault suite
make pool            # mica-deploy and mica-lifecycle, both architectures, indexed
make package-gate    # the gate over that pool
make publish         # the release build-<commit12> of this commit
```

Two packages leave here: `mica-deploy` (the device-side client) and
`mica-lifecycle` (the static `mica-runkit` under
`/usr/lib/mica/lifecycle/`, which the assembly's kernel component reads out
of the archive and packs into the signed kernel; no root installs it). The
assembly (`ybolab/mica-build`) imports both through `deps/packages/` and
builds neither.

`mica-runkit`, invoked as `init`, runs from the signed UKI or FIT. It validates the selected
`mos/deployment/v1` envelope against the public keys embedded in that kernel,
checks board/kernel associations, opens the authenticated SYSTEM and DATA
partitions, and creates signed dm-verity mappings for root and support. Kernel
modules and firmware come from the selected read-only support image before
systemd starts. Persistent machine identity is established on DATA before PID 1.

`mica-deploy` serializes mutations with the DATA transaction lock. Immutable
objects are verified and synced before the boot entry becomes visible. Native
UEFI/FIT trial records and authenticated retained descriptors drive state
reconciliation, confirmation, rollback and garbage collection.

## Device commands

```sh
mica-deploy status
mica-deploy booted
mica-deploy probe
mica-deploy check --source https://updates.example/v1/manifest.json --channel stable
mica-deploy fetch --source https://updates.example/v1/manifest.json --channel stable
mica-deploy import /path/to/update.mosupd
mica-deploy install /mos/updates/verified/DEPLOYMENT_ID.json \
  --objects /mos/updates/verified/objects
mica-deploy confirm
mica-deploy rollback
mica-deploy gc
mica-deploy firmware-readback
```

`reject ID` retires a deployment, and `fail-boot` handles the authenticated
running boot failure. `discard` clears the bounded acquisition workspace.
Commands derive their trust and partition policy from the authenticated boot;
there is no user-space trust override.

Online distribution uses signed `mos/catalog/v1` metadata with revision and
freshness checks. Offline `MOSUPD01` archives contain the same signed deployment
and bounded digest/length-addressed objects. The installer preserves current and
fallback objects when acquisition, capacity checks or publication fail.

Firmware is independently described by `mos/firmware/v1`. `firmware-readback`
checks installed bytes; an explicit receipt and `--record` durably record the
verified receipt. It never writes the loader. Host-side offline firmware
maintenance lives in `build/src/firmware-maintenance.ts`.

## Validation

`make os-rust-gate` runs the pinned Rust checks. `make os-file-transaction-faults`
executes native install/confirm/GC under process interruption and injected IO
failures. QEMU lifecycle and acquisition tests are under `tests/file-ab-x64/`,
with ARM64 selected explicitly. FIT policy and signature tests are under
`tests/file-ab-fit/`. VM and injected IO results do not substitute for physical
cx3576 storage power-cut acceptance.
