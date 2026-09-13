# Changelog

## 2026-09-13 03:30 [progress]

Created from `pkgs/mosd/` of `ybolab/mica-build` (163 commits kept through
`git subtree split`, then the tree at the Mica OS rename). Moved in with the
workspace: the Rust gate driver (`gate/rust-gate.sh`), the built-in UI's
build contract test (`gate/apid-ui-build-contract-test.sh`) and the D-Bus
policy test (`tests/dbus-policy-test.sh`). The API harness that boots the
assembled image stays in the assembly (`mica-build:tests/apid-api/`); for
its build-time half the `mica-apid` archive now ships
`/usr/share/mica-apid/openapi.json`. The substrate is the `mica-build-env`
source pin at `build-env/`; the four packages are published as
`build-<commit12>` and imported by `mica-build` through `deps/packages/`
(Phase 5 of `20260911-2006-split-package-repositories`).
