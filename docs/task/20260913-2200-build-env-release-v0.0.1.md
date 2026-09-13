# 20260913-2200-build-env-release-v0.0.1 Build on the mica-build-env v0.0.1 release

- **status**: in_progress
- **priority**: P1
- **owner**: vtv87o8e/mica-core
- **createdAt**: 2026-09-13 22:00

## Description

Coordinator handoff (a0psyi7e), 2026-09-13: mica-build-env v0.0.1 is released
and verified. Migrate this repository's release pin, its Rust and base image
callers and its own build, package and CI scripts to that release, following
its `RULES.md`; implement the straightforward pin and caller migration and
keep any new compatibility policy reviewable before it is applied.

Acceptance: `deps/build-env.json` records the current release and the sha256 of its
`SHA256SUMS`; `make deps` refuses assets that do not match; no caller names a
`LOCAL_MICA_BUILD_*` image; the scripts that run are this repository's own;
`make check`, `make pool` and `make package-gate` pass in the published images
pinned by digest.

## ActiveForm

Building on the mica-build-env v0.0.1 release

## Dependencies

- **blocked by**: (none; release verified by the coordinator)
- **blocks**: the next own-CI publication of the pool

## Notes

- 2026-09-13 23:10: pin moved to v0.0.2 (coordinator delta; tag 7211badc4462,
  SHA256SUMS b75932fc6df3). RULES.md and all four IMAGE_MICA_BUILD_* digests are
  unchanged, so the v0.0.1 gate results stand; only the pin route was re-checked.
- The independent mica-apid upgrade policy is a proposal only:
  `docs/plan/20260913-2230-independent-apid-upgrade.md`, acceptance case
  `scripts/gate/interface-dependency-test.sh` (not wired into `make check`).
