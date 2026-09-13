# 20260913-1935-workspace-convergence mica-deploy joins mica-core; one crate workspace

- **status**: in_progress
- **createdAt**: 2026-09-13 19:35
- **task**: `docs/task/20260913-1935-workspace-convergence.md`

## Context

User request (2026-09-13): merge `mica-deploy` into this repository as a crate,
converge the repository into one Cargo workspace managed crate by crate, and stop
mixing scripts and code across top-level directories; follow `/pma-rust`.

Today mica-core has nine member crates at the top level (`micad`, `apid`,
`micad-settings`, `busname`, `mqttd`, `broker`, `ui-bundle`, `mqtt-reference`,
`sftp-server`) beside `dist/`, `deb/`, `gate/`, `hack/`, `tools/`, `tests/`,
`tmp/`. mica-deploy (HEAD 14658a0, two commits ahead of its origin, owned by
another issue) is its own workspace: crates `mica-deploy` (root package, bin
`mica-deploy` plus `mica-runkit`) and `lifecycle-sys` (the one UAPI crate,
`unsafe_code = "deny"` by a dated decision), producers `deb/deploy` and
`deb/lifecycle` (packages `mica-deploy`, `mica-lifecycle`), gates `rust-gate`,
`boot-shutdown-test`, `file-transaction-faults`, and `tests/component-contracts/`
shared with mica-build.

## Target layout

Revision 2 (approved by the user 2026-09-13 19:45 UTC, with the crate renames
`apid` -> `mica-apid` and `micad` -> `mica-core` asked at 19:44 UTC): the
executables, Debian packages, units, D-Bus names and filesystem paths keep
their names; only the Cargo packages and their directories are renamed.

```text
Cargo.toml                 virtual workspace: members = ["crates/*"]
Cargo.lock  deny.toml  rustfmt.toml  clippy.toml  VERSION  Makefile
crates/
  mica-core/ (bin micad)  mica-apid/ (bin apid; ui/ inside)  micad-settings/  mica-busname/
  mica-mqttd/  mica-mqtt-broker/  mica-ui-bundle/  mica-mqtt-reference/
  mica-sftp-server/  mica-deploy/  lifecycle-sys/
  (each crate keeps its own dist/ units and tests/)
packaging/deb/<producer>/  micad mqtt sftp deploy lifecycle, plus copyright
scripts/                   every shell entry point, by purpose:
  gate/  (rust-gate, shell-lint, ui contract, boot-shutdown, file faults, dbus policy)
  build/ (build-deb, build-target, build-aarch64, check)
tools/deps.sh              stays: vendored helper at a fixed path in every repository
docs/
```

Directory name = package name for every crate. The Makefile stays the one
entry point that CI and build-env call.

## Phases

1. Layout only, no behaviour change: move the nine crates under `crates/`
   (git mv), units into their crates, `deb/` to `packaging/deb/`, `hack/`,
   `gate/` and `tests/dbus-policy-test.sh` to `scripts/`; fix every path in
   Cargo manifests, scripts, Dockerfiles, producer contexts and prose.
   Verify: `make check`, `make pool`, `make package-gate` byte-for-byte
   (package contents unchanged), `make dbus-policy-test`.
2. Import mica-deploy at a named commit, with its history, into
   `crates/mica-deploy` and `crates/lifecycle-sys`; merge dependencies into
   `[workspace.dependencies]`, deny.toml (the union of what is actually
   matched), its producers into `packaging/deb/`, its gates into `scripts/gate/`
   and the Makefile. Verify: the full test suite of both repositories, both
   producers' packages identical in payload to mica-deploy's own build of that
   commit, `package-gate` over seven packages, `boot-shutdown-test`,
   `file-transaction-faults`.
3. pma-rust baseline where it is not yet met, as its own reviewed step:
   deny-warnings policy moves from the gate's command line into
   `[workspace.lints]`, `#![forbid(unsafe_code)]` in every crate root except
   lifecycle-sys, rustfmt/clippy config, and the missing gates
   (cargo-shear, typos, MSRV check). The rust-check image carries none of the
   three tools today, so that part is a build-env request, and the stricter
   runtime lints (`unwrap_used`, `expect_used`, `panic`) are surveyed and
   planned rather than switched on in one go.

## Dependencies and risks

- mica-deploy is another issue's repository with unpublished commits: the
  import names one commit agreed with its owner, and that repository's
  retirement or freeze is the user's/owner's decision, not this repository's.
- Consumers move: mica-build pins `mica-deploy` and `mica-lifecycle` from
  `ghcr.io/ybolab/mica-deploy:pool.*`; after phase 2 they come from
  `ghcr.io/ybolab/mica-core:pool.*`. `tests/component-contracts/` divergence
  check in mica-build must read mica-core. Existing immutable artifacts stay.
- Record citations `mica-core:micad/...` in mica docs and mica-build change
  path; routed through the coordinator.
- One git stamp per pool: mica-deploy's packages take mica-core's version.
- Pending local work lands first (runtime commits, own-CI branch) so the move
  is a pure rename on top of it.

## Progress

- Phase 1 done: 1805e84 (layout), d8a8b55 (renames, `--bins` in
  build-deb.sh). Package payloads before and after: identical file lists,
  modes and control fields for all five packages on both architectures.
- Phase 2 done: 5a1bea2 merges b698b10dd6aaa023fa1b3896c5d0c40f03a35329
  (tree a922119d..., 18 commits) under import/mica-deploy/; 9c4187d moves it
  into place and drops the duplicated tooling. mica-deploy and mica-lifecycle
  match mica-deploy's own archives in files, modes and control fields;
  mica-runkit is static on both architectures.
- Phase 3 not started: kept to its own review, and no lint or toolchain scope is
  widened here.
