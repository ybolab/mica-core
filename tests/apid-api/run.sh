#!/usr/bin/env bash
# Boot a UEFI board's image with apid reachable from outside it, and run the API suite
# against the running daemon.
#
#   bash pkgs/mosd/tests/apid-api/run.sh
#   bash pkgs/mosd/tests/apid-api/run.sh --dry-run
#
# This is the only check here that talks to apid over a real socket, on a
# machine that came up through enrolled UEFI, signed UKIs and its own unit ordering, so a route
# that exists in routes.rs but is unreachable in the running daemon looks
# different from one that works. It builds nothing: the image is an input, and a
# missing one is refused by name with the two commands that make it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
# WHICH BOARD. Resolved once, here, and exported so that src/qemu.ts reads the
# same value rather than defaulting independently -- two halves that each
# picked a board could disagree, and the disagreement would present as "image
# missing" for a board nobody asked about.
#
# x64 is the default because it is the board this harness has always booted and
# the one with a KVM-capable host; virt-arm64 is the same suite on the
# architecture the device runs. Any UEFI board works: nothing below knows a
# board name.
MOS_BOARD="${MOS_BOARD:-x64}"
export MOS_BOARD
BOARD_ENV="${REPO_ROOT}/boards/${MOS_BOARD}/board.env"
if [ ! -f "${BOARD_ENV}" ]; then
    echo "FAIL: MOS_BOARD is '${MOS_BOARD}' and ${BOARD_ENV} does not exist; a board IS its board.env" >&2
    exit 1
fi

# The board layout is the board definition; the image's name is read from it
# rather than repeated here, the same way src/qemu.ts reads it. `:?` on the
# one key used turns a layout that stopped defining it into a sentence instead
# of an empty path that fails four lines later as "image missing".
# shellcheck source=/dev/null  # a data file of assignments, resolved at runtime
. "${BOARD_ENV}"

OUT_DIR="${REPO_ROOT}/_out/${MOS_BOARD}"
IMG="${MOS_QEMU_IMAGE:?explicit complete factory image required}"
BOOT_CERT="${MOS_QEMU_BOOT_CERT:?explicit public boot certificate required}"
IMG="$(readlink -f "$IMG")"
BOOT_CERT="$(readlink -f "$BOOT_CERT")"
RUN_DIR="${OUT_DIR}/.qemu"
ART_DIR="${OUT_DIR}/apid-api"

# `RUN_DIR` is the boot engine's single fixed path -- src/qemu.ts prepares the
# disk there and the DATA seeder writes into that same disk by name
# -- so two runs at once clobber each other's disk.img and this script refuses
# to start while another container holds it. Two runs at once is not a
# hypothetical: `_out` is per-checkout and gitignored, so a worktree points it
# at the checkout that built the image and the two then share the REAL
# directory.
#
# `RUN_DIR_REAL` is resolved once and used for every comparison against a docker
# mount source. `docker inspect` reports the path it was given, not the path it
# resolved, so a run directory reached through a symlink -- a git worktree
# pointing _out at the checkout that built the image is the ordinary case --
# slips past a string comparison, and the guard below would wave through the
# collision it exists to stop.
# `readlink -f` demands every component but the last, so a checkout with no
# ${OUT_DIR} at all -- nothing was ever built here -- used to die right on this
# assignment under `set -e`, as a bare exit 1 with no output. Refuse with a
# sentence instead; the image check further down never gets a chance to.
if ! RUN_DIR_REAL="$(readlink -f "${RUN_DIR}")"; then
    echo "FAIL: ${OUT_DIR} does not exist, so there is no image to boot; this harness builds nothing. Build a complete image with build/run.sh --components image" >&2
    exit 1
fi
OUT_REAL="$(readlink -f "${REPO_ROOT}/_out")"

# Ports: the same names src/qemu.ts reads, so a caller sets them once and
# the two halves cannot disagree about which port was opened.
HTTPS_PORT="${MOS_QEMU_HTTPS_PORT:-18443}"
HTTP_PORT="${MOS_QEMU_HTTP_PORT:-18080}"

# A TCG boot with no /dev/kvm on a quiet machine reaches APID_LISTENING in
# 60-66s on x64 and both readiness signals in 65-72s. That measures the daemon
# answering, not a login prompt; this harness never waits for a login prompt.
#
# BOTH BOARDS, MEASURED THE SAME WAY on 2026-09-06, wall clock from the
# `docker run` to the APID_LISTENING line, fresh disk, 5s polling:
#
#   x64          75s   (guest 39.3s)   qemu-system-x86_64 -machine q35, OVMF
#   virt-arm64   95s   (guest 57.8s)   qemu-system-aarch64 -machine virt, AAVMF
#
# So the arm64 board costs 1.27x the amd64 one under TCG -- NOT the order of
# magnitude an emulated foreign architecture invites you to assume, and the
# reason PLAN-085 made this a measurement rather than a predicted multiplier.
# The gap is smaller than it looks even so: the guest-time figures differ by
# 18.5s while the wall-clock ones differ by 20s, so most of the difference is
# the guest's own work and not the firmware.
#
# READY_TIMEOUT IS NOT CHANGED FOR virt-arm64, and that is the conclusion the
# measurement supports rather than a decision taken around it: 900s over a
# measured 95s is 9.5x headroom, so a board-specific deadline would be
# machinery for a problem that does not exist.
#
# READY_TIMEOUT stays at 900s regardless, because the deadline exists for the
# bad case rather than the measured one: a contended host is materially slower
# and goes long stretches without printing a line, indistinguishable from a
# stall unless the waiting loop says what it is doing. Hence a generous
# deadline, an override, and progress lines carrying the elapsed time and the
# last thing the console said.
READY_TIMEOUT="${MOS_APID_READY_TIMEOUT:-900}"
CONTAINER_TIMEOUT="${MOS_APID_CONTAINER_TIMEOUT:-240}"
POLL_INTERVAL="${MOS_APID_POLL_INTERVAL:-5}"
PROGRESS_INTERVAL="${MOS_APID_PROGRESS_INTERVAL:-15}"

