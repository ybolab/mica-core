#!/bin/bash
# the kernel half of VLAN, bridge and WireGuard, proved by USING it
# on a booted device rather than by reading a config file.
#
# This runs INSIDE the guest. `verify`'s two kernel checks read
# `/boot/config-*` and the module indexes out of an unpacked squashfs, which is
# a claim about what the image CONTAINS; nothing offline can answer whether the
# running kernel will actually hand back a device. `ip link add` is the only
# thing that asks that question, and it has to be asked where the kernel is.
#
# Every conclusion is one `M7-SMOKE: <id> PASS|FAIL <detail>` line on the
# console, which is the harness's only channel into this guest: the image keeps
# journald at `Storage=volatile` and there is no login and no route in. The
# suite reads these lines back out of the captured serial log.
#
# It creates nothing that outlives it. The three links are torn down in the
# order they were made, and the key fixture is removed, because a device left
# behind would be indistinguishable from one mosd rendered and would be swept
# by the next reconcile.

set -u

STATE_DIR=/var/lib/mos
KEY_DIR="${STATE_DIR}/networkd-secrets"
SECRETS_DIR="${STATE_DIR}/secrets"
NET_USER=systemd-network

# Names nothing else in this suite writes; a
# collision would make one phase's teardown another phase's failure.
VLAN_ID=4094
BRIDGE_DEV=m7br0
WG_DEV=m7wg0

say() { echo "M7-SMOKE: $*"; }
pass() { say "$1 PASS ${2-}"; }
fail() { say "$1 FAIL ${2-}"; }

# `set -u` plus a subshell would hide a failure; each check reports its own
# verdict and none of them aborts the script, because a run that stopped at the
# first red would report nothing about the checks after it and the console would
# look identical to a run that never started.

# `wait_for <seconds> <command...>`: poll until the command succeeds.
#
# Three things this script needs are produced by other units racing it, and on
# a TCG guest it wins that race routinely: STATE is mounted by local-fs.target,
# a network interface is named by systemd-networkd, and <state>/secrets/ is
# created by mosd's first-boot provisioning. Measured 2026-08-28: this unit
# reached its first check 54s into the boot with none of the three ready, and
# every conclusion drawn then described the CLOCK rather than the image.
# The limits are SHORT because this unit runs after sysinit.target and holds
# the rest of the boot while it runs: every second spent here is a second mosd,
# apid and the suite's own readiness deadline do not get.
wait_for() {
    local limit="$1"; shift
    local i=0
    while [ "$i" -lt "$limit" ]; do
        if "$@" >/dev/null 2>&1; then return 0; fi
        i=$((i + 1))
        sleep 1
    done
    return 1
}

state_is_mounted() { mountpoint -q "${STATE_DIR}"; }

# Any interface that is not the loopback, up or down. A VLAN's parent has to
# EXIST; it does not have to be up, and it does not have to carry a route --
# `ip link add link <parent> ... type vlan` is accepted on a down parent. An
# earlier form of this looked for the default route, which is a fact about
# DHCP having completed and not about the kernel supporting VLANs.
first_real_link() {
    ip -o link show 2>/dev/null |
        awk -F': ' '$2 != "lo" && $2 != "" {print $2; found=1; exit} END {exit !found}'
}

say "BEGIN $(uname -r)"

if wait_for 60 state_is_mounted; then
    pass state-mounted "${STATE_DIR} is a mount point"
else
    fail state-mounted "${STATE_DIR} was not mounted within 60s; the key-store checks below cannot run"
fi

# 1. modprobe resolution, against the running kernel's own view. `-n` is a dry
# run: it resolves the name and the dependency chain and loads nothing, which is
# what makes this safe to run before the link checks that need the real thing.
for m in 8021q bridge wireguard; do
    if out=$(modprobe -n "$m" 2>&1); then
        pass "modprobe-${m}" "modprobe -n ${m} resolved${out:+ (${out})}"
    else
        fail "modprobe-${m}" "modprobe -n ${m} failed: ${out}"
    fi
