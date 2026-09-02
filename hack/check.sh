#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE="$(cd "${HERE}/.." && pwd)"
REPO_ROOT="$(cd "${WORKSPACE}/../.." && pwd)"
cd "${WORKSPACE}"
export PATH="$HOME/.cargo/bin:$PATH"

if [ -z "${MOS_APID_UI_DIST_DIR:-}" ]; then
    bash apid/ui/build.sh
    export MOS_APID_UI_DIST_DIR="${REPO_ROOT}/_out/apid-ui/dist"
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked

# `cargo nextest` does not execute doctests, so this is not a duplicate of the
# line above: without it, a broken doctest passes the gate silently.
cargo test --doc --workspace --locked
cargo deny check licenses bans advisories

echo "ALL CHECKS PASSED"
