#!/usr/bin/env bash
# Live-bus tests for the shipped micad D-Bus policy.
#
#   bash scripts/gate/dbus-policy-test.sh
#
# The policy claims com.mica.micad is reachable only by root. Reading XML back
# proves nothing about what dbus-daemon does with it, so this harness stands up
# real dbus-daemons whose configurations include the shipped file verbatim,
# owns the name from a root connection, and drives root and non-root clients.
# Every guard is exercised in both directions: a refusal-only suite would pass
# against a policy that denied everything, including root.
#
# The last section repeats the live checks specifically as the mica-mqttd uid
# and audits every repository policy fragment: mqttd may receive exact Item1
# grants from application packages, but no policy may grant it any access to
# com.mica.micad.
#
# Needs root (to drop to uid 65534 with setpriv) plus dbus-daemon and python3.
# It fails loudly when it cannot run rather than skipping: a skipped case that
# prints nothing reads exactly like a passing one.
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "${HERE}/../.." && pwd)
POLICY="${REPO_ROOT}/crates/mica-core/dist/com.mica.micad.conf"
NAME=com.mica.micad

[ -f "${POLICY}" ] || { echo "no ${POLICY} to test" >&2; exit 1; }
for tool in dbus-daemon setpriv python3; do
    command -v "${tool}" >/dev/null 2>&1 || {
        echo "required tool not available: ${tool}" >&2
        exit 1
    }
done
[ "$(id -u)" -eq 0 ] || {
    echo "must run as root: the whole point is dropping to an unprivileged uid" >&2
    exit 1
}

WORK=$(mktemp -d)
BUS_PID=""
SERVER_PIDS=()
cleanup() {
    for pid in ${SERVER_PIDS[@]+"${SERVER_PIDS[@]}"}; do
        kill "${pid}" 2>/dev/null || true
    done
    [ -n "${BUS_PID}" ] && kill "${BUS_PID}" 2>/dev/null || true
    rm -rf "${WORK}"
}
trap cleanup EXIT
chmod 0755 "${WORK}"

PASS=0
FAIL=0
check() {
    local name=$1 want=$2 got=$3
    if [ "$want" = "$got" ]; then
        PASS=$((PASS + 1))
        echo "PASS $name"
    else
        FAIL=$((FAIL + 1))
        echo "FAIL $name: expected [$want], got [$got]"
    fi
}

# --- the minimal D-Bus client ------------------------------------------------
# python3 stdlib only: there are no D-Bus bindings on this host and adding a
# dependency is out of scope. It speaks just enough of the wire protocol to own
# a name, make a method call, emit a signal and subscribe to one -- which is
# exactly the set of operations the policy governs.
#
# dbus-send cannot stand in for it: it makes method calls only, so it can test
# neither own= nor receive_sender=. dbus-monitor cannot either, because
# monitoring goes through BecomeMonitor/eavesdrop, a different policy path from
# the ordinary signal delivery the SettingsChanged broadcast actually uses.
CLIENT="${WORK}/dbusclient.py"
cat >"${CLIENT}" <<'PY'
"""Minimal D-Bus client, stdlib only. Modes: serve | call | own | recv."""
import binascii
import os
import select
import socket
import struct
import sys
import time

LITTLE = ord('l')
METHOD_CALL, METHOD_RETURN, ERROR, SIGNAL = 1, 2, 3, 4
# Header field code -> its variant type. Only the codes this client uses.
FIELD_TYPE = {1: 'o', 2: 's', 3: 's', 4: 's', 5: 'u', 6: 's', 7: 's', 8: 'g'}
PATH = '/com/mos/micad'
IFACE = 'com.mica.micad1'


def pad(buf, n):
    while len(buf) % n:
        buf.append(0)


def put_u32(buf, v):
    pad(buf, 4)
    buf.extend(struct.pack('<I', v))


def put_str(buf, v):
    b = v.encode()
    pad(buf, 4)
    buf.extend(struct.pack('<I', len(b)))
    buf.extend(b)
    buf.append(0)


def put_sig(buf, v):
    b = v.encode()
    buf.append(len(b))
    buf.extend(b)
    buf.append(0)