done

# 2. The links. A VLAN needs a declared parent, so the parent is DISCOVERED
# rather than written down: a name in this file would make the check fail on a
# guest whose NIC is enumerated differently, which is a fact about the host's
# QEMU and not about the kernel under test.
#
# Nothing here disturbs the parent. A VLAN is a new device hanging off it, the
# bridge takes no ports, and the tunnel is created from nothing -- enslaving the
# parent to the bridge would drop the port forward and take the rest of the
# suite with it.
wait_for 30 first_real_link || true
PARENT=$(first_real_link || true)

VLAN_DEV="${PARENT}.${VLAN_ID}"

link_check() {
    local id="$1" dev="$2"; shift 2
    local out
    if ! out=$("$@" 2>&1); then
        fail "$id" "ip link add ${dev} failed: ${out}"
        return
    fi
    if out=$(ip -o link show dev "${dev}" 2>&1); then
        pass "$id" "${out}"
    else
        # `ip link add` exited 0 and the device is not there. Reported as its
        # own sentence because the repair is not the same one: the command
        # succeeding and the device not existing is a kernel that accepted the
        # netlink message and created nothing.
        fail "$id" "ip link add ${dev} exited 0 but the device is absent: ${out}"
    fi
    ip link del "${dev}" >/dev/null 2>&1
}

if [ -z "${PARENT}" ]; then
    fail link-vlan "no non-loopback interface appeared within 30s, so there is no parent to declare a VLAN on"
else
    link_check link-vlan "${VLAN_DEV}" \
        ip link add link "${PARENT}" name "${VLAN_DEV}" type vlan id "${VLAN_ID}"
fi
link_check link-bridge "${BRIDGE_DEV}" ip link add name "${BRIDGE_DEV}" type bridge
link_check link-wireguard "${WG_DEV}" ip link add dev "${WG_DEV}" type wireguard

# 3. The traversal check handed forward.
#
# Run AS the user, never by reading mode bits. The M5 defect the amendment
# corrected -- a key under a 0700 `secrets/` -- left every mode assertion green
# while `PrivateKeyFile=` was EACCES on every real image, because the mode of
# the key says nothing about whether its PATH can be walked. `setpriv
# --reuid/--regid --clear-groups` drops to the account systemd-networkd runs as
# and the kernel answers the question that mode bits cannot.
if ! id "${NET_USER}" >/dev/null 2>&1; then
    fail netuser-exists "no ${NET_USER} account on this image; mosd's key store chowns to a group that does not exist and PrivateKeyFile= could not be read by anyone but root"
