# 20260913-1840-dropbear-and-sftp-server micad drives dropbear; a Rust sftp-server package

- **status**: completed
- **priority**: P1
- **owner**: vtv87o8e/mica-core
- **createdAt**: 2026-09-13 18:40

## Description

Decision 2026-09-13 (user): the device SSH server is dropbear instead of
OpenSSH; scp and sftp are served by a Rust sftp-server on `russh-sftp`, and
the legacy SCP protocol (`scp -O`) is not supported. This task is the
mica-core half (issue vtv87o8e); `mica-system` ships `dropbear.service` and
the host keys, `mica-build` adopts the packages.

Acceptance:

1. `micad/src/reconciler/sshd.rs` renders `/run/mica/dropbear.env` (one line,
   `DROPBEAR_ARGS="..."`) and each managed account's `~/.ssh/authorized_keys`
   (owner = account, `~/.ssh` 0700, file 0600, home refused when group- or
   world-writable) instead of the `sshd_config.d` drop-in and
   `/etc/ssh/authorized_keys.d/<account>`; it restarts `dropbear.service` when
   the environment file changes. Policy semantics are kept: password
   authentication gated on an active transient password, listen addresses,
   permitRootLogin, published effective/requested state. Unit tests first, on
   the existing `UnitControl` seam.
2. New workspace member `mica-sftp-server`: a stdio SFTP server on
   `russh-sftp` running as the invoking user, covering open/read/write/close,
   opendir/readdir, stat/lstat/fstat, setstat/fsetstat,
   mkdir/rmdir/remove/rename, realpath, readlink/symlink. Protocol unit tests
   plus an interop test through OpenSSH `sftp -D` when `sftp` exists.
3. Package `mica-sftp-server` installing `/usr/lib/sftp-server`, through a
   producer under `deb/`.
4. `make check` passes; the new producer builds and the package gate passes.

## ActiveForm

Moving micad to dropbear and adding the sftp-server package

## Dependencies

- **blocked by**: (none)
- **blocks**: `mica-build` adoption (`mica-system` needs nothing from here beyond `/run/mica/dropbear.env`)

## Notes

- Plan: `docs/plan/20260913-1840-dropbear-and-sftp-server.md`.
- The documentation half (`mica:docs/design/access.md`, `mica:docs/changelog.md`)
  is transferred to the documentation issue through coordinator a0psyi7e.

### Progress 2026-09-13

- Reconciler (`micad/src/reconciler/sshd.rs`): `/run/mica/dropbear.env` is one
  line, `-p <addr>:<port>` per listen address (`-p <port>` for none, IPv6 in
  brackets, at most 10), `-s` unless password authentication is effective,
  `-w` when `permitRootLogin` is false, never `-g`. Keys go to
  `~/.ssh/authorized_keys` of `root` and `mos` as `/etc/passwd` names them,
  through directory descriptors with `O_NOFOLLOW`; a home that is group- or
  world-writable or foreign-owned fails the apply before anything is written,
  a missing account or home is skipped. A changed file under a running server
  restarts `dropbear.service` (mica-system sets `KillMode=process`). The
  published state key `dropIn` became `environmentFile`; the reconciler name
  and live-state key stay `sshd`. `UnitControl::reload` lost its only caller
  and was removed. `dist/micad.service` drops `ProtectHome=yes`.
- Tests: 58 reconciler tests; each of no `-s`, reload instead of restart, no
  home check, following a `~/.ssh` symlink, no address cap and `-g` for root
  was mutated in and caught; a planted FIFO test was RED before the
  `O_NONBLOCK` fix. Also run as a non-root user.
- `mica-sftp-server` on russh-sftp 3.0.0 (latest stable, crates.io
  2026-09-13): own framing loop, 18 protocol tests, 2 interop tests through
  OpenSSH 10.0 `sftp -D` (they skip where `sftp` is absent, as in the
  rust-check image; run in a container with `openssh-client`, root and
  non-root).
- End to end with the pinned `dropbear-bin_2025.89-1~deb13u1_amd64.deb`,
  OpenSSH 10.0 clients and the layout micad renders: sftp batch (1 MB
  upload/download, `ls -l`, rename, mkdir/rmdir, symlink, Permission denied
  into /etc), scp and `scp -r` in SFTP mode, `scp -O` refused with no scp on
  the server, root refused under `-w`, key login as mos, key refused under a
  group-writable home.
- `deb/sftp` producer: `mica-sftp-server` installs `/usr/lib/sftp-server`,
  `Depends: libc6, libgcc-s1`, both architectures built.
- Gates on ae513fb: `make check` (shell lint, apid UI build contract, fmt,
  clippy `-D warnings`, nextest 1104/1104, doctests, cargo-deny advisories,
  bans and licences ok, OpenAPI match); `make pool`; `make package-gate`
  91/91 over 10 archives, reproducibility rebuilds byte-identical.
- Left for others (through coordinator a0psyi7e): mica-build adoption
  (consumers, verifier, pin bump), the `mica:docs/design/access.md` and
  changelog text, and a rust-check image with `openssh-client` so the
  interop tests run in the gate instead of skipping.

- complete: Implemented and verified on ae513fb; in review

### Follow-up 2026-09-13 (coordinator decisions)

- Root policy confirmed: `-w` for permitRootLogin=false, `-s` for effective
  password authentication off, never `-g`. Dropbear ships disabled on both
  profiles; micad enables and starts it at runtime after writing the files.
- Added: a unit control that observes the files at each call proves the env
  file and both key files are complete before enable, start and restart; a
  `failed` dropbear.service is reset before it is started (RED first), as the
  mqtt and time reconcilers do; every permitRootLogin x passwordAuthentication
  x transient-password combination through `apply`; a port change across IPv4,
  IPv6 and an entry with its own port.
- Evidence correction: the pinned Debian dropbear lists `-D <directory>` for
  another authorized_keys location; micad does not pass it.
- Service-layer session survival across restart is mica-system's evidence
  (7faafef, KillMode=process); a micad-driven guest check is still open.
- `make check` 1109/1109.

### SFTP is a board feature (user policy, 2026-09-13 19:12 UTC)

- `mica-sftp-server` is no longer a dependency of `mica-system` (its local
  13c1299 drops the `Depends`); boards select it and mica-build composes it.
  The producer and the Rust code stay here.
- Implications here: the `sftp` producer still builds for both architectures on
  every commit and the package gate covers it; nothing in micad assumes the
  binary exists, and the reconciler renders the same arguments either way; the
  sftp -D interop tests skip in
  the rust-check image, which has no OpenSSH client, and were run explicitly in
  a container that has one (adding openssh-client to that image is optional).
- Own CI (cc4a3f9 and its parents f3de5c6, 1c7442a) sit below the runtime
  commits, so they can be published without them.