def put_val(buf, code, v):
    if code in 'so':
        put_str(buf, v)
    elif code == 'u':
        put_u32(buf, v)
    elif code == 'g':
        put_sig(buf, v)
    else:
        raise ValueError('unhandled type ' + code)


def get_u32(buf, off):
    off = (off + 3) & ~3
    return struct.unpack_from('<I', buf, off)[0], off + 4


def get_str(buf, off):
    n, off = get_u32(buf, off)
    return buf[off:off + n].decode('utf-8', 'replace'), off + n + 1


def get_sig(buf, off):
    n = buf[off]
    off += 1
    return buf[off:off + n].decode(), off + n + 1


def get_val(buf, off, code):
    if code in 'so':
        return get_str(buf, off)
    if code == 'u':
        return get_u32(buf, off)
    if code == 'g':
        return get_sig(buf, off)
    raise ValueError('unhandled type ' + code)


def parse(raw):
    _, mtype, _, _, body_len, serial = struct.unpack_from('<BBBBII', raw, 0)
    fields_len = struct.unpack_from('<I', raw, 12)[0]
    fields, off, end = {}, 16, 16 + fields_len
    while off < end:
        off = (off + 7) & ~7
        code = raw[off]
        off += 1
        sig, off = get_sig(raw, off)
        val, off = get_val(raw, off, sig)
        fields[code] = val
    body_off = (end + 7) & ~7
    return {
        'type': mtype,
        'serial': serial,
        'fields': fields,
        'body': raw[body_off:body_off + body_len],
        'sig': fields.get(8, ''),
    }


def body_args(sig, body):
    out, off = [], 0
    for code in sig:
        val, off = get_val(body, off, code)
        out.append(val)
    return out


class Conn:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.serial = 0
        self.buf = bytearray()
        self.sock.sendall(b'\0')
        uid = binascii.hexlify(str(os.getuid()).encode())
        self.sock.sendall(b'AUTH EXTERNAL ' + uid + b'\r\n')
        line = self._readline()
        if not line.startswith(b'OK'):
            raise RuntimeError('EXTERNAL auth refused: %r' % line)
        self.sock.sendall(b'BEGIN\r\n')
        reply = self.call('org.freedesktop.DBus', '/org/freedesktop/DBus',
                          'org.freedesktop.DBus', 'Hello', '', [])
        if reply is None or reply['type'] == ERROR:
            raise RuntimeError('Hello refused')
        self.unique = body_args('s', reply['body'])[0]

    def _readline(self):
        out = bytearray()
        while not out.endswith(b'\r\n'):
            b = self.sock.recv(1)
            if not b:
                raise RuntimeError('connection closed during auth')
            out.extend(b)
        return bytes(out)

    def send(self, mtype, fields, sig='', args=(), flags=0):
        self.serial += 1
        body = bytearray()
        for code, arg in zip(sig, args):
            put_val(body, code, arg)
        items = list(fields.items())
        if sig:
            items.append((8, sig))
        packed = bytearray()
        for code, val in items:
            pad(packed, 8)
            packed.append(code)
            put_sig(packed, FIELD_TYPE[code])
            put_val(packed, FIELD_TYPE[code], val)
        msg = bytearray(struct.pack('<BBBBII', LITTLE, mtype, flags, 1,
                                    len(body), self.serial))
        msg.extend(struct.pack('<I', len(packed)))
        msg.extend(packed)
        pad(msg, 8)
        msg.extend(body)
        self.sock.sendall(bytes(msg))
        return self.serial

    def _fill(self, want, deadline):
        while len(self.buf) < want:
            left = deadline - time.monotonic()
            if left <= 0:
                return False
            if not select.select([self.sock], [], [], left)[0]:
                continue
            chunk = self.sock.recv(65536)
            if not chunk:
                return False
            self.buf.extend(chunk)
        return True

    def recv(self, timeout):
        deadline = time.monotonic() + timeout
        if not self._fill(16, deadline):
            return None
        body_len = struct.unpack_from('<I', self.buf, 4)[0]
        fields_len = struct.unpack_from('<I', self.buf, 12)[0]
        total = ((16 + fields_len + 7) & ~7) + body_len
        if not self._fill(total, deadline):
            return None
        raw = bytes(self.buf[:total])
        del self.buf[:total]
        return parse(raw)

    def call(self, dest, path, iface, member, sig, args, timeout=5.0):
        serial = self.send(METHOD_CALL,
                           {1: path, 2: iface, 3: member, 6: dest}, sig, args)
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            msg = self.recv(left)
            if msg is None:
                return None
            if msg['fields'].get(5) == serial:
                return msg

    def request_name(self, name):
        return self.call('org.freedesktop.DBus', '/org/freedesktop/DBus',
                         'org.freedesktop.DBus', 'RequestName', 'su', [name, 0])