else
    pass netuser-exists "$(id "${NET_USER}")"

    NET_UID=$(id -u "${NET_USER}")
    NET_GID=$(id -g "${NET_USER}")
    as_netuser() { setpriv --reuid "${NET_UID}" --regid "${NET_GID}" --clear-groups -- "$@"; }

    # The fixture is written at the production path with the production modes,
    # so what is being read is the store's own directory rather than a lookalike
    # somewhere traversable. A key mosd generated would be a stronger subject
    # still, but it exists only once a wireguard interface has been configured,
    # and a check that depended on another phase's timing would report EACCES
    # for "not created yet". Both are asserted: the fixture unconditionally, and
    # every real key beside it when there is one.
    FIXTURE="${KEY_DIR}/m7-probe.key"
    mkdir -p "${KEY_DIR}" 2>/dev/null
    chmod 0750 "${KEY_DIR}" 2>/dev/null
    chown "root:${NET_USER}" "${KEY_DIR}" 2>/dev/null
    printf 'not-a-key\n' >"${FIXTURE}" 2>/dev/null
    chmod 0640 "${FIXTURE}" 2>/dev/null
    chown "root:${NET_USER}" "${FIXTURE}" 2>/dev/null

    if out=$(as_netuser cat "${FIXTURE}" 2>&1); then
        pass keystore-readable "${NET_USER} read ${FIXTURE} (mode $(stat -c '%a %U:%G' "${FIXTURE}" 2>/dev/null)); every path component is traversable by that user"
    else
        fail keystore-readable "${NET_USER} could NOT read ${FIXTURE} (mode $(stat -c '%a %U:%G' "${FIXTURE}" 2>/dev/null)): ${out}. systemd-networkd would fail PrivateKeyFile= with the same error and every mode assertion in mosd's own tests would still be green"
    fi
    rm -f "${FIXTURE}"

    real=0
    for key in "${KEY_DIR}"/wg-*.key; do
        [ -e "${key}" ] || continue
        real=$((real + 1))
        if out=$(as_netuser cat "${key}" >/dev/null 2>&1); then
            pass keystore-real-readable "${NET_USER} read the key mosd generated at ${key} (mode $(stat -c '%a %U:%G' "${key}" 2>/dev/null))"
        else
            fail keystore-real-readable "${NET_USER} could NOT read mosd's own key at ${key} (mode $(stat -c '%a %U:%G' "${key}" 2>/dev/null)): ${out}"
        fi
    done
    [ "${real}" -eq 0 ] && say "keystore-real-readable SKIP no wg-*.key in ${KEY_DIR} on this boot"

    # The negative, and it is not decoration. If `systemd-network` could read
    # `secrets/` then the amendment's whole premise -- that a key cannot live
    # under it -- would be false, and the path correction this milestone
    # verifies would have been unnecessary. A check that only ever asserts
    # access cannot tell a correctly scoped grant from a wide-open state dir.
    # NOT a wait. This unit runs immediately after sysinit.target and BLOCKS the
    # rest of the boot -- mosd has not started yet and is the only thing that
    # creates secrets/, so waiting for it here deadlocks the two: measured
    # 2026-08-28, a 120s poll for this directory held multi-user.target for the
    # whole 120s and mosd could not run until it gave up.
    #
    # So the directory is made when it is absent, at the mode identity.rs pins
    # (0700, root-owned), and removed again. On the second boot of a disk mosd
    # has already provisioned it and the real one is used untouched -- the
    # detail below says which was read, because "the shipped directory refuses
    # this user" and "a directory with the shipped mode refuses this user" are
    # different claims and a reader must not have to guess.
    secrets_fixture=0
    if [ ! -d "${SECRETS_DIR}" ]; then
        mkdir -p "${SECRETS_DIR}" 2>/dev/null && chmod 0700 "${SECRETS_DIR}" 2>/dev/null \
            && chown root:root "${SECRETS_DIR}" 2>/dev/null \
            && printf 'not-a-password\n' >"${SECRETS_DIR}/device-password" 2>/dev/null \
            && chmod 0600 "${SECRETS_DIR}/device-password" 2>/dev/null \
            && secrets_fixture=1
    fi
    secrets_origin="the directory mosd provisioned"
    [ "${secrets_fixture}" -eq 1 ] && secrets_origin="a fixture at the mode identity.rs:44 pins, mosd not having provisioned yet on this boot"

    if [ ! -d "${SECRETS_DIR}" ]; then
        say "secrets-unreadable SKIP ${SECRETS_DIR} does not exist and could not be created on this boot"
    elif as_netuser cat "${SECRETS_DIR}/device-password" >/dev/null 2>&1; then
        fail secrets-unreadable "${NET_USER} CAN read ${SECRETS_DIR}/device-password (dir mode $(stat -c '%a %U:%G' "${SECRETS_DIR}" 2>/dev/null), ${secrets_origin}); the plaintext device password is readable by the network account"
    else
        pass secrets-unreadable "${NET_USER} cannot read ${SECRETS_DIR}/device-password (dir mode $(stat -c '%a %U:%G' "${SECRETS_DIR}" 2>/dev/null), ${secrets_origin}), which is why the key store is a SIBLING of secrets/ and not a directory under it"
    fi
    [ "${secrets_fixture}" -eq 1 ] && rm -rf "${SECRETS_DIR}"
fi

say "END"
