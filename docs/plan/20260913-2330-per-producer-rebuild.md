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

The narrower option A below removes the env-only and docs-only rebuilds
without any new package mechanism; option B is needed only for commits whose
effective changes reach a strict subset of producers.

## The current candidate is a justified full rebuild

Every package's effective inputs changed since the last publication
(74235c3): the workspace convergence and mica-deploy import, the mica names,
mica-apid as its own executable, and the toolchain route itself (local images
replaced by the published `IMAGE_MICA_BUILD_RUST` and `IMAGE_MICA_BUILD_BASE`
packer). The 74235c3 pool holds four pre-convergence packages, so none of
them can be reused. The v0.0.1 -> v0.0.2 pin changed no image digest and is
not by itself a reason to rebuild.

## Option A (smallest): skip pool build and publication when no effective input changed

One decision step at the start of `release.yml`, fail-closed:

1. The base is the newest commit that is an ancestor of HEAD and has a
   published `pool.<arch>.build-<commit12>` in this repository's package
   (anonymous tags list and manifests, as publish.sh already reads them).
   No base found: full pool.
2. `git diff --name-only <base>..HEAD` is classified against an explicit
   allow-list of paths that are not package inputs: `docs/**`, `*.md` at
   any depth, `.gitignore`, and `deps/build-env.json`. Any other path, a
   workflow change included: full pool, exactly as today.
3. When `deps/build-env.json` changed, both pins are fetched and verified
   (`scripts/build/build-env.sh`), and the pool is skipped only when
   `IMAGE_MICA_BUILD_RUST` and `IMAGE_MICA_BUILD_BASE`, the images this
   repository actually uses, resolve to the same digests in both
   `images.env`. Any difference, or a pin that does not verify: full pool.
4. Skipped: no `make check`, `make pool`, `make package-gate` or
   `make publish`; the step prints the base commit and the reason. The commit
   has no pool; consumers keep the rows of the base commit, whose archives
   carry their real provenance. Whenever a pool is built, every existing
   full-pool gate and identity check applies unchanged.

This satisfies the immediate rule: a docs-only commit or a release-pin
version bump with identical effective digests rebuilds nothing. The
allow-list holds only while no build reads those paths; today no crate
`include_str!`s a markdown file and no producer copies one, and a path that
becomes an input leaves the allow-list in the same commit. It
deliberately leaves to option B every commit that touches a runtime,
packaging or build-script path of only some producers (for example the sftp
crate alone), and any commit changing `Cargo.lock`, the workspace
`Cargo.toml`, `VERSION`, `scripts/` or an image digest: those reach every
producer (each compiles in the Rust image and packs in the base image) and
stay full rebuilds under both options.

## Option B (larger): per-producer selection with partial pools

Only for changes that reach a strict subset of producers:

1. Each `producer.env` declares `INPUTS`, the repository paths its archives
   are built from; a test fails when a crate a producer builds reaches a path
   outside them. `scripts/deb/build.sh` and `producers.sh` accept the key
   (they read plain `KEY=value` today and use only the keys they know).
2. CI resolves, per producer, the newest published pool that contains its
   packages and whose commit is an ancestor of HEAD, and rebuilds the
   producer when its `INPUTS` or an image digest it uses changed since then.
   This is new reader code in this repository's CI: it lists tags, reads
   manifests and matches layer titles to `PACKAGES`.
3. `Makefile` `pool`/`deb` build the selected producers instead of a fixed
   list; `package-gate.sh` gets a selected-producers mode (check h, ENABLEMENT
   and the reproducibility rebuild over the selected producers only).
   `publish.sh` needs no change: it already publishes whatever archives are
   in the pool, refusing any not built at HEAD, so a partial pool is published
   under its own commit and nothing is relabelled.
4. Exact pins bind groups: `mica-apid`, `mica-mqttd` and `mica-mqtt-broker`
   declare `micad (= @VERSION@)`, so the micad group is selected together.
   `mica-sftp-server`, `mica-deploy` and `mica-lifecycle` depend on no package
   built here and are selectable alone. This does not make mica-apid
   upgradable on its own; that is the separate, pending interface-revision
   proposal with its closure check.

Consumer representation needs no new schema. `deps/packages/<name>.json`
already records a `commit` per package (RULES.md 6), and `lock.sh --bump
<component> --tag build-<c12> --package <p> ...` locks named packages from
one pool. Real limits of the current consumer scripts, read in mica-build at
1584825a, none of which this repository changes:

- `lock.sh --bump` without `--package` removes the component's rows the
  named pool no longer provides, so a partial pool must always be locked
  with `--package`; with no `--tag` it takes the newest pool by `created`,
  which may be partial.
- `source.sh <component>` refuses a component whose rows name two commits
  unless `MICA_SOURCE_COMMIT` is set; its one caller today is
  `tests/lifecycle-uefi/early-hang-init.sh` (component `mica-deploy`).
- The consumer gate fails an exact pin across two lock origins, so the micad
  group's rows must name one commit, which item 4 guarantees.

## Not covered

- This repository copies no `fetch.sh`/`source.sh` and reads no pool into its
  own; neither option needs it to.
- Consumer adoption, image and guest acceptance are paused and separate.

## Decision needed

Option A as the build-selection rule for env-only and docs-only commits;
option B only if subset rebuilds are wanted on top of it.