# The QEMU-side backstops. RUN_SECONDS is when src/qemu.ts presses the
# virtual power button; TIMEOUT is when it gives up on the container entirely,
# and it must exceed RUN_SECONDS by more than the 90s grace the boot engine
# allows a guest which ignores ACPI. Both are generous because the normal end of a run
# is this script tearing the container down after the suite, not a backstop
# firing -- and a backstop firing mid-suite looks exactly like apid dying.
RUN_SECONDS="${MOS_QEMU_RUN_SECONDS:-2400}"
QEMU_TIMEOUT="${MOS_QEMU_TIMEOUT:-2700}"

# An empty selection runs the entire registry in src/main.ts. Keep no second
# phase list here: adding a phase must include it in the default QEMU run.
# Explicit MOS_APID_PHASES remains available for focused debugging.
PHASES="${MOS_APID_PHASES:-}"
# The bun image is pinned by digest, not by tag. `oven/bun:1` is a
# major-version tag upstream repoints onto every 1.x release, and this harness
# is what decides whether apid's API is judged conformant, so the default is the
# digest build-env/images.env records. MOS_APID_BUN_IMAGE overrides it.
BUN_IMAGE="${MOS_APID_BUN_IMAGE:-$(bash "${REPO_ROOT}/build-env/from.sh" --ref IMAGE_BUN_1)}"
KEEP_DISK="${MOS_APID_KEEP_DISK:-0}"

# The daemon socket is MOUNTED into the container the boot engine runs in, so it
# has to be a socket on this host. A DOCKER_HOST naming a TCP daemon is a
# different arrangement -- the container would need the variable, not a mount --
# and guessing which one a caller meant is how a run comes to talk to a daemon
# nobody chose. Same rule, and the same two cases, as verify/run.sh's.
case "${DOCKER_HOST:-}" in
"") DOCKER_SOCK=/var/run/docker.sock ;;
unix://*) DOCKER_SOCK="${DOCKER_HOST#unix://}" ;;
*) DOCKER_SOCK="" ;;
esac

DRY_RUN=0
case "${1:-}" in
--dry-run) DRY_RUN=1 ;;
"") ;;
*) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac

# --- reporting, in the register the verification contract uses ------------------
# One PASS/FAIL line per assertion and a final RESULT with dynamic totals. The
# totals are counted, never written down: a hand-maintained constant stops being
# true the first time somebody adds a check, and the obvious repair -- "no FAIL
# lines means success" -- is equally true of a run in which nothing executed at
# all. Zero checks is a failure here, and the RESULT line says so.
CHECKS_PASSED=0
CHECKS_FAILED=0
pass() { CHECKS_PASSED=$((CHECKS_PASSED + 1)); echo "PASS: $*"; }
fail() { CHECKS_FAILED=$((CHECKS_FAILED + 1)); echo "FAIL: $*"; }
note() { echo "note: $*"; }

# --- docker observation -----------------------------------------------------
# Every template below is deliberately free of Go template variables. `range`
# over a map with no variable binds the dot to the value, which is all these
# need, and it keeps the format strings free of `$` -- which inside a shell
# script would otherwise mean a shellcheck suppression on every one of them.

# Running containers whose bind mounts resolve to the shared run directory.
# Prints `<short-id> <name>` per holder, nothing when there are none.
run_dir_holders() {
    local ids id src resolved name mounts
    ids="$(docker ps -q 2>/dev/null)" || return 0
    [ -n "${ids}" ] || return 0
    while read -r id; do
        [ -n "${id}" ] || continue
        mounts="$(docker inspect "${id}" --format '{{range .Mounts}}{{.Source}}{{"\n"}}{{end}}' 2>/dev/null)" || continue
        while read -r src; do
            [ -n "${src}" ] || continue
            resolved="$(readlink -f "${src}" 2>/dev/null)" || continue
            [ "${resolved}" = "${RUN_DIR_REAL}" ] || continue
            name="$(docker inspect "${id}" --format '{{.Name}}' 2>/dev/null)"
            printf '%s %s\n' "${id:0:12}" "${name#/}"
            break
        done <<<"${mounts}"
    done <<<"${ids}"
}

# This container's own address, then the docker network that holds it. Two
# steps, because the second is what makes the answer an observation: the
# network is whichever one lists our address, so a session moved to a different
# network keeps working, and a session on none of them is told why rather than
# left to fail later at connect time with a bare refusal.
own_address() {
    ip -4 -o addr show dev eth0 2>/dev/null | awk '{ split($4, a, "/"); print a[1]; exit }'
}

