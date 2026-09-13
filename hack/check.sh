#!/usr/bin/env bash
# mos-build-side: container -- the same as micad:hack/check.sh, for the same reason and by the same two callers: tests/rust-gate.sh takes both workspaces into localhost/mos-build-rust-check, and since PLAN-080 backlog B7 the CI runner reaches this file only through that script. The PATH prepend below finds nothing in the image.
# The gate for this workspace. It exists because `mica-deploy` is no longer a
# member of the micad workspace: the split gave it its own `[workspace]`, and
# from that moment `cargo clippy --workspace` and `cargo nextest run --workspace`
# run from micad: stopped reaching this crate. Without this script the code would
# ship unchecked with every other gate still green.
#
# Deliberately a line-for-line twin of micad:hack/check.sh, in the same order.
# Two gate definitions that differ only in where they run should read as the
# same gate; a divergence should be visible as a diff, not hidden in phrasing.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked

# `cargo nextest` does not execute doctests, so this is not a duplicate of the
# line above: without it, a broken doctest passes the gate silently.
cargo test --doc --workspace --locked
cargo deny check licenses bans advisories

echo "ALL CHECKS PASSED"