def outcome(reply, ok_text):
    if reply is None:
        return 'TIMEOUT'
    if reply['type'] == ERROR:
        return 'ERROR %s' % reply['fields'].get(4, '?')
    return ok_text


def mode_own(sock, name):
    reply = Conn(sock).request_name(name)
    print(outcome(reply, 'OWNED'))


def mode_call(sock, dest, iface=IFACE, member='GetSettings', path=PATH):
    # iface/member/path are parameters because send_interface= and
    # send_member= policy rules discriminate on them: a suite that could only
    # ever call one member cannot tell a per-member grant from a blanket one.
    conn = Conn(sock)
    reply = conn.call(dest, path, iface, member, 's', [''])
    print(outcome(reply, 'OK'))


def mode_recv(sock, sender, member, seconds):
    conn = Conn(sock)
    rule = "type='signal',sender='%s',member='%s'" % (sender, member)
    reply = conn.call('org.freedesktop.DBus', '/org/freedesktop/DBus',
                      'org.freedesktop.DBus', 'AddMatch', 's', [rule])
    if reply is None or reply['type'] == ERROR:
        print(outcome(reply, 'MATCHED'))
        return
    deadline = time.monotonic() + float(seconds)
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            break
        msg = conn.recv(left)
        if msg and msg['type'] == SIGNAL and msg['fields'].get(3) == member:
            print('GOT')
            return
    print('NONE')


def mode_serve(sock, name, member, iface=IFACE, path=PATH, also=''):
    # `also` is a comma-separated list of extra "iface/member" signals to emit
    # in the same loop. One owner per bus name means a receive_member= grant
    # and its negative control cannot be driven by two servers, so one server
    # broadcasts both.
    extra = [spec.split('/', 1) for spec in also.split(',') if spec]
    conn = Conn(sock)
    reply = conn.request_name(name)
    if reply is None or reply['type'] == ERROR:
        print(outcome(reply, 'OWNED'), flush=True)
        sys.exit(1)
    print('OWNED', flush=True)
    last = 0.0
    while True:
        msg = conn.recv(0.05)
        if msg and msg['type'] == METHOD_CALL:
            conn.send(METHOD_RETURN,
                      {5: msg['serial'], 6: msg['fields'][7]}, 's', ['{}'])
        now = time.monotonic()
        if now - last >= 0.2:
            last = now
            # Same shape as the real SettingsChanged(path, value_json).
            conn.send(SIGNAL, {1: path, 2: iface, 3: member}, 'ss',
                      ['access.webAdmin.password_hash', '"$2b$12$secret"'])
            for extra_iface, extra_member in extra:
                conn.send(SIGNAL, {1: path, 2: extra_iface, 3: extra_member},
                          'ss', ['access.webAdmin.password_hash',
                                 '"$2b$12$secret"'])


MODES = {'own': mode_own, 'call': mode_call, 'recv': mode_recv,
         'serve': mode_serve}
MODES[sys.argv[1]](*sys.argv[2:])
PY
chmod 0755 "${CLIENT}"