network_holding() {
    local want="$1" net members nets
    nets="$(docker network ls --format '{{.Name}}' 2>/dev/null)" || return 1
    while read -r net; do
        [ -n "${net}" ] || continue
        members="$(docker network inspect "${net}" \
            --format '{{range .Containers}}{{.IPv4Address}}{{"\n"}}{{end}}' 2>/dev/null)" || continue
        if awk -v want="${want}" -F/ '$1 == want { found = 1 } END { exit found ? 0 : 1 }' <<<"${members}"; then
            printf '%s\n' "${net}"
            return 0
        fi
    done <<<"${nets}"
    return 1
}

# A container's address on the discovered network, looked up by name because
# the map key in `docker network inspect` is the container id and reading a map
# key needs a template variable. The name is stable for the life of the
# container. src/qemu.ts sets none, so it is docker's random name -- which is
# also why the QEMU container cannot simply be looked up by name to begin with.
address_on_network() {
    local cid name members
    cid="$1"
    name="$(docker inspect "${cid}" --format '{{.Name}}' 2>/dev/null)" || return 1
    name="${name#/}"
    members="$(docker network inspect "${NET}" \
        --format '{{range .Containers}}{{.Name}} {{.IPv4Address}}{{"\n"}}{{end}}' 2>/dev/null)" || return 1
    awk -v n="${name}" '$1 == n { split($2, a, "/"); print a[1]; exit }' <<<"${members}"
}

# --- the boot engine --------------------------------------------------------
# The TypeScript boot engine drives sibling containers from the pinned Bun
# and Docker CLI runner. Each boot reads a current signed factory disk.
#
# The repository is mounted at ITS OWN PATH and not at /w. Every `docker run`
# src/qemu.ts makes hands the daemon a path -- the disk directory, the console
# file -- and that daemon is the host's, so the containers it opens are SIBLINGS
# rather than children and a path has to mean the same thing on both sides.
# Mounted at its own path there is no prefix to rewrite and no arithmetic to go
# stale. `_out` is mounted a second time for the case this script already
# handles everywhere else: a worktree whose `_out` is a symlink into the
# checkout that built the image, whose target does not exist inside a container
# that mounted only the repository.
#
# What is deliberately NOT mounted is `RUN_DIR`. The competing-run guard and
# `find_guest` both identify the QEMU container by a bind whose source RESOLVES
# to ${RUN_DIR_REAL}; ${REPO_ROOT} and ${OUT_REAL} resolve to neither, so this
# container -- which is up for the whole of a boot -- can never be mistaken for
# the one running QEMU.
PORT_IMAGE=""
resolve_port_image() {
    local cli
    cli="$(bash "${REPO_ROOT}/build-env/from.sh" --ref IMAGE_DOCKER_CLI_28)" || return 1
    PORT_IMAGE="ai-agent/mos-verify-bun:$(printf '%s\n%s\n' "${BUN_IMAGE}" "${cli}" | sha256sum | cut -c1-16)"
    PORT_CLI_IMAGE="${cli}"
    return 0
}

# Built on demand and once, like verify's: nothing in `make build-env` builds
# it, so an image produced there would be missing at exactly the moment it is
# needed. One layer over two images that are already local, so it costs seconds.
build_port_image() {
    docker image inspect "${PORT_IMAGE}" >/dev/null 2>&1 && return 0
    note "building ${PORT_IMAGE} (the pinned bun plus the pinned docker client)"
    docker build -q --label ai-agent=true \
        --build-arg "MOS_BUN_IMAGE=${BUN_IMAGE}" \
        --build-arg "MOS_DOCKER_CLI_IMAGE=${PORT_CLI_IMAGE}" \
        -t "${PORT_IMAGE}" -f "${REPO_ROOT}/verify/Dockerfile" "${REPO_ROOT}/verify" >/dev/null
}

# How the boot engine is invoked, and the only place that decides. Both call
# sites go through here, so the mounts, the socket and the environment cannot
# differ between the prepare and the boots -- which is the one way a disk gets
# prepared with one set of kernel arguments and booted with another.
qemu_port() {
    local -a envargs devargs
    local kv
    envargs=()
    for kv in "${QEMU_ENV[@]}"; do envargs+=(-e "${kv}"); done
    # MOS_BOARD, EXPLICITLY. It is exported at the top of this script so that
    # src/qemu.ts "reads the same value rather than defaulting independently",
    # and that only holds for a process in this shell's environment -- the
    # engine runs in a CONTAINER, which inherits nothing. Without this the
    # engine took its own `?? "x64"` default and refused with
    # "_out/x64/x64-mos-latest.img not found" on a run that had already passed
    # its "image present" precondition against the board actually asked for.
    envargs+=(-e "MOS_BOARD=${MOS_BOARD}")
    if [ "${1:-}" = "--reuse" ]; then
        envargs+=(-e MOS_QEMU_REUSE_DISK=1)
        shift
    fi
    # /dev/kvm as THIS process sees it, passed through so that src/qemu.ts's own
    # test of it answers what this shell would have answered. Without the
    # passthrough the engine would decide on the runner image's view of /dev,
    # which is nobody's machine.
    devargs=()
    if [ -e /dev/kvm ]; then devargs+=(--device /dev/kvm); fi
    docker run --rm --label ai-agent=true --network traefik \
        -v "${REPO_ROOT}:${REPO_ROOT}" -v "${OUT_REAL}:${OUT_REAL}" \
        -v "${DOCKER_SOCK}:/var/run/docker.sock" \
        -w "${SCRIPT_DIR}" \
        ${devargs[@]+"${devargs[@]}"} "${envargs[@]}" \
        "${PORT_IMAGE}" bun run src/qemu.ts "$@"
}

