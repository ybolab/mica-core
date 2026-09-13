# Package producers for the micad workspace

A **producer** is a subset of this Cargo workspace compiled and packaged on its
own. It has its own crate list, its own `CARGO_TARGET_DIR`, its own control
templates and its own archives, and it emits nothing outside that set:

| Producer | Compiles | Emits |
| --- | --- | --- |
| `micad` | `micad`, `apid` | `micad`, `mica-apid` |
| `mqtt` | `mica-mqttd`, `mica-mqtt-broker` | `mica-mqttd`, `mica-mqtt-broker` |
| `sftp` | `mica-sftp-server` | `mica-sftp-server` (`/usr/lib/sftp-server`) |

Each is built by the repository's one generic driver, which discovers them:

```
make os-deb-micad
  -> bash build-env/deb/build.sh --producer micad --arch <amd64|arm64>
  -> _out/debs/<arch>/pool/<package>_<version>_<arch>.deb
```

A producer is a directory holding a `producer.env` and a `Dockerfile`;
`build-env/deb/README.md` is that convention, and `make os-debs` builds every
producer discovered anywhere in the tree. `bash build-env/deb/repo.sh --arch
<arch>` then indexes the pool, which is shared: a producer deletes only its own
archives from it.

## Why the split

The workspace builds four binaries and the image wants them in four packages
with four different dependency stories. Building all four to package two makes
"this package's build produced that binary" a fact about the whole workspace
rather than about the package -- so a producer's independence is asserted and
not merely intended: after the compile, `build-deb.sh` fails by name if any
binary the producer does not own is present in its target directory. That is
also why the target directory is producer-private
(`target-deb/<producer>/`, gitignored) rather than shared with
`hack/build-target.sh`'s `target/`: a shared one would let a
four-binary build satisfy the assertion with binaries nobody asked for.

## Adding a producer

Create `deb/<producer>/` holding `producer.env`, a `Dockerfile`,
`control/<package>.control` per package, and a `prepare.sh` naming the crates it
compiles. Nothing else: there is no register to add a case to, and no `make`
target to write -- `make os-deb-<producer>` is a pattern rule resolved against
`build-env/deb/producers.sh`, and `make os-debs` loops over the same list.
The `producer.env` keys are documented in `build-env/deb/README.md`.

### Why `hack/build-deb.sh` still exists

It SURVIVED the move to the generic driver as this workspace's `PREPARE` hook
implementation, and as nothing else: it cross-compiles a named crate set and
then asserts that the producer boundary held, which is a claim about a `cargo`
build that no key in `producer.env` could describe. It no longer knows what a
package is, what a pool is, what version anything carries or how to run buildx;
`build-env/deb/build.sh` owns all of that for every producer in the
repository. Each producer reaches it through its own `prepare.sh`, which is
where the crate list lives.

The unit files reach the Dockerfile through the `BUILD_CONTEXTS` each
`producer.env` names, because they are not all in one directory: `micad.service`
and `apid.service` are in `dist/`, while `mica-mqttd.service` and
`mica-mqtt-broker.service` sit beside their own crates in `mqttd/dist/` and
`broker/dist/`. The `mqtt` producer therefore takes two of them,
`mqttd-dist` and `broker-dist`, rather than one pointed at their only common
parent -- which is the workspace root, cargo target trees and all. The shared
`copyright` arrives the same way, as `family`, because the build context is now
each producer's own directory.

`copyright` is shared by every package of every producer here and is written
once, in this directory. It is installed per package as
`/usr/share/doc/<package>/copyright`, which is what keeps two packages from
owning one path.

## The two routes

The compile runs at **amd64 for both architectures**: `cargo` cross-compiles,
so it is a plain `docker run` against `localhost/mica-build-rust:amd64` with
`--target x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu`, and needs
no buildx and no emulation.

The packaging runs at the **target** architecture, because `dpkg-shlibdeps`
resolves an ELF's dependencies against the libraries of the container it runs
in; `build-env/deb/pack.sh` refuses the mismatch rather than recording
amd64's versions in an arm64 package. This host has no binfmt registration, so
`docker run --platform linux/arm64` is not a route -- it dies with `exec format
error`. `docker buildx build --platform linux/arm64` on the `mos-arm64`
docker-container builder is, because its buildkit image bundles the emulators.
That builder cannot resolve a `localhost/*` tag, so the base is handed over as
an OCI layout by `build-env/from.sh --contexts=`, exactly as
`pkgs/rauc/build.sh` does it.

`pack.sh` is not baked into `mica-build-deb`. Each producer Dockerfile takes it
through the `packer` named build context, which is what lets an edit to the
packer take effect without rebuilding the builder family.

## Version

```
<crate version>+git<commit>-1          e.g. 0.1.0+git9671c7cf2d4d-1
<crate version>+git<commit>.dirty-1    when the tree is not clean
```

Computed by `build-env/deb/version.sh`, which is the one implementation of
this rule for every producer in the repository -- including the ones that are
not Rust and have no manifest to read a number out of. `<crate version>` comes
from this workspace's crate manifests, which must all agree, and is never
written in this tree twice; `<commit>` is `git rev-parse --short=12 HEAD`. The
`-1` is the Debian revision; these packages have no upstream/downstream split,
so it does not move.