# --- the bus configuration ---------------------------------------------------
# The <policy context="default"> stanza below is a verbatim copy of the one in
# the standard dbus system.conf, so the only thing separating this bus from a
# stock system bus is the shipped file it <include>s. Two of those base rules
# matter here: `deny send_type="method_call"` (which is why dropping an allow is
# enough on the send side) and `allow receive_type="signal"` (which is why it is
# not enough on the receive side, and the shipped file carries an explicit deny).
SOCK="${WORK}/bus.sock"
BUS_CONF="${WORK}/bus.conf"
cat >"${BUS_CONF}" <<XML
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <listen>unix:path=${SOCK}</listen>
  <auth>EXTERNAL</auth>

  <policy context="default">
    <allow user="*"/>
    <deny own="*"/>
    <deny send_type="method_call"/>
    <allow send_type="signal"/>
    <allow send_requested_reply="true" send_type="method_return"/>
    <allow send_requested_reply="true" send_type="error"/>
    <allow receive_type="method_call"/>
    <allow receive_type="method_return"/>
    <allow receive_type="error"/>
    <allow receive_type="signal"/>
    <allow send_destination="org.freedesktop.DBus"
           send_interface="org.freedesktop.DBus"/>
  </policy>

  <!-- TEST SCAFFOLDING, not part of anything shipped. com.mica.control is a
       deliberately OPEN name and com.mica.unprivileged is a deliberately ownable
       one. They are the controls: without them "nobody was refused" cannot be
       told apart from "the unprivileged connection never worked", and every
       refusal below would be indistinguishable from a broken harness. -->
  <policy user="root">
    <allow own="com.mica.control"/>
  </policy>
  <policy context="default">
    <allow own="com.mica.unprivileged"/>
    <allow send_destination="com.mica.control"/>
    <allow receive_sender="com.mica.control"/>
  </policy>

  <include>${POLICY}</include>
</busconfig>
XML

dbus-daemon --config-file="${BUS_CONF}" --nofork &
BUS_PID=$!
for _ in $(seq 1 50); do
    [ -S "${SOCK}" ] && break
    sleep 0.1
done
[ -S "${SOCK}" ] || { echo "dbus-daemon did not create ${SOCK}" >&2; exit 1; }
chmod 0777 "${SOCK}"

# Own com.mica.micad (the name under test) and com.mica.control (the open control)
# from two SEPARATE root connections. One connection owning both would defeat
# the control outright: receive_sender= matches on the sending CONNECTION's
# names, so a deny on com.mica.micad would suppress the control name's signals too.
start_server() {
    local name=$1 log=$2
    setsid python3 "${CLIENT}" serve "${SOCK}" "${name}" SettingsChanged >"${log}" 2>&1 &
    SERVER_PIDS+=($!)
    for _ in $(seq 1 50); do
        [ -s "${log}" ] && break
        sleep 0.1
    done
    head -n1 "${log}"
}

owned_mosd=$(start_server "${NAME}" "${WORK}/server-micad.log")
owned_ctl=$(start_server com.mica.control "${WORK}/server-ctl.log")

as_root() { python3 "${CLIENT}" "$@" 2>&1 | tail -n1; }
as_nobody() {
    setpriv --reuid=65534 --regid=65534 --clear-groups \
        python3 "${CLIENT}" "$@" 2>&1 | tail -n1
}

# `awk 'NR == 1'`, not `| head -n1`: head closes the pipe after its line,
# dbus-daemon takes SIGPIPE, and the `set -euo pipefail` at :19 turns that 141
# into a failed run of the whole suite at its very first banner line. awk reads
# to EOF, so there is no early exit for dbus-daemon to be signalled by.
echo "bus: ${SOCK} (dbus-daemon $(dbus-daemon --version | awk 'NR == 1 {print $NF}'))"
echo "policy under test: ${POLICY}"
echo

# --- 0. the harness itself works ---------------------------------------------
# Without these the whole suite could be measuring a bus nobody can reach.
check "root owns ${NAME} (the daemon's own identity)" "OWNED" "${owned_mosd}"
check "root owns the control name" "OWNED" "${owned_ctl}"
check "nobody can connect and own an unrestricted name" "OWNED" \
    "$(as_nobody own "${SOCK}" com.mica.unprivileged)"
check "nobody can call an unrestricted name" "OK" \
    "$(as_nobody call "${SOCK}" com.mica.control)"
check "nobody can receive a signal from an unrestricted name" "GOT" \
    "$(as_nobody recv "${SOCK}" com.mica.control SettingsChanged 3)"
echo