# --- teardown ---------------------------------------------------------------
# Idempotent, and run from both the normal path and the EXIT trap, so the last
# line of stdout is always the RESULT rather than a teardown note that arrived
# after it. An argument error and a failure nine minutes into a boot therefore
# leave the same amount behind: nothing running, and every console log still on
# disk. The logs are the evidence and are never removed; the 4 GiB disk is not
# evidence, and is removed unless asked for.
QEMU_CID=""
QEMU_PID=""
PREPARED=0
TORN_DOWN=0
teardown() {
    local id
    [ "${TORN_DOWN}" -eq 0 ] || return 0
    TORN_DOWN=1
    if [ -n "${QEMU_PID}" ] && kill -0 "${QEMU_PID}" 2>/dev/null; then
        kill "${QEMU_PID}" 2>/dev/null || true
    fi
    if [ -n "${QEMU_CID}" ] && docker inspect "${QEMU_CID}" >/dev/null 2>&1; then
        note "stopping the QEMU container ${QEMU_CID:0:12}"
        docker stop -t 10 "${QEMU_CID}" >/dev/null 2>&1 || true
    elif [ "${PREPARED}" -eq 1 ]; then
        # The container came up but was never identified -- the run that would
        # otherwise leave one behind holding the shared directory. Anything
        # holding it now is ours: the precondition proved nothing held it when
        # this run started.
        while read -r id _; do
            [ -n "${id}" ] || continue
            note "stopping the QEMU container ${id} (found by its hold on the run directory)"
            docker stop -t 10 "${id}" >/dev/null 2>&1 || true
        done <<<"$(run_dir_holders)"
    fi
    if [ "${PREPARED}" -eq 1 ] && [ "${KEEP_DISK}" != "1" ]; then
        rm -f "${RUN_DIR}/disk.img"
    fi
}

finish() {
    local total
    teardown
    total=$((CHECKS_PASSED + CHECKS_FAILED))
    if [ "${CHECKS_FAILED}" -eq 0 ] && [ "${total}" -gt 0 ]; then
        echo "RESULT: PASS (${CHECKS_PASSED}/${total} checks)"
        exit 0
    fi
    echo "RESULT: FAIL (${CHECKS_PASSED}/${total} checks)"
    exit 1
}

# --- 1. preconditions -------------------------------------------------------
note "repository ${REPO_ROOT}"

if [ ! -e "${IMG}" ]; then
    fail "image ${IMG##*/} is missing; this harness builds nothing. Build a complete image with build/run.sh --components image"
    finish
fi
pass "image present: ${IMG##*/} -> $(basename "$(readlink -f "${IMG}")")"

if ! docker info >/dev/null 2>&1; then
    fail "docker is not usable from here; QEMU, the readiness probe and the bun suite all run in containers"
    finish
fi
pass "docker is usable"

if [ -z "${DOCKER_SOCK}" ]; then
    fail "DOCKER_HOST=${DOCKER_HOST} is not a unix:// socket, and the boot engine reaches the daemon by MOUNTING one into the container it runs in. Run this where the daemon has a unix socket."
    finish
fi
if [ ! -S "${DOCKER_SOCK}" ]; then
    fail "${DOCKER_SOCK} is not a socket, so the boot engine would start with no daemon to reach and fail later making the QEMU container"
    finish
fi
pass "the daemon socket to mount into the boot engine: ${DOCKER_SOCK}"

if ! resolve_port_image; then
    fail "build-env/from.sh could not resolve IMAGE_DOCKER_CLI_28; the boot engine needs bun and a docker client in one image and that key is the client half"
    finish
fi
pass "boot engine image: ${PORT_IMAGE} ($(docker image inspect "${PORT_IMAGE}" >/dev/null 2>&1 && echo "present" || echo "built at first use from verify/Dockerfile"))"

HOLDERS="$(run_dir_holders)"
if [ -n "${HOLDERS}" ]; then
    fail "another container already binds ${RUN_DIR_REAL}: ${HOLDERS//$'\n'/, }"
    note "  that is the boot engine's single fixed run directory, and a checkout whose _out"
    note "  resolves here shares it. Continuing would overwrite its disk.img underneath a"
    note "  running boot, so this run refuses rather than clobbering it. Wait for it to finish."
    finish
fi
pass "no competing run: nothing binds ${RUN_DIR_REAL}"

if [ ! -f "${SCRIPT_DIR}/src/main.ts" ]; then
    # Said here rather than left to the container: inside it the same situation
    # prints `Module not found`, which on this host is also what a bind mount
    # that did not propagate looks like. Not fatal at this point -- the boot
    # half of the harness is still worth running and is still checkable without
    # the suite.
    note "pkgs/mosd/tests/apid-api/src/main.ts does not exist yet; the suite step will FAIL loudly rather than be skipped quietly"
fi

# --- 2. discover the network by observation ---------------------------------
# Three doors sit between here and apid and each fails as "connection refused"
# with nothing to say which was shut: QEMU's user-mode `hostfwd` binds inside
# the container running QEMU; that container must publish the port, which
# src/qemu.ts does; and `-p 127.0.0.1:...` publishes on the docker
# host's loopback, which is not this container's and has no route to it -- a
# containerised session sits on its own docker network while a plain
# `docker run` lands on the default bridge. So the guest's address is the QEMU
# container's own address on a network this script shares with it, discovered
# rather than named: the script reads its own eth0 address and asks each docker
# network whether it holds it. Hardcoding a network name would work on one host
# and nowhere else, and `hostname` is the container's short id about as often as
# it is a name.
MY_IP="$(own_address)"
if [ -z "${MY_IP}" ]; then
    fail "could not read this container's own eth0 address; without it the docker network holding this session cannot be identified, and the guest then has no address that is reachable from here"
    finish
