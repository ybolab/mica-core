## Project Development

This repository follows the PMA workflow. The actual rules live in the `/pma`
skill and the stack skill below — do not duplicate them here. If a rule in
this file ever conflicts with `/pma`, treat `/pma` as the source of truth and
update this file.

### Skill stack

- `/pma` — workflow control, three-phase gate, task and plan tracking
- `/pma-rust` — the workspace at the root (`mica-deploy`, `lifecycle-sys`)

The packaging (`deb/`), the gate drivers (`gate/`) and `hack/` are bash and
Dockerfiles; `/pma`'s *Delivery* rules apply to them directly.

### Triggers

Any feature, bug fix, refactor, planning, progress tracking, or multi-agent
execution goes through `/pma` (investigate → proposal → implement). Ceremony
is tiered by complexity per `/pma` *Task Tiers*: only trivial changes take
the fast path; everything else waits for explicit approval such as `proceed`.

### Project-specific facts

- Primary language / runtime: Rust `1.96` (`Cargo.toml` `rust-version`), compiled in the pinned `mica-build-rust` image; the host carries no cargo
- The build substrate is `build-env/`, the `mica-build-env` source pin (`deps/sources/mica-build-env.json`, fetched by `make deps`); `hack/build-deb.sh` and the producers under `deb/` derive the repository as their own root and refuse a missing substrate by name
- Products: two Debian packages per architecture, `mica-deploy` (the device-side client, `/usr/bin/mica-deploy`) and `mica-lifecycle` (the static `mica-runkit`, reached as `init` and `shutdown`, under `/usr/lib/mica/lifecycle/`, read by the assembly's kernel component and never installed into a root), versioned `VERSION+git<commit12>-1` and published as the GitHub Release `build-<commit12>` of `ybolab/mica-deploy`; the assembly (`mica-build`) imports both through `deps/packages/`
- `tests/component-contracts/` is the contract between the assembly's component producer (`mica-build:build/`) and this crate's reader; the assembly keeps the same files and its `make os-pool` refuses a divergence from the copy at the pinned commit
- Quality gates: `make check` (lint, `rust-gate`, `boot-shutdown-test`, `file-transaction-faults`); `make pool` then `make package-gate` over the built archives
- Build resources: no fixed CPU, memory or job quotas; use the host and tool defaults

### Documentation entry points

- Tasks: `docs/task/index.md`
- Plans: `docs/plan/index.md`
- Changelog: `docs/changelog.md`
- Design and project management for the whole of Mica OS: `ybolab/mica`