# --- 1. send: non-root refused, root permitted -------------------------------
check "non-root SEND to ${NAME} is refused" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_nobody call "${SOCK}" "${NAME}")"
check "root SEND to ${NAME} is permitted" "OK" "$(as_root call "${SOCK}" "${NAME}")"
echo

# --- 2. receive: non-root refused, root permitted ----------------------------
# SettingsChanged carries the settings VALUE, so a uid that can subscribe reads
# every change as it happens whether or not it may call anything.
check "non-root RECEIVE of SettingsChanged from ${NAME} is refused" "NONE" \
    "$(as_nobody recv "${SOCK}" "${NAME}" SettingsChanged 3)"
check "root RECEIVE of SettingsChanged from ${NAME} is permitted" "GOT" \
    "$(as_root recv "${SOCK}" "${NAME}" SettingsChanged 3)"
echo

# --- 3. own: non-root refused ------------------------------------------------
# The permitted direction is check 0's "root owns com.mica.micad": the name is
# owned by a root connection for the entire run, which is the only reason any
# of the send and receive cases above have anything to talk to.
check "non-root OWN of ${NAME} is refused" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_nobody own "${SOCK}" "${NAME}")"



# --- 4. the MQTT bridge has zero micad access -------------------------------
# The bridge is a non-root bus client, but it is not a micad client. Application
# packages may grant this uid exact Item1 access to their own direct service
# names; nothing punches through com.mica.micad.conf.
MQTTD_UID=65534
# The ungranted control uid. Distinct from MQTTD_UID because "an unprivileged
# uid is refused" and "the granted uid is allowed" have to be two different
# uids or one of them is unobservable.
#
# It must be a uid that EXISTS. 65533 was the obvious pick and was wrong:
# dbus-daemon answers EXTERNAL auth from an unresolvable uid with
# "REJECTED EXTERNAL", so the client never reaches the policy at all — and the
# case still "failed to call GetItems", which is indistinguishable from a
# policy refusal in every way except the error string. The connectivity check
# below is what keeps that confusion from recurring silently.
OTHER_UID=33

MQTTD_SOCK="${WORK}/mqttd-bus.sock"
MQTTD_CONF="${WORK}/mqttd-bus.conf"

echo
echo "mqttd bus: ${MQTTD_SOCK}"
echo "policy under test: ${POLICY}"
echo

cat >"${MQTTD_CONF}" <<XML
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>system</type>
  <listen>unix:path=${MQTTD_SOCK}</listen>
  <auth>EXTERNAL</auth>

  <policy context="default">
    <allow user="*"/>
    <deny own="*"/>
    <deny send_type="method_call"/>
    <allow send_type="signal"/>
    <allow send_requested_reply="true" send_type="method_return"/>
    <allow send_requested_reply="true" send_type="error"/>
    <allow receive_type="method_call"/>
    <allow receive_type="method_return"/>
    <allow receive_type="error"/>
    <allow receive_type="signal"/>
    <allow send_destination="org.freedesktop.DBus"
           send_interface="org.freedesktop.DBus"/>
  </policy>

  <include>${POLICY}</include>
</busconfig>
XML

dbus-daemon --config-file="${MQTTD_CONF}" --nofork --print-address \
    >"${WORK}/mqttd-bus.addr" 2>"${WORK}/mqttd-bus.err" &
MQTTD_BUS_PID=$!
SERVER_PIDS+=("${MQTTD_BUS_PID}")
for _ in $(seq 1 50); do
    [ -S "${MQTTD_SOCK}" ] && break
    sleep 0.1
done
[ -S "${MQTTD_SOCK}" ] || {
    echo "mqttd bus did not start: $(cat "${WORK}/mqttd-bus.err")" >&2
    exit 1
}
chmod 0777 "${MQTTD_SOCK}"

# One root server owns com.mica.micad and deliberately broadcasts a legacy Item1
# signal plus a management signal. The bridge must receive neither even if a
# future regression or hostile system service emits them.
setsid python3 "${CLIENT}" serve "${MQTTD_SOCK}" "${NAME}" ItemsChanged \
    com.mica.Item1 /com/mos/micad com.mica.micad1/SettingsChanged \
    >"${WORK}/mqttd-server.log" 2>&1 &
