## Project Development

This repository follows the PMA workflow. The actual rules live in the `/pma`
skill and the stack skills below — do not duplicate them here. If a rule in
this file ever conflicts with `/pma`, treat `/pma` as the source of truth and
update this file.

### Skill stack

- `/pma` — workflow control, three-phase gate, task and plan tracking
- `/pma-rust` — the workspace at the root (`micad`, `apid`, `mica-mqttd`, `mica-mqtt-broker`, `micad-settings`, `mica-busname`, `mica-ui-bundle`, `mica-mqtt-reference`)
- `/pma-web` — `apid/ui/` (React + Vite, embedded into apid)

The packaging (`deb/`), the gate drivers (`gate/`), `hack/` and
`tests/dbus-policy-test.sh` are bash and Dockerfiles; `/pma`'s *Delivery*
rules apply to them directly.

### Triggers

Any feature, bug fix, refactor, planning, progress tracking, or multi-agent
execution goes through `/pma` (investigate → proposal → implement). Ceremony
is tiered by complexity per `/pma` *Task Tiers*: only trivial changes take
the fast path; everything else waits for explicit approval such as `proceed`.

### Project-specific facts

- Primary language / runtime: Rust `1.96` (`Cargo.toml` `rust-version`), compiled in the pinned `mica-build-rust` image; the UI in the bun of `mica-build-base`, pinned by digest as `IMAGE_MICA_BUILD_BASE` in `build-env/images.env`; the host carries no cargo
- The build substrate is `build-env/`, the `mica-build-env` source pin (`deps/sources/mica-build-env.json`, fetched by `make deps`); `hack/build-deb.sh`, `hack/build-target.sh`, `hack/check.sh` and the producers under `deb/` derive the repository as their own root and refuse a missing substrate by name
- Products (repository mica-core): four Debian packages per architecture -- `micad`, `mica-apid` (which also ships `/usr/share/mica-apid/openapi.json`), `mica-mqttd`, `mica-mqtt-broker` -- versioned `VERSION+git<commit12>-1` (every crate's `[package] version` must equal `VERSION`; `hack/check.sh` asserts it) and published as the GitHub Release `build-<commit12>` of `ybolab/micad`; the assembly (`mica-build`) imports them through `deps/packages/`
- The API harness that boots the assembled image and drives apid over a socket lives in the assembly (`mica-build:tests/apid-api/`); its build-time half pins phase literals against the OpenAPI document the `mica-apid` archive ships
- Database / storage: none -- micad persists to `DATA/state` and `DATA/meta` as files (`mica:docs/design/`)
- Quality gates: `make check` (lint, `apid-ui-build-contract-test`, `rust-gate`); `make dbus-policy-test` where a `dbus-daemon` exists; `make pool` then `make package-gate` over the built archives
- Build resources: no fixed CPU, memory or job quotas; use the host and tool defaults

### Documentation entry points

- Tasks: `docs/task/index.md`
- Plans: `docs/plan/index.md`
- Changelog: `docs/changelog.md`
- Design and project management for the whole of Mica OS: `ybolab/mica`
