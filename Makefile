# micad: the management daemon of Mica OS and what ships beside it -- micad,
# apid with its built-in UI, mica-mqttd and mica-mqtt-broker -- packed as the
# Debian packages micad, mica-apid, mica-mqttd and mica-mqtt-broker. Heavy
# lifting stays in the scripts; this file only routes.

# THE SOURCE DEPENDENCY, before anything else: build-env/ (mica-build-env) is
# the substrate every target reaches through. It is fetched at its pin
# (deps/sources/mica-build-env.json) by tools/deps.sh and is gitignored, so a
# fresh clone has none. `make deps` is the one target that may run without it.
ifeq ($(filter deps,$(MAKECMDGOALS)),)
ifeq ($(wildcard build-env/from.sh),)
$(error build-env/ is empty: the build substrate is fetched at its pin from ybolab/mica-build-env. Run: make deps)
endif
endif

MOS_ARCH ?= arm64

.PHONY: help deps deps-check deps-bump build-env rust-gate dbus-policy-test apid-ui-build-contract-test deb pool package-gate preflight publish lint check

help:
	@echo "  deps                fetch build-env/ at its pin (deps/sources/); deps-check reads without downloading"
	@echo "  deps-bump           rewrite the pin from the newest build-* release (DEP_TAG=build-<commit12> picks one)"
	@echo "  build-env           the builder images, from the pins in build-env/images.env"
	@echo "  rust-gate           hack/check.sh (VERSION agreement, fmt, clippy -D warnings, nextest, doctests, cargo-deny, openapi) in the pinned rust-check image"
	@echo "  dbus-policy-test    prove the shipped micad D-Bus policy is root-only against a real dbus-daemon (needs dbus-daemon on the host)"
	@echo "  apid-ui-build-contract-test  the built-in SPA builds as ignored production assets in the pinned Bun image"
	@echo "  deb                 both producers for \$$MOS_ARCH into _out/debs/\$$MOS_ARCH/pool (MOS_ARCH=amd64|arm64)"
	@echo "  pool                both producers, both architectures, indexed"
	@echo "  package-gate        the package gate over this repository's pool"
	@echo "  publish             the pool as the GitHub Release build-<commit12> of this commit"
	@echo "  lint                shell hygiene of the tree"
	@echo "  check               everything that runs from the pinned images: lint, apid-ui-build-contract-test, rust-gate"

deps:
	bash tools/deps.sh fetch
deps-check:
	bash tools/deps.sh fetch --check
deps-bump:
	bash tools/deps.sh bump mica-build-env $(if $(DEP_TAG),--tag "$(DEP_TAG)")

build-env:
	bash build-env/build.sh

rust-gate:
	bash gate/rust-gate.sh

dbus-policy-test:
	bash tests/dbus-policy-test.sh

apid-ui-build-contract-test:
	bash gate/apid-ui-build-contract-test.sh
	bash apid/ui/run.sh

preflight:
	bash build-env/deb/preflight.sh

deb: preflight
	bash build-env/deb/build.sh --producer micad --arch $(MOS_ARCH)
	bash build-env/deb/build.sh --producer mqtt --arch $(MOS_ARCH)

pool: preflight
	bash build-env/deb/build.sh --producer micad --arch amd64
	bash build-env/deb/build.sh --producer mqtt --arch amd64
	bash build-env/deb/build.sh --producer micad --arch arm64
	bash build-env/deb/build.sh --producer mqtt --arch arm64
	bash build-env/deb/repo.sh --arch amd64
	bash build-env/deb/repo.sh --arch arm64

package-gate:
	bash build-env/deb/package-gate.sh

publish:
	bash build-env/deb/publish.sh

lint:
	bash gate/shell-lint.sh

check: lint apid-ui-build-contract-test rust-gate
