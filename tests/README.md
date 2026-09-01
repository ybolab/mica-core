# mosd workspace tests

This directory owns tests that exercise more than one mosd crate or require a
running OS image. It is deliberately separate from Cargo's crate-local test
layout:

- Rust unit tests stay beside their implementation under each crate's `src/`.
- Rust integration tests stay under each crate's `tests/`, where Cargo
  discovers them automatically.
- Workspace-level and black-box harnesses live here.

## Harnesses

- `apid-api/` is the Bun-based HTTP black-box suite. It boots the x64 image in
  QEMU, drives APID through its public HTTPS surface, and checks its literals
  against `apid/openapi.json`. See `apid-api/README.md` and
  `apid-api/HARNESS.md` before running it.
- `dbus-policy-test.sh` starts isolated D-Bus daemons and verifies the shipped
  mosd policy plus the MQTT bridge's zero-mosd boundary in both the permitted
  and refused directions. It also audits all repository policy fragments for
  prefix and wildcard grants. It requires root, `dbus-daemon`, `setpriv`, and
  Python 3.

Run the repository entry points rather than depending on the harnesses'
internal commands:

```sh
make os-apid-api-spec-pins
make os-apid-api-test
make os-dbus-policy-test
```

The full APID test requires a built x64 image. Its spec-pin target is the
self-contained CI gate and does not boot QEMU.
