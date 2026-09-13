## Project Development

This repository follows the PMA workflow. The actual rules live in the `/pma`
skill and the stack skills below — do not duplicate them here. If a rule in
this file ever conflicts with `/pma`, treat `/pma` as the source of truth and
update this file.

### Skill stack

- `/pma` — workflow control, three-phase gate, task and plan tracking
- `/pma-rust` — the workspace under `crates/` (`mica-core`, which builds the `micad` executable; `mica-apid`, which builds the `mica-apid` executable; `mica-mqttd`, `mica-mqtt-broker`, `micad-settings`, `mica-busname`, `mica-ui-bundle`, `mica-mqtt-reference`, `mica-sftp-server`, `mica-deploy`, which builds `mica-deploy` and the static `mica-runkit`; `lifecycle-sys`, the one crate allowed `unsafe` by its dated decision)
- `/pma-web` — `crates/mica-apid/ui/` (React + Vite, embedded into apid)

Every crate is under `crates/<package name>/`. The packaging
(`pkgs/`) and the shell entry points (`scripts/gate/`, `scripts/build/`) are bash and Dockerfiles; `/pma`'s *Delivery*
rules apply to them directly.

### Triggers

Any feature, bug fix, refactor, planning, progress tracking, or multi-agent
execution goes through `/pma` (investigate → proposal → implement). Ceremony
is tiered by complexity per `/pma` *Task Tiers*: only trivial changes take
the fast path; everything else waits for explicit approval such as `proceed`.

### Project-specific facts

- Primary language / runtime: Rust `1.96` (`Cargo.toml` `rust-version`), compiled in the published `IMAGE_MICA_BUILD_RUST`; the UI in the bun of `IMAGE_MICA_BUILD_BASE`, which also packs the archives; both pinned by digest in `build-env/images.env`; the host carries no cargo
- The build substrate is the `mica-build-env` release pinned in `deps/build-env.json` (version and the sha256 of its `SHA256SUMS`), fetched and verified into the gitignored `build-env/` by `make deps` (`scripts/build/build-env.sh`); this repository runs only its own scripts (`scripts/build/from.sh`, `scripts/deb/`, owned copies of the release reference implementation per its `RULES.md`), and `scripts/build/build-deb.sh`, `scripts/build/build-target.sh`, `scripts/build/check.sh` and the producers under `pkgs/` derive the repository as their own root and refuse a missing `build-env/images.env` by name
- Products (repository mica-core): seven Debian packages per architecture -- `micad` (`/usr/bin/micad`), `mica-apid` (its own `/usr/bin/mica-apid` executable from its own producer, plus `/usr/share/mica-apid/openapi.json`), `mica-mqttd`, `mica-mqtt-broker`, `mica-sftp-server` (`/usr/lib/sftp-server`, the SFTP server dropbear runs; a board feature that boards select, not a dependency of the base system), `mica-deploy` (the device-side client, `/usr/bin/mica-deploy`), `mica-lifecycle` (the static `mica-runkit`, reached as `init` and `shutdown`, under `/usr/lib/mica/lifecycle/`, read by the assembly's kernel component and never installed into a root) -- versioned `VERSION+git<commit12>-1` (every crate's `[package] version` must equal `VERSION`; `scripts/build/check.sh` asserts it) and published by this repository's CI as the public OCI artifacts `ghcr.io/ybolab/mica-core:pool.<arch>.build-<commit12>`; the assembly (`mica-build`) imports them through `deps/packages/`
- The API harness that boots the assembled image and drives apid over a socket lives in the assembly (`mica-build:tests/apid-api/`); its build-time half pins phase literals against the OpenAPI document the `mica-apid` archive ships
- Database / storage: none -- micad persists to `DATA/state` and `DATA/meta` as files (`mica:docs/design/`)
- `crates/mica-deploy/tests/component-contracts/` is the contract between the assembly's component producer (`mica-build:build/`) and mica-deploy's reader; the assembly keeps the same files and refuses a divergence from the copy at the pinned commit
- Quality gates: `make check` (lint, `pool-decision-test`, `apid-ui-build-contract-test`, `rust-gate`, `boot-shutdown-test`, `file-transaction-faults`); `make dbus-policy-test` where a `dbus-daemon` exists; `make pool` then `make package-gate` over the built archives; CI builds and publishes a pool only when `scripts/build/pool-decision.sh` finds a package, build or workflow input or an effective image digest changed since the nearest complete published pool
- Build resources: no fixed CPU, memory or job quotas; use the host and tool defaults

### Documentation entry points

- Tasks: `docs/task/index.md`
- Plans: `docs/plan/index.md`
- Changelog: `docs/changelog.md`
- Design and project management for the whole of Mica OS: `ybolab/mica`
