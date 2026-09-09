# mos-deploy

Native boot and update tools for current signed file deployments. This workspace
contains `mos-init` and `mos-deploy`; it accepts no earlier disk, metadata or
package format.

`mos-init` runs from the signed UKI or FIT. It validates the selected
`mos/deployment/v1` envelope against the public keys embedded in that kernel,
checks board/kernel associations, opens the authenticated SYSTEM and DATA
partitions, and creates signed dm-verity mappings for root and support. Kernel
modules and firmware come from the selected read-only support image before
systemd starts. Persistent machine identity is established on DATA before PID 1.

`mos-deploy` serializes mutations with the DATA transaction lock. Immutable
objects are verified and synced before the boot entry becomes visible. Native
UEFI/FIT trial records and authenticated retained descriptors drive state
reconciliation, confirmation, rollback and garbage collection.

## Device commands

```sh
mos-deploy status
mos-deploy booted
mos-deploy probe
mos-deploy check --source https://updates.example/v1/manifest.json --channel stable
mos-deploy fetch --source https://updates.example/v1/manifest.json --channel stable
mos-deploy import /path/to/update.mosupd
mos-deploy install /mos/updates/verified/DEPLOYMENT_ID.json \
  --objects /mos/updates/verified/objects
mos-deploy confirm
mos-deploy rollback
mos-deploy gc
mos-deploy firmware-readback
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
