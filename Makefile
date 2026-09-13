# mica-deploy: the native boot and deployment tools of Mica OS -- mica-init,
# mica-shutdown and mica-deploy -- packed as the Debian packages mica-deploy
# (the device-side client) and mica-lifecycle (the two static executables
# the signed kernel image carries). Heavy lifting stays in the scripts; this
# file only routes.

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

.PHONY: help deps deps-check deps-bump build-env rust-gate boot-shutdown-test file-transaction-faults deb pool package-gate preflight publish lint check

help:
	@echo "  deps                fetch build-env/ at its pin (deps/sources/); deps-check reads without downloading"
	@echo "  deps-bump           rewrite the pin from the newest build-* release (DEP_TAG=build-<commit12> picks one)"
	@echo "  build-env           the builder images, from the pins in build-env/images.env"
	@echo "  rust-gate           hack/check.sh (fmt, clippy -D warnings, nextest, doctests, cargo-deny) in the pinned rust-check image"
	@echo "  boot-shutdown-test  the native shutdown suite and the UAPI translation unit (BOOT_SHUTDOWN_ARM_ABI=1 adds the aarch64 compiler)"
	@echo "  file-transaction-faults  exact native transaction code, interrupted before and after each observed IO"
	@echo "  deb                 both producers for \$$MOS_ARCH into _out/debs/\$$MOS_ARCH/pool (MOS_ARCH=amd64|arm64)"
	@echo "  pool                both producers, both architectures, indexed"
	@echo "  package-gate        the package gate over this repository's pool"
	@echo "  publish             the pool as the GitHub Release build-<commit12> of this commit"
	@echo "  lint                shell hygiene of the tree"
	@echo "  check               everything that runs from the pinned images: lint, rust-gate, boot-shutdown-test, file-transaction-faults"

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

boot-shutdown-test:
	bash gate/boot-shutdown-test.sh $(if $(BOOT_SHUTDOWN_ARM_ABI),--arm-abi)

file-transaction-faults:
	bash gate/file-ab-faults/run.sh

preflight:
	bash build-env/deb/preflight.sh

deb: preflight
	bash build-env/deb/build.sh --producer deploy --arch $(MOS_ARCH)
	bash build-env/deb/build.sh --producer lifecycle --arch $(MOS_ARCH)

pool: preflight
	bash build-env/deb/build.sh --producer deploy --arch amd64
	bash build-env/deb/build.sh --producer lifecycle --arch amd64
	bash build-env/deb/build.sh --producer deploy --arch arm64
	bash build-env/deb/build.sh --producer lifecycle --arch arm64
	bash build-env/deb/repo.sh --arch amd64
	bash build-env/deb/repo.sh --arch arm64

package-gate:
	bash build-env/deb/package-gate.sh

publish:
	bash build-env/deb/publish.sh

lint:
	bash gate/shell-lint.sh

check: lint rust-gate boot-shutdown-test file-transaction-faults