SERVER_PIDS+=($!)
for _ in $(seq 1 50); do
    [ -s "${WORK}/mqttd-server.log" ] && break
    sleep 0.1
done

as_mqttd() {
    setpriv --reuid="${MQTTD_UID}" --regid="${MQTTD_UID}" --clear-groups \
        python3 "${CLIENT}" "$@" 2>&1 | tail -n1
}
as_other() {
    setpriv --reuid="${OTHER_UID}" --regid="${OTHER_UID}" --clear-groups \
        python3 "${CLIENT}" "$@" 2>&1 | tail -n1
}

# The harness first. Every claim below is about a bus with a live owner on it.
check "mqttd: root owns ${NAME} on the policy bus" "OWNED" \
    "$(head -n1 "${WORK}/mqttd-server.log")"
check "mqttd: root can still reach ${NAME}" "OK" \
    "$(as_root call "${MQTTD_SOCK}" "${NAME}")"

echo
# Even the former read-only exception is gone: device identity is runtime
# configuration, not a micad call.
check "mqttd: the bridge's uid CANNOT call the removed GetDeviceId member" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 GetDeviceId /com/mos/micad)"

echo
# The network-facing bridge has no system-management read, write, action, or
# signal surface.
check "mqttd: the bridge's uid CANNOT call system com.mica.Item1.GetItems" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.Item1 GetItems /)"
check "mqttd: the bridge's uid CANNOT call system com.mica.Item1.SetValue" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.Item1 SetValue /hostname)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.Reboot" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 Reboot)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.PowerOff" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 PowerOff)"
check "mqttd: the bridge's uid CANNOT call SetTransientRootPassword" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 SetTransientRootPassword)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.SetSettings" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 SetSettings)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.GetState" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 GetState)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.ReportHealth" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 ReportHealth)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.ForgetService" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 ForgetService)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.RotateWireguardKey" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 RotateWireguardKey)"
# The update members ride the same root-only interface and inherit the same
# refusal; asserted by name anyway, because these are the members whose
# accidental grant would be worst — a uid that can install a bundle or mark a
# slot bad owns the device's next boot, network socket and all.
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.InstallUpdate" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 InstallUpdate)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.MarkUpdate" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 MarkUpdate)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.GetUpdateState" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 GetUpdateState)"
check "mqttd: the bridge's uid CANNOT call com.mica.micad1.GetSettings" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_mqttd call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 GetSettings /com/mos/micad)"
# SettingsChanged carries the settings VALUE, including the web admin password
# hash. The bridge cannot receive this broadcast.
check "mqttd: the bridge's uid CANNOT receive com.mica.micad1.SettingsChanged" "NONE" \
    "$(as_mqttd recv "${MQTTD_SOCK}" "${NAME}" SettingsChanged 3)"
check "mqttd: the bridge's uid CANNOT receive system com.mica.Item1.ItemsChanged" "NONE" \
    "$(as_mqttd recv "${MQTTD_SOCK}" "${NAME}" ItemsChanged 3)"

echo
# CONNECTIVITY FIRST, for both uids. A client that cannot authenticate fails
# every call below, and fails them in a way that reads as a policy refusal
# unless something checks — which is not hypothetical: the control uid was
# 65533 until this suite reported it "refused" for having no passwd entry.
#
# The three outcomes are distinguishable, so the check classifies rather than
# matching one string. Calling GetId on the bus daemon with a body it does not
# take answers InvalidArgs, and an InvalidArgs is a STRONGER connectivity
# proof than a success would be: the daemon accepted the connection, accepted
# the message, and parsed its body far enough to object to it.
connectivity() {
    case "$1" in
    RuntimeError*) echo "AUTH-REFUSED" ;;
    "ERROR org.freedesktop.DBus.Error.AccessDenied") echo "POLICY-REFUSED" ;;
    TIMEOUT) echo "NO-ANSWER" ;;
    *) echo "CONNECTED" ;;
    esac
}
check "mqttd: the bridge's uid is connected (so its refusals are policy, not auth)" \
    "CONNECTED" \
    "$(connectivity "$(as_mqttd call "${MQTTD_SOCK}" org.freedesktop.DBus org.freedesktop.DBus GetId /org/freedesktop/DBus)")"
