# The Debian package scripts

This repository's own copy of the `deb/` scripts of mica-build-env v0.0.1
(v0.0.2 changes only the `lock.sh` grep this copy already carries),
which RULES.md names as the reference implementation of its package rules.
The packing image is the published `IMAGE_MICA_BUILD_BASE` of that release,
pinned by digest in the verified `build-env/images.env`
(`scripts/build/build-env.sh`); no image is built here. `fetch.sh` and
`source.sh` were not copied: this repository imports no pool and locks no
source, so the sections below that describe them are the reference text only.

These are the contract every package producer in this repository is written
against:

| File | Runs | Produces |
| --- | --- | --- |
| `pack.sh` | inside `IMAGE_MICA_BUILD_BASE`, at the **target** architecture | one `.deb` |
| `repo.sh` | on the host | `Packages`, `SHA256SUMS`, `manifest.txt` |
| `producers.sh` | on the host | the producer set, discovered from the tree |
| `build.sh` | on the host | one producer's archives, for one architecture |
| `version.sh` | on the host | the one version the whole pool carries |

The image carries `dpkg-dev` and no Rust compiler. What a package contains is
produced by the pinned language builder (`IMAGE_MICA_BUILD_RUST`); this image
only wraps an already-staged tree. It records what it resolved in
`/etc/mica-build/base.env`, which is how a producer answers "what packaged
this".

## The producer convention

A **producer** is one directory. The marker is the PAIR `Dockerfile` +
`producer.env`, and it is looked for **anywhere in the tree** -- producers live
under `pkgs/*/deb/*/`, `rootfs/packages-src/*/` and
`boards/<board>/deb/*/`, and a search rooted at any one of those silently
omits the others. A `Dockerfile` alone is not the marker: this tree holds a
dozen of those and none of the others is a package producer. `producer.env` is
what declares intent.

```
<anywhere>/<producer>/
  producer.env                   what this producer emits and how it is built
  Dockerfile                     stage the payload, then one pack.sh run per package
  control/<package>.control      one per package in PACKAGES
  <prepare hook>                 optional; see PREPARE
```

The producer's NAME is its directory's basename, and that is what
`make os-deb-<producer>` selects on, so two producers cannot share one. The
directory is also the build context: everything the `Dockerfile` needs from
elsewhere arrives through `BUILD_CONTEXTS`.

### `producer.env`

Plain `KEY=value`, the `boards/*/board.env` discipline: no logic, no command
substitution, safe to source and to parse.

| Key | Required | Meaning |
| --- | --- | --- |
| `PACKAGES` | yes | the Debian packages this producer emits, space separated |
| `ARCHES` | yes | `amd64`, `arm64`, or `all` -- see below |
| `ENABLEMENT` | yes | `<package>=<count>` per package; see below |
| `BUILD_CONTEXTS` | no | `<name>=<repo-relative path>`, passed as `--build-context` |
| `FROM_IMAGES` | no | `<build-arg name>=<images.env key>`, resolved by `scripts/build/from.sh`. The packer image is always supplied; `FROM_IMAGES` declares additional bases only |
| `BUILD_ARGS` | no | extra `<name>=<value>` build arguments |
| `PREPARE` | no | a script in the producer directory, run on the host before the build |
| `PREFLIGHT` | no | `1` if that hook honours `MICA_DEB_PREFLIGHT=1`; see below |
| `VERSION_FROM` | no | `<repo-relative env file>:<KEY>` naming the upstream version this producer repacks; see below |
| `FOR_EACH` | no | a repository-relative glob of plain `KEY=value` files; the producer runs once per match, a **matrix producer** -- see below |
| `CONTROL_DIR` | no | where the control templates are, repository-relative; default `<producer dir>/control` |

