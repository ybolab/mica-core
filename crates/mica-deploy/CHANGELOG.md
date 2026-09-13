# Changelog

## 2026-09-13 02:40 [progress]

Created from `pkgs/mos-deploy/` of `ybolab/mica-build` (13 commits kept
through `git subtree split`, then the tree at the Mica OS rename). Moved in
with the workspace: the contract fixtures its integration tests include
(`tests/component-contracts/`), the native shutdown suite
(`gate/boot-shutdown-test.sh`, `gate/boot-shutdown/`), the IO fault suite
(`gate/file-ab-faults/`) and the Rust gate driver (`gate/rust-gate.sh`).
New here: the `mica-lifecycle` producer (`deb/lifecycle/`), which packs
the static `mica-init` and `mica-shutdown` the assembly used to compile
ad hoc for its kernel component; the substrate as the `mica-build-env`
source pin at `build-env/`; the release workflow that publishes the pool
as `build-<commit12>` (Phase 4 of `20260911-2006-split-package-repositories`).
