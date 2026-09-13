#!/usr/bin/env bash
# Run the actual native transactions with faults at every observed IO boundary.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
command -v docker >/dev/null
image=$(bash build-env/from.sh --arch=amd64 --ref LOCAL_MOS_BUILD_RUST_CHECK)
work=$(mktemp -d "$PWD/_out/io-faults.XXXXXX")
mkdir -p _out/rust-gate _out/cargo/registry _out/cargo/git
# mos-build-side: container-block -- the pinned compiler builds only the test shim.
docker run --rm --label ai-agent=true --network traefik \
    --tmpfs /space:rw,size=132m -e MOS_TEST_SPACE_ROOT=/space \
    -v "$PWD:/src:ro" -v "$work:/evidence" \
    -v "$PWD/_out/rust-gate:/target" \
    -v "$PWD/_out/cargo/registry:/usr/local/cargo/registry" \
    -v "$PWD/_out/cargo/git:/usr/local/cargo/git" \
    -w /src -e CARGO_TARGET_DIR=/target --entrypoint /bin/bash "$image" -ceu '
    command -v cc
    cc -std=c11 -Wall -Wextra -Werror -shared -fPIC /src/gate/file-ab-faults/io-fault.c -ldl -o /evidence/io-fault.so
    export MOS_TEST_FAULT_SHIM=/evidence/io-fault.so
    export MOS_TEST_FAULT_EVIDENCE=/evidence
    cargo test --locked --test io_faults -- --ignored --test-threads=1 --nocapture
    '
# mos-build-side: host
echo "IO fault evidence: $work"