A **matrix producer** is one recipe over many instances: `FOR_EACH=boards/*/board.env`
makes `producers.sh` emit one row per match, named `<producer>@<instance>`
(the matched file's directory name), and the file's assignments are in the
environment when `producer.env` is sourced, so `PACKAGES="mica-board-${LAYOUT_BOARD}"`,
`ARCHES="${MICA_ARCH}"`, `BUILD_CONTEXTS="board=boards/${LAYOUT_BOARD}"` and
`CONTROL_DIR="boards/${LAYOUT_BOARD}/package/control"` name the instance's
own. `build.sh` and the `PREPARE` hook get `MICA_DEB_INSTANCE` (the
instance) and `MICA_DEB_INSTANCE_ENV` (the file), and the Dockerfile the
build argument `MICA_DEB_INSTANCE`. `producers.sh --instance-for` and
`--control-for` resolve a row to its file and its templates; `tests/producers-test.sh`
covers the discovery.

`VERSION_FROM` is for producers that repack **upstream software** (podman,
RAUC). The named key's value -- a leading `v` is stripped -- replaces the
workspace prefix in the version, so the archive is
`<upstream>+git<commit><dirty>-1` instead of `0.1.0+git<commit><dirty>-1`. The
prefix says *what* is packaged and the stamp says *which commit* packaged it:
`scripts/deb/package-gate.sh` asserts one stamp across the pool while
prefixes differ per package, and `rootfs/build.sh` matches the stamp
against the tree it composes from. Producer-scoped -- it applies to every
package in `PACKAGES` -- so a producer must not mix an upstream repack with a
first-party package. First-party producers do not declare it.

An upstream-versioned package that pins a first-party one uses
`@SYSTEM_VERSION@` in its control template (mica-podman: `Depends: mica-system
(= @SYSTEM_VERSION@)`) and passes `--system-version
"${MICA_DEB_SYSTEM_VERSION}"` to `pack.sh` -- `@VERSION@` is that package's
OWN version, which since the split is not the one mica-system carries.
`pack.sh` refuses the token without the flag and the flag without the token.

`ARCHES=all` means the package is architecture-independent. `all` may not be
mixed with a specific architecture: an `all` package is already a member of
every pool, so the pair would say both that the producer is
architecture-independent and that it is not.

Everything that is **not packaging** -- cross-compiling a binary, generating a
payload, asserting whatever the producer is willing to claim about what it just
produced -- happens in the `PREPARE` hook, on the host, before the build. The
hook is handed `MICA_DEB_ARCH`, `MICA_DEB_STAGE`, `MICA_DEB_PRODUCER`,
`MICA_DEB_PRODUCER_DIR`, `MICA_DEB_REPO_ROOT`, `MICA_DEB_VERSION` and
`SOURCE_DATE_EPOCH`, and what it leaves in `MICA_DEB_STAGE` arrives in the build
as the `bin` context. A hook that reports success and stages nothing is refused
by name. This is where a producer keeps the parts of itself no key above could
express, which is what lets one generic driver build all of them.

`build.sh` always passes `packer` (this directory) as a build context, because
`pack.sh` is the packaging contract and an edit to it must take effect without
rebuilding the builder family.

### `preflight.sh` -- every missing input at once

`make os-debs` builds the discovered producers in sequence and each checks its
own inputs when its turn comes, so a missing input surfaced **after** the
producers ahead of it had been packed, named one file, and the next one was
learned on the next attempt. `scripts/deb/preflight.sh` runs first -- it is
a prerequisite of `os-debs` and the target `make os-deb-preflight` -- and
reports all of them together: every `BUILD_CONTEXTS` path, every `PREPARE` hook
file, every base image `FROM_IMAGES` and the packer resolve to, and whatever a
producer's own hook checks. It prints what it examined and refuses to report
success over a count of zero. It builds nothing and starts no container.

A hook whose inputs no key can describe opts in with `PREFLIGHT="1"` and is then
also run with `MICA_DEB_PREFLIGHT=1`, `MICA_DEB_ARCH` and the producer/repo
variables, and **no** `MICA_DEB_STAGE`: there is nothing to stage into yet. Two
producers do:

- `board-cx3576` picks its BSP artefacts through `BSP_OUT` and its committed
  firmware through `BOARD_DIR`, both of which are chosen at
  run time and so cannot be a fixed path in `producer.env`.
- `podman` reuses `pkgs/podman/out-<arch>`, and reports whether it exists, is
  complete and carries a stamp matching `versions.env` -- see
  `pkgs/podman/versions-stamp.sh`. An absent engine **warns**, because this
  producer builds one; a complete engine compiled from a superseded
  `versions.env` is **missing**, because the producer refuses it. Without the
  warning, that three quarters of an hour arrived only when this producer's
  turn came, started from inside a packaging hook.

In that mode a hook prints **three** counts, on **both** paths --
`preflight-examined:`, `preflight-missing:` and `preflight-warned:` -- and exits
non-zero when the missing one is not zero.

**Missing and warned are decided by what the RUN would do, not by how serious it
looks.** Missing means nothing in the run produces it, so `make os-debs` gets no
further than the producer that needs it and the pre-flight refuses. Warned means
the producer makes it itself, at a cost: the run would succeed, it would just
spend three quarters of an hour somewhere the operator did not expect. That is a
visibility problem, and it is answered by saying so -- the warning names the cost
and the command that pays it separately -- not by refusing. Refusing it would
change what `os-debs` means, since a producer whose hook compiles its own input
is how a pool comes to exist on a fresh host, and three of the five hooks here
compile.

All three counts are required and a zero is written rather than omitted: a hook
that reports success without saying what it looked at is indistinguishable from
one that looked at nothing; a failing hook with no missing count would have a
report naming four files counted as one; and a hook with no warned count would
make "this producer has nothing it can make for itself" and "this hook has not
been taught the category" the same run. Only `examined` refuses a zero.

The aggregate prints the warned total on its **own line**, never folded into the
verdict: two numbers in one sentence is how a category that does not fail a run
stops being visible.

Opt-in per producer, not automatic: a hook that has not been taught the variable
would do its full work instead. A pre-flight that compiles is not a pre-flight --
and PLAN-036 section 4 already says composition does not compile a component.

`tests/deb-preflight-test.sh` drives all of it: the aggregate over a
baseline, `BOARD_DIR` and `BSP_OUT` in both directions, and each half of the count contract
mutated until the run goes red.

### Adding one

Create the directory. Nothing else: `make os-deb-<producer>` is a pattern rule
resolved against `producers.sh`, `make os-debs` loops over the same list, and
`scripts/deb/package-gate.sh` reads it too. There is no driver to register a
case in and no `Makefile` target to add.

### `ENABLEMENT` -- what the archive cannot tell you

The gate checks that each package ships exactly the
`/etc/systemd/system/multi-user.target.wants` symlinks it is supposed to. That
expectation cannot come from the archive, because the symlink **is** the fact
under test -- deriving it from the payload would assert that whatever shipped is
what was meant.

It is declared **per package, never as a universal count**. Some packages own
the unit that starts them; others own no unit at all, and both are correct:

```sh
# pkgs/micad/deb/micad/producer.env
ENABLEMENT="micad=1 mica-apid=1"
# pkgs/micad/deb/mqtt/producer.env
ENABLEMENT="mica-mqttd=0 mica-mqtt-broker=0"
```

Every package in `PACKAGES` needs a row, and a package that ships no link writes
`0` rather than being left out -- an omission and a deliberate zero look
identical, and only one of them is a decision. A producer with no `ENABLEMENT`
at all, and a package with no row, are each a hard failure naming it.

### `Provides`, `Conflicts` and virtual names

`pack.sh` passes both fields through from the control template, and they are
used: `mica-profile-dev` and `mica-profile-prod` conflict with each other and both
`Provides: mica-profile`. So the gate classifies a dependency three ways:

- **local-real** -- some producer emits a package of that name. It must be
  pinned to the exact version the pool was built at and be in the pool.
- **local-virtual** -- no producer emits it, but an archive in the pool declares
  it in `Provides`. It is satisfied by that `Provides` and is **not**
  version-pinned: an unversioned `Provides` cannot satisfy an exact-version
  dependency at all, so requiring one would be requiring something unsatisfiable.
- **external** -- neither. Reported, not judged; `mica-system` and `passwd` are
  external and expected.

`Replaces` stays refused outright. It is the one field that would let two
packages own one path, which is the overlap the ownership check exists to
refuse; `Provides` and `Conflicts` do nothing of the kind.

## `producers.sh`

```
bash scripts/deb/producers.sh
  micad pkgs/micad/deb/micad amd64,arm64 micad,mica-apid micad=1,mica-apid=1
  mqtt pkgs/micad/deb/mqtt amd64,arm64 mica-mqttd,mica-mqtt-broker mica-mqttd=0,mica-mqtt-broker=0

bash scripts/deb/producers.sh --dir-for mqtt
  pkgs/micad/deb/mqtt
```

Five space-separated fields, sorted by producer name: `<producer> <dir>
<arches> <packages> <enablement>`, the last three comma separated and
`<enablement>` `-` when the producer declares none.

**This is the only place the layout is written down.** The Makefile, `build.sh`
and the gate all read it and none searches for itself: two implementations of
the search would let `make os-debs` and the gate disagree about what exists, and
the one that had not been taught about a producer would report green over it.

An empty discovery is a **hard failure**, the way an empty pool is one for
`repo.sh`. `make os-debs` over no producers builds nothing and reports success;
the gate over no producers asserts nothing and reports success. Both green, both
having done nothing.

It refuses, by name: a `producer.env` with no `Dockerfile` beside it, an empty
or malformed `PACKAGES` or `ARCHES`, and two producer directories sharing a
basename. It does **not** refuse a missing `ENABLEMENT` -- that means nothing to
a build, and the gate is what enforces it.

## `build.sh`

```
bash scripts/deb/build.sh --producer <name> --arch <amd64|arm64|all>
```

The one driver. It resolves the producer through `producers.sh`, reads its
`producer.env`, runs the `PREPARE` hook, selects a buildx builder, resolves
`FROM_IMAGES` through `scripts/build/from.sh`, clears this producer's own
archives out of every pool it writes, runs the build and then checks what
actually landed on disk.

An **`ARCHES=all` producer is built once and exported twice**: one
`docker buildx build` with two `-o type=local` outputs, one per pool. Two builds
would be two chances to produce two different archives for one package name, and
the composer resolves each pool independently. The driver asserts the two
exports are byte-identical, and the gate asserts it again over the built pools.
Such a build runs at the **host** architecture and needs no emulation -- there is
no ELF in the payload for `dpkg-shlibdeps` to resolve -- and it requires a
`docker-container` builder, because the `docker` driver accepts only one output
per build.

**A complete pool has a prerequisite that is not in the tree: the cx3576 board
producer's BSP inputs.** `board-cx3576` stages a kernel, a device tree, the
kernel modules and U-Boot out of `_out/boards/cx3576/`, which is
gitignored, so a fresh worktree does not have them and its `PREPARE` hook
refuses by name -- naming `make -C boards/cx3576/bsp <target>`, or pointing
`BSP_OUT` at a tree that already carries them. The part worth knowing
before you meet it is the consequence for the aggregate rather than for that one
producer: `make os-debs` walks the producers in the order `producers.sh` prints
them and stops at the first that fails, and `board-cx3576` sorts first. So
without those inputs the aggregate builds **nothing at all** -- the pool comes
out empty rather than short one package, and no other producer is reached.

## `version.sh`

```
bash scripts/deb/version.sh
  0.1.0+git9671c7cf2d4d-1          a clean tree
  0.1.0+git9671c7cf2d4d.dirty-1    a tree with uncommitted changes
```

One **stamp** across the whole pool, in one implementation. The producers that
share that pool have nothing else in common -- the micad ones are Rust and carry
crate manifests, others are neither and carry none -- so a rule each producer
implemented for itself would be a rule they agree on until one of them is
edited. `build.sh` calls this script for every producer, and
`pkgs/micad/hack/build-deb.sh` calls it rather than composing the string
itself. A producer that declares `VERSION_FROM` keeps the `+git…-1` stamp this
script prints and replaces only the prefix in front of it, so the pool-wide
invariant is the stamp, not the whole string.

The number comes from the repository's `VERSION` file -- one line, the
release version -- so the substrate reads it without knowing what the
repository holds; each package repository carries its own. The micad
workspace's crate manifests must **agree** with it, and
`pkgs/micad/hack/check.sh` asserts that they do, naming the crate that is
behind. `<commit>` is `git rev-parse --short=12 HEAD`, `.dirty` marks a tree
no commit reproduces, and the `-1` is the Debian revision, which does not
move because these packages have no upstream/downstream split.

**`make os-debs` is not safe against a tree that changes while it runs.** The
version is read per producer -- `build.sh` calls this script once for each --
and it is read from `git status --porcelain` as well as from HEAD, so a merge, a
commit and an ordinary uncommitted edit all move it equally. Change the tree
mid-run and the producers built before the change carry one version while those
built after carry another, each correct at the moment it was written. What fails
is the gate's one-version rule, and it fails naming the PACKAGES -- so the
symptom points at the pool while the cause is that the tree moved underneath it.

`SOURCE_DATE_EPOCH` is **not** here; it stays with `build.sh`, which resolves it
as HEAD's timestamp on the host. See below for why `pack.sh` has no default for
it.

## `pack.sh`

```
pack.sh --root <staged-tree-dir> --control <control-template> \
        --version <version> --arch <amd64|arm64|all> --out <dir> \
        [--maintainer-scripts <dir>]
```

| Flag | Meaning |
| --- | --- |
| `--root` | the staged filesystem tree, exactly as it is to be installed. Must not contain a `DEBIAN` directory -- the packer owns that one. |
| `--control` | the control template. Rendered, never copied verbatim. |
| `--version` | the Debian version; replaces `@VERSION@`. |
| `--arch` | `amd64`, `arm64` or `all`; replaces `@ARCH@`. |
| `--out` | where the archive is written, as `<package>_<version>_<arch>.deb`. Created if absent. |
| `--maintainer-scripts` | a directory holding any of `preinst`, `postinst`, `prerm`, `postrm`. Any other filename is refused by name. Installed into `DEBIAN/` mode 0755. |

It runs **inside the target architecture's image**, invoked from a producer
Dockerfile's `RUN`, never from the host. `dpkg-shlibdeps` resolves an ELF's
dependencies against the libraries installed next to it, so an `arm64` payload
packed in an `amd64` container would silently record `amd64`'s versions;
`pack.sh` compares `--arch` against `dpkg --print-architecture` and refuses.
`--arch all` is exempt, having no ELF to resolve.

Producers get the script through a named build context rather than from the
image, so an edit here takes effect without rebuilding the builder family:

```dockerfile
COPY --from=packer pack.sh /usr/local/bin/pack.sh
```

```
docker buildx build --build-context packer=scripts/deb ...
```

### `SOURCE_DATE_EPOCH`

Required in the environment; `pack.sh` fails by name if it is unset. There is
no "now" default, because one would make every archive irreproducible while
every build stayed green.

Every payload mtime is **set** to it, not clamped to it. A clamp -- the
Reproducible Builds convention, and what `dpkg-deb` does on its own -- would
leave a file older than the epoch carrying its own mtime, and in this repository
that mtime is a *checkout* time: buildkit's `COPY` preserves the source file's
mtime exactly, so a payload copied out of the working tree carries the moment
that clone was made rather than anything about the commit. The ruling, what the
alternative would have cost and which constraint forced it are written at the
decision site in `pack.sh`. Ownership is normalised to `root:root` and the
archive is built with `dpkg-deb --build --root-owner-group`.

### The control template

| Field | Required | Notes |
| --- | --- | --- |
| `Package` | yes | lowercase name; the archive is named after it |
| `Version` | yes | must contain `@VERSION@` |
| `Architecture` | yes | must contain `@ARCH@` |
| `Maintainer` | yes | |
| `Section` | yes | |
| `Priority` | yes | |
| `Depends` | no | omit it for a package with no dependencies |
| `Description` | yes | synopsis plus indented continuation lines |
| `Installed-Size` | **must be absent** | computed by the packer |
| `Mica-Source-Repo`, `Mica-Source-Commit` | **must be absent** | written by the packer; see *Provenance* |

Two substitutions happen:

- `@VERSION@` and `@ARCH@`, everywhere in the template;
- `${shlibs:Depends}`, wherever it appears in `Depends:`, with the output of
  `dpkg-shlibdeps` over every ELF executable and shared object found under
  `--root`. The payloads are found with `file`, not from a list, so a binary
  added to the staged tree cannot escape the scan. A template that asks for
  `${shlibs:Depends}` and stages no ELF -- or whose ELFs resolve to nothing --
  is an error, not an empty expansion.

Dependencies that ELF metadata cannot show -- a program `exec`ed by name, a
library opened with `dlopen`, a `systemd` or `nftables` relationship -- stay
written out in the template. `dpkg-shlibdeps` cannot see them.

`Installed-Size` is computed over the payload by the rule `dpkg-gencontrol`
applies: a file or symlink costs `ceil(bytes / 1024)` KiB, every other object
costs one, and `DEBIAN/` is excluded because it is not installed.

`DEBIAN/md5sums` is generated over every regular file in the payload, with
relative paths and no leading `./`.

### Provenance

Every archive says which repository and which commit produced it, in two
control fields dpkg keeps and `dpkg-deb -f` reads back:

```
Mica-Source-Repo: mica-podman
Mica-Source-Commit: 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b
```

`pack.sh` takes them from the environment, like `SOURCE_DATE_EPOCH` and for
the same reason: `build.sh` resolves both on the host, where git is, and the
producer `Dockerfile` declares `ARG MICA_DEB_SOURCE_REPO` and
`ARG MICA_DEB_SOURCE_COMMIT` beside `ARG SOURCE_DATE_EPOCH` so the `RUN` sees
them. The repository is the basename of `origin` (`MICA_SOURCE_REPO`
overrides it for a fork or mirror); the commit is `HEAD` in full, a dirty
tree being already marked in the version stamp. Neither has a default: an
archive with an empty source is one nothing can attribute. `repo.sh` writes
both into `manifest.txt`, `rootfs/build.sh` reads the micad archive's commit
into `_out/<board>/micad-build.txt` for the smoke runner, and the lock (below)
names both so a fetched archive is checked against them.

### What it asserts about the archive it just wrote

`pack.sh` reads the finished `.deb` back with `dpkg-deb` and fails unless:

- `Package`, `Version`, `Architecture`, `Installed-Size`, `Mica-Source-Repo`
  and `Mica-Source-Commit` are the values it was asked for and computed;
- `Depends` carries no unexpanded `${...}`;
- every path in the payload is owned `root/root`;
- the payload's path set equals the staged tree's path set.

It then prints one line: the archive, its size and its `Depends`.

### A worked template

```text
Package: mica-mqttd
Version: @VERSION@
Architecture: @ARCH@
Maintainer: mos build <mos@example.invalid>
Section: admin
Priority: optional
Depends: micad (= @VERSION@), ${shlibs:Depends}
Description: mos D-Bus to MQTT application bridge
 Bridges micad's D-Bus surface onto the local MQTT broker.
```

## `repo.sh`

```
bash scripts/deb/repo.sh --arch <amd64|arm64>
```

Reads `_out/debs/<arch>/pool/*.deb` and writes, beside the pool:

```
_out/debs/<arch>/
  pool/<package>_<version>_<arch>.deb
  Packages       dpkg-scanpackages over the pool, pool-relative Filename:
  SHA256SUMS     sha256sum over the pool archives, pool-relative paths
  manifest.txt   package, version, architecture, installed-size, sha256, file,
                 source-repo, source-commit
```

`manifest.txt` is tab-separated with a leading `#` comment header. Every
column of it is read out of an archive with `dpkg-deb --field` or `sha256sum`;
nothing here is maintained by hand, and an archive without the two provenance
fields is refused rather than indexed blank.

`MICA_POOL_DIR=<dir>` makes `build.sh` and `repo.sh` use `<dir>/<arch>/pool`
instead of this checkout's `_out/debs`; see *Local development* below.

An empty pool is a hard failure naming the directory. A repository generated
from nothing reports success and installs nothing.

The output is deterministic: `dpkg-scanpackages` sorts by package name and
version, `SHA256SUMS` and `manifest.txt` are sorted by filename, and no
timestamp is written beyond what the archives already carry.

The host has no `dpkg`, so the work happens in a container. `repo.sh` runs the
**host** architecture's image, not `--arch`'s: reading control fields
and hashing bytes is architecture-neutral, and a host with no binfmt
registration cannot execute a foreign-architecture image at all -- which would
leave the `arm64` pool unindexable on the machine that just produced it.
`--arch` selects the pool, and each archive's declared `Architecture` is
checked against the pool it sits in.

## The pool's two classes: `fetch.sh` (`--packages "<p> ..."` narrows the fetch to the named rows, a product's closure), `lock.sh`, `publish.sh`, `source.sh`

An image is composed from one pool, and every archive in it is one of two
things. **Built here**: a package a producer of this repository emits, at this
tree's stamp. **Imported**: a package a pin under `deps/packages/` names, at
the pinned version, sha256, source repository and source commit, fetched from
the pool artifact of the repository that built it. Anything else -- an
archive no producer emits and no pin names, a pinned archive at another
digest, a built-here archive at another stamp -- is refused by name. The rule
is implemented once, in `rootfs/runtime/source-lineage.py`; `rootfs/build.sh`,
`scripts/deb/package-gate.sh` and `build/src/release-manifest.ts` apply it
to the pool, the gate and the release respectively.

A pin is one JSON file per package, in the shape of the Debian pins under
`rootfs/debian/packages/`, its targets keyed by the pool they serve (an
`Architecture: all` archive serves both):

```json
deps/packages/mica-podman.json
{ "name": "mica-podman", "repository": "mica-podman", "commit": "<40 hex>",
  "targets": {
    "amd64": { "version": "5.8.6+git1a2b3c4d5e6f-1", "architecture": "amd64",
               "sha256": "<64 hex>", "asset": "mos-podman_5.8.6.git1a2b3c4d5e6f-1_amd64.deb" },
    "arm64": { ... } } }
```

repository publishes what one commit built as OCI artifacts on the registry
`registry.env` names (`ghcr.io/ybolab`): one artifact per architecture,
`<repository>:pool.<arch>.build-<commit12>` in the repository's own package, one layer per archive
(`application/vnd.mica.deb`, titled `<package>_<version>_<arch>.deb`),
annotated with the commit, its date and the repository; its workflow does
so on every push to `main` that passes its gates, and `publish.sh` does the
same from a developer machine. A pin names the repository, the full commit
and the archive's sha256 -- which IS its blob digest -- so the archive is
fetched by digest out of the repository's artifact and nothing "latest" is
ever followed. `registry.env` declares the registry, the user the token
logs in as, the NAME of the token variable (falling back to `gh auth
token`) and the source URL prefix; the token itself is never printed.
Every repository publishes to ONE package named after itself, with its own
CI token, and what an artifact is sits in the tag:
`<kind>[.<name>]*.build-<commit12>` -- `source.build-<c12>`,
`pool.<arch>.build-<c12>`, `board.<board>.build-<c12>`,
`root.<product>.build-<c12>` -- with the manifest's artifactType
`application/vnd.mica.<kind>` and the annotations `mica.source-repo` (the
package's repository) and `mica.source-commit`. GHCR creates a package
private and offers no API to change that, so each repository's package is
made public once by hand; every publisher then pulls what it pushed with no
credential (`oci_require_public`) and fails, naming the package's settings
page, when it could not. A tag is never re-pointed. Artifacts published
before per-repository packages stay in the shared `mica-pool` and
`mica-source` packages (`<repository>[.<arch>].build-<commit12>`), and the
readers fall back to them by digest until every pin has moved. `oci.sh` is the client (curl, jq, sha256sum; the Distribution API's token
challenge for the registries that make one), and `control-fields.py` reads
an archive's control fields without dpkg, so nothing here needs a container
on the host. `tests/oci-test.sh` drives every script against the registry
image `images.env` pins, run as a sibling container.

| Script | Does | Refuses, by name |
| --- | --- | --- |
| `fetch.sh --arch <a> [--check] [--packages "<p> ..."]` | reads each pin's blob by digest out of the repository's pool artifact into the pool, hashes it again and reads the five control fields, skips an archive already present at the right digest; `--check` asks the registry for each blob and downloads nothing; `--packages` narrows the rows to a product's closure | a digest the registry does not hold, a field that differs from the pin (the download is discarded), a package the lock does not pin, 401/403 naming the token variable |
| `lock.sh --bump <component> [--tag <build-…>] [--version <v>] [--package <p> …]` | downloads every `.deb` layer of the named (else newest `build-*`, by the manifests' created date) artifacts of both architectures, writes a pin per package from the archives' own fields, replaces that component's pins, prints the diff -- the pins' only writer | a layer whose name or `Mica-Source-*` fields disagree with the artifact, a dirty archive, a tag that is not `build-<commit12>` |
| `lock.sh --rows [--arch <a>]` | prints the validated pins as rows (the one parser every reader uses) | a malformed pin, targets that disagree |
| `publish.sh [--pool <dir>] [--arch <a>]` | pushes each architecture's pool as `<repo>:pool.<arch>.build-<HEAD12>`, one layer per archive, reads each blob back and compares | a `.dirty` version, an archive from another repository or another commit, a dirty checkout, a tag already there with other bytes |
| `source.sh <component>` | checks the component's repository out at its locked commit into `_out/src/<component>/` | a component locked at two commits, an unreachable repository |

`make os-pool` is `fetch.sh` for both architectures, then `make os-debs` (which
skips a producer whose every package is locked), then `repo.sh` for both;
`make os-deb-preflight` runs `fetch.sh --check` beside the producer inputs;
`make os-lock-bump COMPONENT=<name> [LOCK_TAG=build-…]` wraps `lock.sh --bump`;
`make os-pool-lock-test` drives `fetch.sh`, `lock.sh`, `publish.sh` and `tools/deps.sh` against a
local registry container (`tests/oci-test.sh`) and requires every refusal above by name.

### Local development

A package repository under development builds straight into the assembly's
pool, and the assembly composes from it with the digest check waived for the
packages named:

```
cd /srv/ybolab/mica/micad
MICA_POOL_DIR=/srv/ybolab/mica/mica/_out/debs make os-debs   # dirty stamp, into the assembly's pool
cd /srv/ybolab/mica/mica
bash scripts/deb/repo.sh --arch amd64
MICA_POOL_UNLOCKED="micad mica-apid" MICA_BOARD=x64 bash rootfs/build.sh
```

`MICA_POOL_UNLOCKED` names imported packages only (a name the lock does not
import is refused), the waiver is announced, written into
`rootfs-packages.txt` and the image's `/usr/share/mos/release-identity.env`
(`UNLOCKED=…`), recorded in the lineage record, and the release gate refuses
such an image in the `candidate` and `stable` channels.
