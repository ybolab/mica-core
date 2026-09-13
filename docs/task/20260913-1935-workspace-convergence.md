# 20260913-1935-workspace-convergence Merge mica-deploy in as crates and converge the repository into one crate workspace

- **status**: pending
- **priority**: P1
- **owner**: (unassigned)
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

- **blocked by**: approval of `docs/plan/20260913-1935-workspace-convergence.md`;
  the import commit agreed with the mica-deploy owner (through coordinator a0psyi7e)
- **blocks**: mica-build's consumer pins for `mica-deploy` and `mica-lifecycle`

## Notes

- Plan: `docs/plan/20260913-1935-workspace-convergence.md` (draft, awaiting approval).
