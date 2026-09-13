# 20260913-2330-per-producer-rebuild PROPOSAL: rebuild and publish only the producers whose inputs changed

- **status**: proposed (not implemented)
- **createdAt**: 2026-09-13 23:30
- **task**: `20260913-2200-build-env-release-v0.0.1` (notes)

## Steering

User, via coordinator a0psyi7e, 2026-09-13 22:43: each package only has to
agree with its own declared, verified build environment and published
provenance; an env release update alone is not a rebuild trigger; unchanged
archives stay pinned and reused with their original provenance.

## The trigger today

- `.github/workflows/release.yml` runs `make pool` and `make publish` on every
  push to `main`, whatever changed: all six producers, both architectures,
  fourteen archives.
- `scripts/deb/version.sh` stamps every archive `VERSION+git<HEAD commit12>-1`,
  and `scripts/deb/publish.sh` refuses an archive whose `Mica-Source-Commit`
  is not HEAD. So a commit that changes only `deps/build-env.json` (or only
  docs) gives every package a new version and a new pool even when its bytes
  would otherwise be the same.
- `scripts/deb/package-gate.sh` check h requires every producer to contribute
  its packages, so a partial pool fails the gate.

No smaller change removes this: skipping a producer needs a rule for which
inputs are its own, and a gate mode for a partial pool.

## The current candidate is a justified full rebuild

Every package's effective inputs changed since the last publication
(74235c3): the workspace convergence and mica-deploy import, the mica names,
mica-apid as its own executable, and the toolchain route itself (local images
replaced by the published `IMAGE_MICA_BUILD_RUST` and `IMAGE_MICA_BUILD_BASE`
packer). The 74235c3 pool holds four pre-convergence packages, so none of
them can be reused. The v0.0.1 -> v0.0.2 pin changed no image digest and is
not by itself a reason to rebuild.

## Proposal

1. Each `producer.env` declares `INPUTS`, the repository paths its archives
   are built from (its crates and their path dependencies, `Cargo.toml`,
   `Cargo.lock`, `VERSION`, its `packaging/deb/<producer>/` and dist
   directories, `scripts/build/`, `scripts/deb/`). A test fails when a
   crate a producer builds reaches a path outside its `INPUTS`.
2. CI resolves, per producer, the last commit whose pool this repository
   published with that producer's packages, and rebuilds the producer when
   `git diff --name-only <that commit>..HEAD` touches its `INPUTS` or when an
   image it builds in (`IMAGE_MICA_BUILD_RUST`, `IMAGE_MICA_BUILD_BASE`)
   resolves to another digest. A pin whose version changed and whose
   digests did not triggers nothing.
3. The pool of a commit holds only the archives rebuilt at that commit,
   stamped and attributed to it; `package-gate.sh` gains a selected-producers
   mode where check h covers the selected producers only. Nothing is
   relabelled: a reused archive stays in the pool of the commit that built it,
   and a consumer's `deps/packages/` row names that commit.
4. Exact pins bind groups. `mica-apid`, `mica-mqttd` and `mica-mqtt-broker`
   declare `micad (= @VERSION@)`, so the micad group rebuilds together while
   those pins stand (the interface-revision proposal, still pending, is what
   would loosen `mica-apid` alone). `mica-sftp-server`, `mica-deploy` and
   `mica-lifecycle` have no dependency on another package built here and can
   be selected on their own.

## Not covered

- Consumers: `mica-build` must accept rows for one repository at several
  commits; that is its reader, not this repository's.
- This repository copies no `fetch.sh`/`source.sh` and reads no pool, so it
  cannot assemble reused archives into a pool of its own; item 3 avoids that.

## Decision needed

Accept items 1-4 as the build-selection rule for this repository before any
of it is implemented.
