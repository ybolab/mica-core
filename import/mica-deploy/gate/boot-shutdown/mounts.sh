#!/usr/bin/env bash
# Run as PID1 only in a disposable private mount namespace with CAP_SYS_ADMIN.
# Every storage mount created here is tmpfs; no loop, DM, watchdog or disk is opened.
set -euo pipefail
[ "$$" = 1 ] || { echo 'Mount fixture requires disposable container PID1' >&2; exit 1; }
for tool in cp mkdir cat; do command -v "$tool" >/dev/null; done
NATIVE=${1:?explicit same-source native shutdown required}
if [ "${2:-}" = --empty ]; then
    # The emulator is fixture machinery only; the target has no loader, tools,
    # libraries, /proc, /dev or /sys. Never invoke sync or a terminal action.
    mkdir /empty
    cp "$NATIVE" /empty/shutdown
    runner=(/shutdown)
    if [ -n "${3:-}" ]; then
        cp "$3" /empty/emulator
        runner=(/emulator /shutdown)
    fi
    status=0
    chroot /empty "${runner[@]}" reboot > /tmp/empty-public.log 2>&1 || status=$?
    [ "$status" = 1 ]
    grep -Fx 'mica-shutdown requires PID 1' /tmp/empty-public.log >/dev/null
    printf '%s\n' EMPTY_EXEC_LOADER_PASS
    status=0
    chroot /empty "${runner[@]}" --lifecycle-worker '"Scan"' > /tmp/empty-scan.log 2>&1 || status=$?
    [ "$status" = 1 ]
    grep -F 'required proc filesystem unavailable' /tmp/empty-scan.log >/dev/null
    status=0
    chroot /empty "${runner[@]}" --lifecycle-worker '"Private"' > /tmp/empty-private.log 2>&1 || status=$?
    [ "$status" = 1 ]
    cat /tmp/empty-private.log
    # CAP_SYS_ADMIN is absent: a direct mount syscall must report EPERM. The
    # historical external BusyBox path instead reports a missing executable.
    grep -F 'Operation not permitted' /tmp/empty-private.log >/dev/null
    printf '%s\n' STATIC_EMPTY_USERSPACE_PASS
    exit 0
fi
command -v python3 >/dev/null
cp /busybox-out/x64/busybox /bin/busybox
BB=/bin/busybox
mkdir -p /newroot/dev /newroot/proc /newroot/sys /fixture/storage
"$BB" mount -o rprivate / /
for api in dev proc sys; do "$BB" mount -o move "/$api" "/newroot/$api"; done
# The shared group has no peers outside this private container namespace.
"$BB" mount -o rshared / /
"$NATIVE" --lifecycle-worker '"Private"'
for api in dev proc sys; do [ -d "/$api" ]; done
"$BB" mount -t tmpfs -o size=2M,nodev,nosuid tmpfs /fixture
mkdir /fixture/storage /fixture/child
"$BB" mount -t tmpfs -o size=1M,nodev,nosuid tmpfs /fixture/storage
"$BB" mount -o bind /fixture/storage /fixture/child
operation() {
    python3 - "$1" "$2" > /tmp/b3-operation.json <<'PY'
import json, sys
verb, path = sys.argv[1:]
for line in open('/proc/self/mountinfo'):
    left, right = line.rstrip('\n').split(' - ')
    words, extra = left.split(), right.split()
    if words[4] != path:
        continue
    major, minor = map(int, words[2].split(':'))
    print(json.dumps({verb: dict(id=int(words[0]), parent=int(words[1]), device=dict(major=major, minor=minor),
                                root=words[3], path=words[4], kind=extra[0], propagation=words[6:])}))
    break
else:
    raise SystemExit('fixture mount missing')
PY
}
operation Unmount /fixture/child
# An inherited directory descriptor pins the ordinary bind mount.
exec 9< /fixture/child
if "$NATIVE" --lifecycle-worker "$(cat /tmp/b3-operation.json)" > /tmp/b3-busy.log 2>&1; then
    echo 'Busy mount unexpectedly released' >&2; exit 1
fi
exec 9<&-
"$NATIVE" --lifecycle-worker "$(cat /tmp/b3-operation.json)"
operation MoveBacking /fixture/storage
cp /tmp/b3-operation.json /tmp/b3-stale.json
"$NATIVE" --lifecycle-worker "$(cat /tmp/b3-operation.json)"
# The pre-move pathname/parent identity must no longer authorize an operation.
if "$NATIVE" --lifecycle-worker "$(cat /tmp/b3-stale.json)" > /tmp/b3-stale.log 2>&1; then
    echo 'Stale moved mount identity accepted' >&2; exit 1
fi
path=$(python3 - <<'PY'
import json
print('/backing/' + str(json.load(open('/tmp/b3-stale.json'))['MoveBacking']['id']))
PY
)
operation SyncMount "$path"
"$NATIVE" --lifecycle-worker "$(cat /tmp/b3-operation.json)"
operation Unmount "$path"
"$NATIVE" --lifecycle-worker "$(cat /tmp/b3-operation.json)"
operation Unmount /fixture
"$NATIVE" --lifecycle-worker "$(cat /tmp/b3-operation.json)"
printf '%s\n' NATIVE_TMPFS_MOUNT_FIXTURES_PASS