fi
if ! NET="$(network_holding "${MY_IP}")"; then
    fail "no docker network lists ${MY_IP}, so the suite cannot route to the guest: QEMU publishes its forward on the DOCKER HOST's loopback, which is not this container's, and a shared network is the only way in"
    finish
fi
pass "docker network discovered by observation: ${NET} (this container is ${MY_IP})"

mkdir -p "${ART_DIR}"
# A DATA-seeded service forwards the volatile journal to the captured console.
CONSOLE1="${ART_DIR}/console-boot1.log"
# The path the suite is given has to resolve inside the bun container, which
# mounts the repository root at /w. _out is bound over the top of it a second
# time so that a checkout whose _out is a symlink -- a worktree borrowing the
# artefacts of the checkout that built them -- does not hand the suite a
# dangling link, because the symlink's target does not exist in that container.
# Where _out is a real directory the second bind is the same directory twice
# and costs nothing.
ART_IN_CONTAINER="/w/_out/${MOS_BOARD}/apid-api"

SMOKE_IN_GUEST=/state/m7-net-smoke.sh

if [ "${DRY_RUN}" -eq 1 ]; then
    note "--dry-run: nothing will be booted"
    note "would prepare  ${RUN_DIR}/disk.img from ${IMG##*/} (src/qemu.ts --prepare-only, in ${PORT_IMAGE})"
    note "would boot     src/qemu.ts --capture ${CONSOLE1}"
    note "               MOS_QEMU_FORWARD=1 MOS_QEMU_NETWORK=${NET}"
    note "               MOS_QEMU_IMAGE=${IMG} MOS_QEMU_BOOT_CERT=${BOOT_CERT}"
    note "would seed     pkgs/mosd/tests/apid-api/guest/m7-net-smoke.sh -> DATA:${SMOKE_IN_GUEST} (phase 05c)"
    note "               MOS_QEMU_RUN_SECONDS=${RUN_SECONDS} MOS_QEMU_TIMEOUT=${QEMU_TIMEOUT}"
    note "would find     the container binding ${RUN_DIR_REAL} and read its address on ${NET}"
    note "would wait     up to ${READY_TIMEOUT}s for APID_LISTENING on the console AND for"
    note "               https://<guest>:${HTTPS_PORT}/healthz to answer 200 from inside ${BUN_IMAGE}"
    note "would run      docker run --network ${NET} -v ${REPO_ROOT}:/w -v ${OUT_REAL}:/w/_out -w /w/pkgs/mosd/tests/apid-api ${BUN_IMAGE} bun run src/main.ts"
    note "               APID_HOST=<guest> APID_HTTPS_PORT=${HTTPS_PORT} APID_HTTP_PORT=${HTTP_PORT}"
    if [ -n "${APID_NEGATIVE:-}" ]; then
        note "               APID_NEGATIVE=${APID_NEGATIVE} -- this run is EXPECTED TO BE RED"
    fi
    note "               APID_CONSOLE=${ART_IN_CONTAINER}/console-boot1.log APID_PHASES=${PHASES:-<all>}"
    finish
fi

trap 'teardown' EXIT

# Prepare a disposable disk and seed services through the persistent unit path.
QEMU_ENV=(
    MOS_QEMU_IMAGE="$IMG"
    MOS_QEMU_BOOT_CERT="$BOOT_CERT"
    MOS_QEMU_FORWARD=1
    MOS_QEMU_NETWORK="${NET}"
    MOS_QEMU_HTTPS_PORT="${HTTPS_PORT}"
    MOS_QEMU_HTTP_PORT="${HTTP_PORT}"
    MOS_QEMU_RUN_SECONDS="${RUN_SECONDS}"
    MOS_QEMU_TIMEOUT="${QEMU_TIMEOUT}"
)
if [ "${MOS_QEMU_SSH_PORT+x}" = x ]; then
    QEMU_ENV+=("MOS_QEMU_SSH_PORT=${MOS_QEMU_SSH_PORT}")
fi

if ! bash "$REPO_ROOT/tests/signed-boot-lab/images.sh" --lifecycle > "$ART_DIR/qemu-image.log" 2>&1; then
    fail "could not build the pinned Secure Boot QEMU runner"
    finish
fi

if ! build_port_image; then
    fail "could not build ${PORT_IMAGE} from verify/Dockerfile; it is two pinned FROMs and one COPY, and nothing is fetched beyond those two images"
    finish
fi

note "preparing the disk from ${IMG##*/} (a ~2 GiB copy; nothing boots yet)"
if ! qemu_port --prepare-only >"${ART_DIR}/prepare.log" 2>&1; then
    PREPARED=1  # a partial copy still has to be cleaned up
    fail "src/qemu.ts --prepare-only failed; see ${ART_DIR}/prepare.log"
    tail -n 20 "${ART_DIR}/prepare.log" >&2 || true
    finish
fi
PREPARED=1
pass "disk prepared at ${RUN_DIR}/disk.img"

