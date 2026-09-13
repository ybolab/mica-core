# mica-core

The management daemon of Mica OS and what ships beside it, as one Cargo
workspace with every crate under `crates/`: `mica-core` (the `micad`
executable: the reconcilers over settings, network, radios, containers,
updates and the system bus), `mica-apid` (the `apid` HTTP API and the built-in
UI under `crates/mica-apid/ui/`; the executable is `mica-apid`), `mica-mqttd`, `mica-mqtt-broker`,
`mica-sftp-server`, `mica-deploy` (the device-side deployment client and the
static `mica-runkit`), with the shared crates `micad-settings`, `mica-busname`,
`mica-ui-bundle`, `mica-mqtt-reference` and `lifecycle-sys`. It stands on the
`mica-build-env` substrate fetched at its pin into `build-env/`:

```
make deps            # build-env/ at deps/sources/mica-build-env.json
make build-env       # the builder images
make check           # lint, the UI build contract, the Rust gate, the boot/shutdown fixtures, the IO fault suite
make pool            # the seven packages, both architectures, indexed
make package-gate    # the gate over that pool
make publish         # CI only: ghcr.io/ybolab/mica-core:pool.<arch>.build-<commit12>
```

Seven packages leave here: `micad` (one binary carrying both micad and apid),
`mica-apid` (the `/usr/bin/mica-apid` link to it, and the committed OpenAPI
document as `/usr/share/mica-apid/openapi.json`), `mica-mqttd`,
`mica-mqtt-broker`, `mica-sftp-server` (`/usr/lib/sftp-server`, selected by
boards), `mica-deploy` (`/usr/bin/mica-deploy`) and `mica-lifecycle` (the static
`/usr/lib/mica/lifecycle/mica-runkit`). The assembly (`ybolab/mica-build`)
imports them through `deps/packages/` and builds none; the API harness that
boots the assembled image and drives apid over a socket lives there
(`tests/apid-api/`). `packaging/deb/README.md` is the packaging contract of the
producers under `packaging/deb/`; the shell entry points are under
`scripts/build/` and `scripts/gate/`, and `scripts/gate/dbus-policy-test.sh`
proves the shipped D-Bus policy against a real `dbus-daemon`.

mica-deploy was its own repository until 2026-09-13; its history is merged
unchanged at commit `b698b10dd6aa` (the second parent of the import merge), so
`git log <import merge>^2 -- src/acquisition.rs` reads a file's history from
before the move.
