#!/usr/bin/env bash
# mica-build-side: container -- both callers run this inside localhost/mica-build-rust-check: `make os-rust-gate` locally and .github/workflows/check.yml on the runner, each through tests/rust-gate.sh, which invokes this file unmodified. The PATH prepend below finds nothing in there and the image's own /opt/rust/bin answers, so the cargo, clippy, rustfmt, nextest and deny that run are the versions build-env/images.env records. PLAN-080 backlog B7 is what made this true: until CI stopped installing a rustup toolchain of its own, one of the two callers was a bare host and this declaration would have been a claim about only half the runs.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "${HERE}/../.." && pwd)"
REPO_ROOT="${WORKSPACE}"
cd "${WORKSPACE}"
export PATH="$HOME/.cargo/bin:$PATH"

if [ -z "${MICA_APID_UI_DIST_DIR:-}" ]; then
    bash crates/apid/ui/build.sh
    export MICA_APID_UI_DIST_DIR="${REPO_ROOT}/_out/apid-ui/dist"
fi

# THE REPOSITORY VERSION AND THE CRATE VERSION AGREE. build-env/deb/version.sh
# stamps every archive from ${REPO_ROOT}/VERSION; the binaries report the crate
# version from their manifests. Two numbers, one release: a drift between them
# is a package whose version is not the version the binary inside it prints.
declared="$(tr -d '[:space:]' <"${REPO_ROOT}/VERSION")"
for m in crates/*/Cargo.toml; do
    name="$(sed -n '/^\[package\]/,/^\[/ s/^name[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "${m}" | head -n1)"
    [ -n "${name}" ] || continue
    v="$(sed -n '/^\[package\]/,/^\[/ s/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "${m}" | head -n1)"
    [ "${v}" = "${declared}" ] || {
        echo "error: ${m} declares ${name} at version '${v}' and ${REPO_ROOT}/VERSION declares '${declared}'. The package pool is stamped from VERSION and the binaries report the crate version; move whichever is behind" >&2
        exit 1
    }
done

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked

# `cargo nextest` does not execute doctests, so this is not a duplicate of the
# line above: without it, a broken doctest passes the gate silently.
cargo test --doc --workspace --locked
cargo deny check licenses bans advisories

# THE `--openapi` FLAG PATH. `cargo nextest run` above already runs the in-tree
# test that compares the committed document against the generated one, so this
# catches no drift that test misses. What it adds is the argv handling in
# `main()`, above daemon initialisation, which that test bypasses by calling the
# generator function directly: a flag that stopped printing the document, or
# started reaching for the bus before it did, leaves the test green and the
# documented regeneration command broken.
#
# HERE and not in .github/workflows/check.yml, which is where it used to live.
# That step ran `cargo` on the runner, and it was the last reason the runner
# needed a toolchain of its own -- PLAN-080 backlog B7. Moving it into the gate
# means it runs wherever the gate runs: inside localhost/mica-build-rust-check
# under tests/rust-gate.sh, on both of the gate's callers, and for the first
# time on a developer's machine as well.
openapi_tmp="$(mktemp -d)"
trap 'rm -rf "${openapi_tmp}"' EXIT
cargo run --locked -p apid -- --openapi >"${openapi_tmp}/openapi.json"
diff -u crates/apid/openapi.json "${openapi_tmp}/openapi.json" || {
    echo "error: crates/apid/openapi.json is not what apid --openapi prints." >&2
    echo "       Regenerate it from :" >&2
    echo "         bash crates/apid/ui/build.sh" >&2
    echo '         MICA_APID_UI_DIST_DIR="$PWD/_out/apid-ui/dist" cargo run -p apid -- --openapi > crates/apid/openapi.json' >&2
    exit 1
}
echo "crates/apid/openapi.json matches apid --openapi"

echo "ALL CHECKS PASSED"