SMOKE_SRC="${SCRIPT_DIR}/guest/m7-net-smoke.sh"
cat > "$ART_DIR/mos-api-console.service" <<'UNIT'
[Unit]
Description=API acceptance console journal
After=mosd.service
[Service]
ExecStart=/usr/bin/journalctl --no-pager -b -n all -f -o cat
StandardOutput=tty
StandardError=tty
TTYPath=/dev/console
[Install]
WantedBy=multi-user.target
UNIT
cat > "$ART_DIR/mos-api-network.service" <<'UNIT'
[Unit]
Description=API acceptance network checks
Requires=mosd.service
After=mosd.service
[Service]
Type=oneshot
Environment=M7_AFTER_MOSD=1
ExecStart=/bin/bash /mnt/data/state/m7-net-smoke.sh
StandardOutput=journal+console
StandardError=journal+console
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
UNIT
if ! qemu_port --seed \
    "$SMOKE_SRC" "$SMOKE_IN_GUEST" \
    "$ART_DIR/mos-api-console.service" /state/systemd-units/mos-api-console.service \
    "$ART_DIR/mos-api-network.service" /state/systemd-units/mos-api-network.service \
    --enable mos-api-console.service --enable mos-api-network.service > "$ART_DIR/seed.log" 2>&1; then
    fail "current DATA seeding failed; see $ART_DIR/seed.log"
    tail -n 20 "$ART_DIR/seed.log" >&2
    finish
fi
pass "seeded API console and network test units into DATA"

launch_boot() {
    local label="$1" console="$2"
    : >"${console}"
    QEMU_CID=""
    qemu_port --reuse --capture "${console}" \
        >"${ART_DIR}/launch-${label}.log" 2>&1 &
    QEMU_PID=$!
    note "[${label}] QEMU launched in the background (pid ${QEMU_PID}); console -> ${console##*/}"
}

# --- 4. find the guest ------------------------------------------------------
# Two conditions, held apart because they fail for different reasons and the
# message has to say which. src/qemu.ts starts its container with `--rm` and
# no `--name`, so the only handle on it is the bind mount -- and the same mount
# is held for a moment by the short-lived mtools containers that read and write
# the kernel append in the ESP, which are not on the discovered network. Requiring
# an address on that network is what tells the two apart; matching on the mount
# alone latches onto the wrong container and then reports "no address on that
# network" about a container that was never going to have one.
GUEST_CID=""
GUEST_IP=""
find_guest() {
    local label="$1" start elapsed last_report holders holder cid ip
    start="${SECONDS}"
    last_report=0
    while :; do
        elapsed=$((SECONDS - start))
        holders="$(run_dir_holders)"
        holder="${holders%%$'\n'*}"
        if [ -n "${holder}" ]; then
            cid="${holder%% *}"
            ip="$(address_on_network "${cid}")"
            if [ -n "${ip}" ]; then
                GUEST_CID="${cid}"
                GUEST_IP="${ip}"
                QEMU_CID="${cid}"
                return 0
            fi
        fi
        if [ "${elapsed}" -ge "${CONTAINER_TIMEOUT}" ]; then
            if [ -z "${holder}" ]; then
                fail "[${label}] no container bound ${RUN_DIR_REAL} within ${CONTAINER_TIMEOUT}s: QEMU never started"
            else
                fail "[${label}] container ${holder} is up but still has no address on ${NET} after ${CONTAINER_TIMEOUT}s: the forward may exist, but nothing here can route to it"
            fi
            tail -n 20 "${ART_DIR}/launch-${label}.log" >&2 2>/dev/null || true
            return 1
        fi
        if [ $((elapsed - last_report)) -ge "${PROGRESS_INTERVAL}" ]; then
            last_report="${elapsed}"
            if [ -z "${holder}" ]; then
                note "[${label}] ${elapsed}s/${CONTAINER_TIMEOUT}s waiting for the QEMU container to bind the run directory"
            else
                note "[${label}] ${elapsed}s/${CONTAINER_TIMEOUT}s container ${holder%% *} is up; waiting for its address on ${NET}"
            fi
        fi
        sleep "${POLL_INTERVAL}"
    done
}

# --- 5. wait for apid, and make the waiting legible -------------------------
# Both signals, because either alone is a different claim: APID_LISTENING says
# the daemon reached the point in its own start-up where it binds, and a 200
# from /healthz says the three doors between here and that socket are open.
# /healthz is the probe because it is the dedicated listener-health contract;
# a static UI response does not prove the management backend is ready. Redirects
# are not followed -- apid's :80 -> :443 redirect names the guest's port
# 443, which is not followable through a port forward, and a client following it
# blindly hangs in a way that reads as apid being down.
# shellcheck disable=SC2016  # this is JavaScript: ${process.env.H} and the
# backtick template are for bun to expand, not the shell. Substituting them
# here would bake this run's values into a string that is then evaluated in a
# different process, which is how a probe ends up asking about the wrong host.
HEALTHZ_JS='
const url = `https://${process.env.H}:${process.env.P}/healthz`;
let out;
try {
  const r = await fetch(url, { redirect: "manual", tls: { rejectUnauthorized: false } });
  out = { status: r.status, body: (await r.text()).slice(0, 120) };
} catch (e) {
  out = { status: 0, error: String(e && e.message ? e.message : e) };
}
console.log(JSON.stringify(out));
process.exit(out.status === 200 ? 0 : 1);
'

