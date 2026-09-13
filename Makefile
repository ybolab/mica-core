# mica-core: micad, the management daemon of Mica OS, and what ships beside it --
# apid with its built-in UI, mica-mqttd, mica-mqtt-broker, the sftp-server
# dropbear runs, mica-deploy and the static mica-runkit -- packed as the Debian
# packages micad, mica-apid, mica-mqttd, mica-mqtt-broker, mica-sftp-server,
# mica-deploy and mica-lifecycle. Heavy lifting stays in the scripts;
# this file only routes.

# THE RELEASE PIN, before anything else: build-env/ holds the mica-build-env
# release every image reference resolves through (build-env/images.env). It is
# fetched and verified at its pin (deps/build-env.json) by
# scripts/build/build-env.sh and is gitignored, so a fresh clone has none.
# Only the targets that use no build-env image run without it; `make` alone
# is `make help`.
NO_BUILD_ENV_GOALS := help deps deps-check lint pool-decision-test release-check
ifneq ($(filter-out $(NO_BUILD_ENV_GOALS),$(or $(MAKECMDGOALS),help)),)
ifeq ($(wildcard build-env/images.env),)
$(error build-env/ is empty: the mica-build-env release is fetched at its pin. Run: make deps)
endif
endif

MICA_ARCH ?= arm64

.PHONY: help deps deps-check rust-gate dbus-policy-test apid-ui-build-contract-test boot-shutdown-test file-transaction-faults pool-decision-test deb pool package-gate preflight publish release-check lint check

help:
	@echo "  deps                fetch and verify the mica-build-env release at its pin (deps/build-env.json) into build-env/; deps-check verifies build-env/ without the network"
	@echo "  rust-gate           scripts/build/check.sh (VERSION agreement, fmt, clippy -D warnings, nextest, doctests, cargo-deny, openapi) in the pinned IMAGE_MICA_BUILD_RUST"
	@echo "  dbus-policy-test    prove the shipped micad D-Bus policy is root-only against a real dbus-daemon (needs dbus-daemon on the host)"
	@echo "  apid-ui-build-contract-test  the built-in SPA builds as ignored production assets in the pinned Bun image"
	@echo "  deb                 every producer for \$$MICA_ARCH into _out/debs/\$$MICA_ARCH/pool (MICA_ARCH=amd64|arm64)"
	@echo "  pool                every producer, both architectures, indexed"
	@echo "  package-gate        the package gate over this repository's pool"
	@echo "  publish             the pool as ghcr.io/ybolab/mica-core:pool.<arch>.build-<commit12> (the release workflow publishes, for a v<VERSION> tag; a developer machine does not)"
	@echo "  release-check       TAG=v<VERSION>: the tag may be released from HEAD (v<VERSION>, on origin/main), before pushing it"
	@echo "  lint                shell hygiene of the tree"
	@echo "  pool-decision-test  scripts/build/pool-decision.sh (does this commit build a pool) against fixture history, registry and releases"
	@echo "  boot-shutdown-test  the native shutdown suite and the UAPI translation unit (BOOT_SHUTDOWN_ARM_ABI=1 adds the aarch64 compiler)"
	@echo "  file-transaction-faults  mica-deploy's transaction code, interrupted before and after each observed IO"
	@echo "  check               everything that runs from the pinned images: lint, pool-decision-test, apid-ui-build-contract-test, rust-gate, boot-shutdown-test, file-transaction-faults"

deps:
	bash scripts/build/build-env.sh fetch
deps-check:
	bash scripts/build/build-env.sh check

rust-gate:
	bash scripts/gate/rust-gate.sh

dbus-policy-test:
	bash scripts/gate/dbus-policy-test.sh

boot-shutdown-test:
	bash scripts/gate/boot-shutdown-test.sh $(if $(BOOT_SHUTDOWN_ARM_ABI),--arm-abi)

file-transaction-faults:
	bash scripts/gate/file-ab-faults/run.sh

apid-ui-build-contract-test:
	bash scripts/gate/apid-ui-build-contract-test.sh
	bash crates/mica-apid/ui/run.sh

preflight:
	bash scripts/deb/preflight.sh

deb: preflight
	bash scripts/deb/build.sh --producer micad --arch $(MICA_ARCH)
	bash scripts/deb/build.sh --producer apid --arch $(MICA_ARCH)
	bash scripts/deb/build.sh --producer mqtt --arch $(MICA_ARCH)
	bash scripts/deb/build.sh --producer sftp --arch $(MICA_ARCH)
	bash scripts/deb/build.sh --producer deploy --arch $(MICA_ARCH)
	bash scripts/deb/build.sh --producer lifecycle --arch $(MICA_ARCH)

pool: preflight
	bash scripts/deb/build.sh --producer micad --arch amd64
	bash scripts/deb/build.sh --producer apid --arch amd64
	bash scripts/deb/build.sh --producer mqtt --arch amd64
	bash scripts/deb/build.sh --producer sftp --arch amd64
	bash scripts/deb/build.sh --producer deploy --arch amd64
	bash scripts/deb/build.sh --producer lifecycle --arch amd64
	bash scripts/deb/build.sh --producer micad --arch arm64
	bash scripts/deb/build.sh --producer apid --arch arm64
	bash scripts/deb/build.sh --producer mqtt --arch arm64
	bash scripts/deb/build.sh --producer sftp --arch arm64
	bash scripts/deb/build.sh --producer deploy --arch arm64
	bash scripts/deb/build.sh --producer lifecycle --arch arm64
	bash scripts/deb/repo.sh --arch amd64
	bash scripts/deb/repo.sh --arch arm64

package-gate:
	bash scripts/deb/package-gate.sh

publish:
	bash scripts/deb/publish.sh

release-check:
	@[ -n "$(TAG)" ] || { echo "usage: make release-check TAG=v<VERSION>" >&2; exit 1; }
	bash scripts/build/release.sh check "$(TAG)"

lint:
	bash scripts/gate/shell-lint.sh

pool-decision-test:
	bash scripts/gate/pool-decision-test.sh

check: lint pool-decision-test apid-ui-build-contract-test rust-gate boot-shutdown-test file-transaction-faults
