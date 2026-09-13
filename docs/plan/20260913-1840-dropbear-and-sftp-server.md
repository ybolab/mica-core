# 20260913-1840-dropbear-and-sftp-server micad drives dropbear; a Rust sftp-server package

- **status**: in_progress
- **createdAt**: 2026-09-13 18:40
- **task**: `docs/task/20260913-1840-dropbear-and-sftp-server.md`

## Context

`dropbear-bin` 2025.89 replaces OpenSSH on the device. It has no
configuration file: every policy is a command-line flag, it reads only
`~/.ssh/authorized_keys` (after checking that `~/.ssh` and the home are owned
by the user or root and not group/world-writable), and its sftp subsystem
execs `/usr/lib/sftp-server` as the logged-in user. It re-reads nothing on
SIGHUP, so a changed flag needs a restart.

## Mapping from `access.ssh`

| Setting | dropbear |
| --- | --- |
| `listenAddresses` empty | `-p <port>` (every address) |
| each `listenAddresses` entry | `-p <addr>:<port>`, IPv6 as `[addr]:port`; an entry carrying its own port keeps it; at most 10 (dropbear's `DROPBEAR_MAX_PORTS`, beyond which it silently ignores the rest) |
| effective password authentication off | `-s` |
| `permitRootLogin = false` | `-w` (OpenSSH `PermitRootLogin no` refused root by every method) |
| `enabled` | runtime enable + start / stop + disable of `dropbear.service` |

Effective password authentication stays `passwordAuthentication AND a
transient password is active`. `-g` is not emitted: the only password that can
exist is the transient root password, so `-g` would either disable exactly that
login or duplicate `-s` (reported to the coordinator for confirmation).

## Phases

1. Reconciler, tests first: environment-file render, per-account
   `authorized_keys` through `/etc/passwd` (injectable path; a missing account
   is skipped), symlink-safe writes relative to directory descriptors,
   restart on change, published state. Comment and doc sweep of `sshd`/OpenSSH
   wording in micad, micad-settings and apid; `dist/micad.service` loses
   `ProtectHome=yes`, which would hide `/root` and `/home` from micad.
   Verify: `cargo nextest` for micad in the rust-check image.
2. `mica-sftp-server` crate on `russh-sftp` 3.0.0 (latest stable, checked at
   crates.io 2026-09-13), tests first. Verify: unit tests plus the `sftp -D`
   interop test in a container that has `openssh-client`.
3. `deb/sftp/` producer, `hack/build-deb.sh` binary list, Makefile pool/deb.
   Verify: `make deb MICA_ARCH=amd64` then install check of the payload.
4. `make check`; self-review; commit and push.