# The probe runs in the bun image on the discovered network -- the suite's own
# runtime over the suite's own path -- so a green wait is evidence about the
# thing that is about to run rather than about this shell's networking. apid's
# certificate is self-signed, so verification is switched off explicitly and
# only here: measured 2026-08-24, bun rejects that certificate by default with
# "self signed certificate", which is the right default and the reason the
# opt-out is written down rather than inherited from an environment variable.
probe_healthz() {
    docker run --rm --label ai-agent=true --network "${NET}" \
        -e H="$1" -e P="${HTTPS_PORT}" \
        "${BUN_IMAGE}" bun -e "${HEALTHZ_JS}" >"${ART_DIR}/healthz.last" 2>&1
}

console_tail_line() {
    tail -n 1 "$1" 2>/dev/null | tr -d '\r' | tr -dc '[:print:]' | cut -c1-100
}

# The console log is appended to across a reboot: the second boot of a guest
# that reset in place writes into the same file, under the first boot's
# APID_LISTENING line. A whole-file grep therefore answers "apid is listening"
# using a line the previous boot wrote -- measured 2026-08-24, where it declared
# the guest ready 0s after a reboot that had just taken it down, and then spent
# its whole deadline waiting for a /healthz that could not come.
#
# So every wait is anchored: `from` is the console's size at the moment the wait
# began, and only bytes after it are searched. Boot 1's file is truncated by
# launch_boot, so its anchor is 0 and nothing changes for it.
console_size() {
    stat -c %s "$1" 2>/dev/null || echo 0
}

console_since() {
    local file="$1" from="$2"
    tail -c "+$((from + 1))" "${file}" 2>/dev/null
}

wait_for_apid() {
    local label="$1" console="$2" ip="$3" from="${4:-0}" start elapsed last_report console_seen hit
    start="${SECONDS}"
    last_report=0
    console_seen=0
    while :; do
        elapsed=$((SECONDS - start))
        if [ "${console_seen}" -eq 0 ]; then
            hit="$(console_since "${console}" "${from}" | grep -m1 'APID_LISTENING' || true)"
            if [ -n "${hit}" ]; then
                console_seen=1
                pass "[${label}] APID_LISTENING on the console after ${elapsed}s: $(printf '%s' "${hit}" | tr -d '\r' | tr -dc '[:print:]' | cut -c1-120)"
            fi
        fi
        if [ "${console_seen}" -eq 1 ] && probe_healthz "${ip}"; then
            pass "[${label}] https://${ip}:${HTTPS_PORT}/healthz answered 200 from inside ${BUN_IMAGE} after ${elapsed}s: $(cut -c1-160 "${ART_DIR}/healthz.last")"
            return 0
        fi
        if [ "${elapsed}" -ge "${READY_TIMEOUT}" ]; then
            if [ "${console_seen}" -eq 0 ]; then
                fail "[${label}] APID_LISTENING never appeared on the console within ${READY_TIMEOUT}s"
            else
                fail "[${label}] apid announced itself but https://${ip}:${HTTPS_PORT}/healthz never answered 200 within ${READY_TIMEOUT}s: $(cut -c1-160 "${ART_DIR}/healthz.last" 2>/dev/null)"
            fi
            # The tail of the console goes out before anything unwinds,
            # because the container is about to be stopped and the reader
            # would otherwise be told only that it took too long.
            echo "--- last 40 lines of ${console} ---" >&2
            tail -n 40 "${console}" 2>/dev/null | tr -d '\r' >&2 || true
            echo "--- end of console ---" >&2
            return 1
        fi
        if [ $((elapsed - last_report)) -ge "${PROGRESS_INTERVAL}" ]; then
            last_report="${elapsed}"
            if [ "${console_seen}" -eq 0 ]; then
                note "[${label}] ${elapsed}s/${READY_TIMEOUT}s waiting for APID_LISTENING | console: $(console_tail_line "${console}")"
            else
                note "[${label}] ${elapsed}s/${READY_TIMEOUT}s apid announced; waiting for /healthz | last: $(cut -c1-100 "${ART_DIR}/healthz.last" 2>/dev/null)"
            fi
        fi
        sleep "${POLL_INTERVAL}"
    done
}

# --- 6. run the suite -------------------------------------------------------
# The repository root is mounted, never a temporary directory: measured
# 2026-08-24, a /tmp bind does not propagate to the docker daemon on this host,
# which is a sibling-container arrangement, so the container sees an empty
# directory and says `Module not found` -- which reads like a bug in the suite
# and is not.
#
# APID_NEGATIVE is forwarded when the caller sets it and omitted otherwise.
# It is how a live run is made to go red on demand: the selftest proves the machinery
# can fail offline, and this proves it can fail against the actual guest.
# Without the forward, `APID_NEGATIVE=... make os-apid-api-test` runs green and
# looks like the inversion had been applied.
SUITE_RC=0
suite_passthrough() {
    local -n out="$1"
    out=()
    [ -n "${APID_NEGATIVE:-}" ] && out+=(-e "APID_NEGATIVE=${APID_NEGATIVE}")
    return 0
}

