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
