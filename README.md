# mica-core

The management daemon of Mica OS and what ships beside it: `micad` (the
reconcilers over settings, network, radios, containers, updates and the
system bus), `apid` (the HTTP API and the built-in UI under `apid/ui/`),
`mica-mqttd` and `mica-mqtt-broker`, with the shared crates
`micad-settings`, `mica-busname`, `mica-ui-bundle` and `mica-mqtt-reference`.
It is a repository of its own, standing on the `mica-build-env` substrate
fetched at its pin into `build-env/`:

```
make deps            # build-env/ at deps/sources/mica-build-env.json
make build-env       # the builder images
make check           # lint, the UI build contract, the Rust gate (hack/check.sh)
make pool            # the four packages, both architectures, indexed
make package-gate    # the gate over that pool
make publish         # the release build-<commit12> of this commit
```

Four packages leave here: `micad`, `mica-apid` (which also ships the
committed OpenAPI document as `/usr/share/mica-apid/openapi.json`),
`mica-mqttd` and `mica-mqtt-broker`. The assembly (`ybolab/mica-build`)
imports them through `deps/packages/` and builds none; the API harness that
boots the assembled image and drives apid over a socket lives there
(`tests/apid-api/`). `deb/README.md` is the packaging contract of the
producers under `deb/`; `tests/dbus-policy-test.sh` proves the shipped
D-Bus policy against a real `dbus-daemon`.