## `SOURCE_DATE_EPOCH`

Derived on the host as `git log -1 --format=%ct` and passed into the packaging
stage as a build argument. `pack.sh` requires it, sets every payload mtime to
it and hands it to `dpkg-deb`; it has no "now" default, because one would make
every archive irreproducible while every build stayed green.

A **dirty tree keeps the same commit timestamp**. The version already says
`.dirty`, so the archive is marked as one no commit reproduces; taking `now`
instead would additionally make two dirty builds of one tree differ from each
other, and that is the property worth keeping.

## Enablement is package-owned

A package that ships a unit ships the `multi-user.target.wants` symlink that
starts it, as a **file in its payload**. `micad` owns
`/etc/systemd/system/multi-user.target.wants/micad.service` and `mica-apid` owns
`apid.service`. Installing the package is what makes the daemon run.

No maintainer script is involved and nothing calls `systemctl enable` --
both control archives hold `control` and `md5sums` and nothing else. A unit
enabled by a script is enabled by something you have to run to see; a symlink
in the archive is visible to `dpkg-deb --contents` and to every gate that reads
one.

The links are **not** conffiles. They sit under `/etc`, where dpkg would
normally expect configuration a user edits and wants preserved across upgrades,
but this root filesystem is an immutable dm-verity squashfs and nothing in it is
edited. A `DEBIAN/conffiles` entry would promise a merge that cannot happen.

A package that must NOT start on its own ships no such link, and the `mqtt`
producer is that case: `micad` renders `/run/mica/mqtt-broker.toml` and
`/run/mica/mqttd-device.env` from the settings tree and starts both units from
`mqtt.enabled`, so a symlink in either payload would start a broker nobody
asked for, before micad has rendered anything for it to read. Both units keep
their `[Install]` section so `systemctl enable` stays meaningful on a writable
root; neither postinst calls it.

## Maintainer scripts

`mica-mqttd` and `mica-mqtt-broker` each carry a `postinst` that creates their
pinned service account (uid/gid 970 and 969). The account moved here out of the
retired rootfs account scripts, into the package that owns it. `postinst` and
not `preinst`, because no path in either payload is owned by those accounts:
they are needed when the unit starts, not when the files are unpacked.

Those rootfs scripts hard-failed when the uid already existed, which is right
for a once-per-build script and wrong for a maintainer script -- a postinst runs
again on every upgrade and reinstall. The packaged form accepts an existing
account that is exactly the pin and still fails, by name, on one held by
anybody else. `useradd`, `groupadd` and `chage` come from `passwd`, which both
packages declare in `Depends`: `dpkg-shlibdeps` cannot see a program `exec`ed
by name.

## `micad` depends on `mica-system`

`micad.service` declares `RequiresMountsFor=/var/lib/mica /mos`, and both paths
are bind-mount targets: `var-lib-mica.mount` and the `/var/lib/mica` mountpoint
directory are two halves of one mechanism, as are `mos.mount` and `/mos`, so
one package owns all four. That package is `mica-system`
(`rootfs/packages-src/system`), and `micad` names it in `Depends` rather than
shipping the directories itself.

`/mos` joined that line with PLAN-070 §5.2: system configuration lives in
`/mos/config` on DATA, so a micad that started before the mount would come up on
schema defaults. The ordering also puts micad after `mica-data-layout.service`,
which runs `Before=mos.mount` and is what creates `/mos/config` at its declared
`0700`.

The dependency is UNVERSIONED -- `mica-system` is not built from this
workspace's commit and pins nothing to it -- and it is declared once. `mica-apid`
inherits it through its exact-version dependency on `micad`, and so do both MQTT
packages; no other control template mentions it.

`mica-system` is not in this pool and will not be until that workstream lands.
That is expected: it is an EXTERNAL name to these producers, the same as
`passwd`, and the gate below classifies it as one. Installing these four
packages into a clean root is therefore the composer's check and not this
directory's.

## The package gate

```
make os-debs              # every producer, both architectures, then both indexes
make os-deb-package-gate  # bash build-env/deb/package-gate.sh
```

The gate reads the built pools and asserts, out of the archives themselves:
unique non-directory file ownership across the four packages with no `Replaces`
escape; the fields and the `Depends` closure, with every local dependency
pinned to the exact version the pool was built at; a non-empty
`/usr/share/doc/<package>/copyright` in each; the enablement asymmetry
described above -- one `multi-user.target.wants` symlink in each micad-family
payload, none in either MQTT payload; no `DEBIAN/conffiles`; and `sh -n` over
every maintainer script, which the pipefail lint does not cover because these
are `#!/bin/sh` and never enable it.

It also rebuilds one producer per architecture and requires byte-identical
archives. That rebuild runs on a buildx builder the gate CREATES, whose cache is
empty by construction: a second build on the normal builder replays the cached
packing layer and re-exports the same bytes, which would prove the export is
deterministic and nothing at all about `pack.sh`.