run_suite() {
    local label="$1" ip="$2" console_name="$3" phases="$4" log rc p f
    local -a passthrough
    suite_passthrough passthrough
    log="${ART_DIR}/suite-${label}.log"
    if [ ! -f "${SCRIPT_DIR}/src/main.ts" ]; then
        fail "[${label}] the suite entry point pkgs/mosd/tests/apid-api/src/main.ts does not exist, so nothing was asserted about apid"
        SUITE_RC=1
        return 0
    fi
    note "[${label}] running the suite against ${ip}:${HTTPS_PORT}${phases:+ (phases: ${phases})}"
    set +e
    docker run --rm --label ai-agent=true --network "${NET}" \
        -v "${REPO_ROOT}:/w" -v "${OUT_REAL}:/w/_out" -w /w/pkgs/mosd/tests/apid-api \
        -e APID_HOST="${ip}" \
        -e APID_HTTPS_PORT="${HTTPS_PORT}" \
        -e APID_HTTP_PORT="${HTTP_PORT}" \
        -e APID_CONSOLE="${ART_IN_CONTAINER}/${console_name}" \
        -e APID_RESULT_JSON="${ART_IN_CONTAINER}/result-${label}.json" \
        -e APID_PHASES="${phases}" \
        ${passthrough[@]+"${passthrough[@]}"} \
        "${BUN_IMAGE}" bun run src/main.ts 2>&1 | tee "${log}"
    rc="${PIPESTATUS[0]}"
    set -e
    # The suite's own PASS/FAIL lines are folded into this run's totals, so the
    # final RESULT counts assertions and not scripts. An exit code with no FAIL
    # line behind it is recorded as its own failure: a suite that died before
    # asserting anything must not be able to leave a clean report.
    p="$(grep -c '^PASS:' "${log}" 2>/dev/null || true)"
    f="$(grep -c '^FAIL:' "${log}" 2>/dev/null || true)"
    CHECKS_PASSED=$((CHECKS_PASSED + p))
    CHECKS_FAILED=$((CHECKS_FAILED + f))
    note "[${label}] suite exited ${rc} with ${p} PASS and ${f} FAIL lines"
    if [ "${rc}" -ne 0 ] && [ "${f}" -eq 0 ]; then
        fail "[${label}] the suite exited ${rc} without emitting a single FAIL line; it did not get far enough to assert anything"
    fi
    [ "${rc}" -eq 0 ] || SUITE_RC="${rc}"
    return 0
}

# --- the run ----------------------------------------------------------------
BOOT1_START="${SECONDS}"
launch_boot boot1 "${CONSOLE1}"
find_guest boot1 || finish
pass "boot1 guest found: container ${GUEST_CID} at ${GUEST_IP} on ${NET} after $((SECONDS - BOOT1_START))s"
wait_for_apid boot1 "${CONSOLE1}" "${GUEST_IP}" || finish
note "boot1 was ready $((SECONDS - BOOT1_START))s after launch"

run_suite boot1 "${GUEST_IP}" "console-boot1.log" "${PHASES}"

# --- 7. one machine-readable result for the whole run -----------------------
# The envelope is this harness's; what each boot wrote is embedded verbatim and
# is not reinterpreted here. The suite owns the shape of its own file, and a
# merger that reached inside it would have to be changed in step with it -- and
# would silently produce zeros on the day it was not.
MERGED="${ART_DIR}/result.json"
# The image identity goes in the envelope, because a result file that does not
# say which artefact it covered is a result file that cannot be trusted a week
# later. `<board>-mos-latest.img` is a symlink and its target changes under it
# every time somebody builds; the resolved name and the mtime are what pin a run
# to a surface. When a route check fails, this identity says whether the image
# moved under the source tree or the contract did.
IMG_RESOLVED="$(readlink -f "${IMG}" 2>/dev/null || echo "${IMG}")"
IMG_MTIME_EPOCH="$(stat -c %Y "${IMG_RESOLVED}" 2>/dev/null || echo 0)"
IMG_MTIME_ISO="$(date -u -d "@${IMG_MTIME_EPOCH}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || echo unknown)"
IMG_BYTES="$(stat -c %s "${IMG_RESOLVED}" 2>/dev/null || echo 0)"
{
    printf '{\n'
    printf '  "image": {\n'
    printf '    "latest": "%s",\n' "${IMG##*/}"
    printf '    "resolved": "%s",\n' "${IMG_RESOLVED##*/}"
    printf '    "resolvedPath": "%s",\n' "${IMG_RESOLVED}"
    printf '    "mtime": "%s",\n' "${IMG_MTIME_ISO}"
    printf '    "mtimeEpoch": %s,\n' "${IMG_MTIME_EPOCH}"
    printf '    "bytes": %s\n' "${IMG_BYTES}"
    printf '  },\n'
    printf '  "boots": [\n'
    sep=""
    for label in boot1; do
        rf="${ART_DIR}/result-${label}.json"
        [ -f "${rf}" ] || continue
        printf '%s    { "boot": "%s", "result": ' "${sep}" "${label}"
        cat "${rf}"
        printf ' }'
        sep=$',\n'
    done
    printf '\n  ],\n'
    printf '  "passed": %d,\n  "failed": %d,\n  "checks": %d\n}\n' \
        "${CHECKS_PASSED}" "${CHECKS_FAILED}" "$((CHECKS_PASSED + CHECKS_FAILED))"
} >"${MERGED}"
note "merged result written to ${MERGED}"
note "image under test: ${IMG_RESOLVED##*/} (mtime ${IMG_MTIME_ISO})"
note "console log kept: ${CONSOLE1}"

if [ "${SUITE_RC}" -ne 0 ] && [ "${CHECKS_FAILED}" -eq 0 ]; then
    fail "the suite exited ${SUITE_RC} but no assertion recorded a failure"
fi

finish