check "mqttd: the control uid is connected (so its refusals are policy, not auth)" \
    "CONNECTED" \
    "$(connectivity "$(as_other call "${MQTTD_SOCK}" org.freedesktop.DBus org.freedesktop.DBus GetId /org/freedesktop/DBus)")"
check "mqttd: another unprivileged uid CANNOT call micad either" \
    "ERROR org.freedesktop.DBus.Error.AccessDenied" \
    "$(as_other call "${MQTTD_SOCK}" "${NAME}" com.mica.micad1 GetSettings /com/mos/micad)"
check "mqttd: another unprivileged uid CANNOT receive SettingsChanged" "NONE" \
    "$(as_other recv "${MQTTD_SOCK}" "${NAME}" SettingsChanged 3)"

# Repository-wide static audit. Application packages may name mica-mqttd, but
# only for exact direct service destinations. No prefix ownership grant and no
# micad destination/sender may hide in another policy fragment.
check "mqttd: legacy global extension policy is absent" "ABSENT" \
    "$([ ! -e "${REPO_ROOT}/crates/mica-core/dist/com.mica.ext.conf" ] && echo ABSENT || echo PRESENT)"
check "mqttd: legacy micad exception policy is absent" "ABSENT" \
    "$([ ! -e "${REPO_ROOT}/crates/mica-core/dist/mica-mqttd.conf" ] && echo ABSENT || echo PRESENT)"

policy_audit=$(python3 - "${REPO_ROOT}" <<'PY'
import pathlib
import sys
import xml.etree.ElementTree as ET

root = pathlib.Path(sys.argv[1])
violations = []
for path in root.joinpath("os").rglob("*.conf"):
    try:
        tree = ET.parse(path)
    except (ET.ParseError, OSError):
        continue
    if tree.getroot().tag != "busconfig":
        continue
    for allow in tree.findall(".//allow"):
        prefix = allow.get("own_prefix")
        if prefix == "com.mica" or (prefix and prefix.startswith("com.mica.")):
            violations.append(f"{path}: prefix ownership grant {prefix}")
    for policy in tree.findall(".//policy"):
        policy_user = policy.get("user")
        for allow in policy.findall("allow"):
            owned = allow.get("own")
            if (owned and owned.startswith("com.mica.") and
                    (not policy_user or policy_user == "*")):
                violations.append(
                    f"{path}: exact com.mica ownership grant is not scoped to one explicit user"
                )
        if policy.get("user") != "mica-mqttd":
            continue
        for allow in policy.findall("allow"):
            destination = allow.get("send_destination")
            sender = allow.get("receive_sender")
            if destination == "com.mica.micad" or sender == "com.mica.micad":
                violations.append(f"{path}: mica-mqttd reaches com.mica.micad")
            send_item = allow.get("send_interface") == "com.mica.Item1"
            receive_item = allow.get("receive_interface") == "com.mica.Item1"
            endpoint = destination if send_item else sender if receive_item else None
            if send_item or receive_item:
                if (not endpoint or "*" in endpoint or
                        not endpoint.startswith("com.mica.") or
                        endpoint == "com.mica.micad"):
                    violations.append(f"{path}: Item1 grant lacks a safe exact application endpoint")
                member = allow.get("send_member") if send_item else allow.get("receive_member")
                if not member:
                    violations.append(f"{path}: Item1 grant lacks an exact member")
            elif ((destination and destination.startswith("com.mica.")) or
                    (sender and sender.startswith("com.mica."))):
                violations.append(f"{path}: mica-mqttd has non-Item1 com.mica access")

print("OK" if not violations else " | ".join(violations))
PY
)
check "mqttd: all policy fragments keep exact application grants and zero micad access" \
    "OK" "${policy_audit}"

echo
echo "$PASS passed, $FAIL failed"
echo "RESULT: $([ "$FAIL" -eq 0 ] && echo PASS || echo FAIL) ($PASS/$((PASS + FAIL)) checks)"
[ "$FAIL" -eq 0 ]
