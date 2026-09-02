#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

if [ -z "${MOS_APID_UI_DIST_DIR:-}" ]; then
    bash apid/ui/build.sh --out-dir "${PWD}/apid/ui/dist"
    export MOS_APID_UI_DIST_DIR="${PWD}/apid/ui/dist"
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked

# `cargo nextest` does not execute doctests, so this is not a duplicate of the
# line above: without it, a broken doctest passes the gate silently.
cargo test --doc --workspace --locked
cargo deny check licenses bans advisories

echo "ALL CHECKS PASSED"
