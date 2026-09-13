# 20260913-1935-workspace-convergence Merge mica-deploy in as crates and converge the repository into one crate workspace

- **status**: in_progress
- **priority**: P1
- **owner**: vtv87o8e/mica-core
- **createdAt**: 2026-09-13 19:35

## Description

User request, 2026-09-13: bring `/srv/ybolab/mica/mica-deploy` into this
repository as crates, converge the repository into one Cargo workspace managed
crate by crate, and keep scripts and code from being scattered across top-level
directories, following `/pma-rust`.

Acceptance (per the plan's phases): every crate under `crates/`, directory name
equal to package name; shell entry points under `scripts/`, packaging under
`packaging/deb/`; mica-deploy's `mica-deploy` and `lifecycle-sys` crates,
producers and gates in this workspace with their tests passing; package
payloads unchanged apart from the version stamp; `make check`, `make pool`,
`make package-gate` and the imported gates green; the pma-rust baseline gaps
closed or planned with named dependencies.

## ActiveForm

Converging the repository into one crate workspace

## Dependencies

- **blocked by**: (none; plan approved 2026-09-13 19:45 UTC, import source
  b698b10 handed over by the mica-deploy owner through coordinator a0psyi7e)
- **blocks**: mica-build's consumer pins for `mica-deploy` and `mica-lifecycle`

## Notes

- Plan: `docs/plan/20260913-1935-workspace-convergence.md`.
- Identity mapping: Cargo packages `micad` -> `mica-core`, `apid` -> `mica-apid`
  (directories follow); new packages `mica-deploy`, `lifecycle-sys`.
  Unchanged: executables `micad`, `apid`, `mica-deploy`, `mica-runkit`,
  `mica-mqttd`, `mica-mqtt-broker`, `mica-sftp-server`; Debian packages
  `micad`, `mica-apid`, `mica-mqttd`, `mica-mqtt-broker`, `mica-sftp-server`,
  `mica-deploy`, `mica-lifecycle`; units `micad.service`, `apid.service`;
  D-Bus `com.mica.micad`; every installed path; `micad --version` and
  `apid --version` output; the OpenAPI document.
- The old mica-deploy repository: the user allowed its deletion
  (2026-09-13 19:45 UTC); that belongs to its owner through the coordinator,
  not to this repository.

### 2026-09-13 user renames: the apid executable and the /mos directory

- User, in this issue: only `micad` keeps its executable name; `apid` becomes
  `mica-apid`. The link is `/usr/bin/mica-apid` -> `micad`, micad dispatches on
  `mica-apid` (plain `apid` is refused), `mica-apid --version` prints
  `mica-apid <version> (<commit>)`, `apid.service` runs `/usr/bin/mica-apid`.
  Unchanged: unit names `micad.service` and `apid.service`, the Debian packages,
  D-Bus `com.mica.micad` `/com/mos/micad` `com.mica.micad1`, the `apid` Unix
  account in the bus policy, the health and API keys (`apid` in `/healthz`,
  `source`/`daemon` values), the OpenAPI title, `APID_*` variables.
- User, in this issue: the `/mos` directory is `/mica`. Every `/mos` path in code,
  units, tests, UI text and the OpenAPI descriptions is `/mica` (216 references,
  43 files; `/mica/config`, `/mica/ui`, `/mica/diagnostics`, `/mica/updates`,
  `/mica/containers`, `/mica/home`, ...). Not paths and unchanged: the bind label
  `mos` in the storage status, the DATA pool subdirectory `mos`
  (`/mnt/data/mos`, reset's `SYSTEM_DIR`), the `mos` account, schema ids
  `mos/meta/v1` and `mos/fleet-config/v1`.
- The executable change was test-first: `tests/multicall.rs` asked for
  `mica-apid` and refused `apid` (RED), then passed.
- Retained: the retired mica-deploy's refs, bundles and release assets are at
  `.retained/mica-deploy/20260913/` (gitignored, verified against its manifest).

### 2026-09-13 user and base naming: standalone mica-apid, every mos name is mica

- User, in this issue: mica-apid is its own executable (like mica-sftp-server),
  so an apid upgrade never repacks micad. 20b7569: micad no longer links apid;
  packaging/deb/apid packs mica-apid alone. Test-first: tests/executable.rs in
  mica-core (micad answers as micad under any name) was RED, then GREEN.
- Base contract (mica-system-base 960ffe6, user instruction to base: every
  legacy mos name becomes mica, no compatibility): the D-Bus object is
  /com/mica/micad; environment variables MOSD_* are MICAD_*; the operator
  account is mica (home /home/mica); the provisioning file is
  mica-provisioning.toml; the DATA pool subdirectory is mica; the storage bind
  label is mica; token scheme, TLS names, UI keys, boot entry and EFI names,
  the `mica.recovery=` kernel parameter, the verity device mica-root, the
  U-Boot environment key mica_entries and the schema ids of the core-owned
  documents (meta, fleet-config, update-config, catalog, updates) are mica.
- Kept, as the signed update contract shared with mica-build: the schema ids
  mos/deployment/v1, mos/kernel/v1, mos/rootfs/v1, mos/update-catalog/v1,
  mos/update-envelope/v1, mos/firmware/v1, the component archive magic
  MOSUPD01 and tests/component-contracts/ (signed fixtures that mica-build
  produces and mirrors). Also kept: build-env's mos-<arch> buildx builders, and
  history in CHANGELOG files and docs.
