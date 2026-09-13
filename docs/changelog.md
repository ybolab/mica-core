# Changelog

## 2026-09-13 21:20 [progress]

mica-apid is its own executable and producer; micad no longer carries apid.
Every legacy mos name in this repository is mica, with no compatibility: the
D-Bus object /com/mica/micad, MICAD_* variables, the mica account, the pool
subdirectory, boot entry, verity and U-Boot names, and the core-owned schema
ids. The signed update contract shared with mica-build (mos/deployment,
kernel, rootfs, update-catalog, update-envelope, firmware schema ids and the
MOSUPD01 archive magic) waits for mica-build's re-signed fixtures. Not yet
published.

## 2026-09-13 20:45 [progress]

The apid executable is `mica-apid` (`/usr/bin/mica-apid`, a link to `micad`;
`micad` keeps its name) and the system DATA namespace is mounted at `/mica`
instead of `/mos`, both on the user's request. Units, packages, D-Bus names
and API keys are unchanged. Not yet published.

## 2026-09-13 20:16 [progress]

One crate workspace (`20260913-1935-workspace-convergence`, phases 1 and 2):
every crate under `crates/`, the producers under `packaging/deb/`, the shell
entry points under `scripts/build/` and `scripts/gate/`. The Cargo packages
`micad` and `apid` are now `mica-core` and `mica-apid`; executables, Debian
packages, units, D-Bus names and installed paths are unchanged. mica-deploy
joined with its history (b698b10dd6aa, merged unchanged, then moved into
`crates/mica-deploy`, `crates/lifecycle-sys`, `packaging/deb/{deploy,lifecycle}`
and `scripts/gate/`), so the pool has seven packages; `mica-runkit` is built
alone on the static route. Not yet published.

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
