/**
 * Phase 05c -- the kernel actually creating the three link kinds,
 * and the key store actually being readable by the account that reads it.
 *
 * WHY THIS HARNESS AND NOT `verify`. verify has four modes and not one of
 * them boots anything: `--lint` reads board definitions, `--verify` reads an
 * assembled image, `--smoke` executes self-built binaries inside the packed
 * root, `--smoke-negative` rejects three defective images. Its two new
 * checks read `/boot/config-*` and the module indexes out of an unpacked
 * squashfs, and that is the limit of what an offline reader can say: a config
 * symbol is a claim about what was COMPILED, and `modules.dep` is a claim about
 * what was PACKED. Neither answers whether the running kernel hands back a
 * device. Only `ip link add` on a live guest does, and this harness is the only
 * thing in the tree that has one -- it already prepares a disk, boots it, puts
 * journald on the serial line and captures the console. Teaching verify to
 * boot QEMU would be a second copy of all of that.
 *
 * THE GUEST SIDE IS NOT HERE. `pkgs/mosd/tests/apid-api/guest/m7-net-smoke.sh` runs inside
 * the guest, seeded onto STATE by `run.sh` and started from the kernel command
 * line; this phase only reads its conclusions back off the console. The split
 * is forced: there is no login, no ssh and no route into this guest, and the
 * console is a one-way channel. So the guest decides and this side reads.
 *
 * THE MARKER IS OFFSET ZERO, which is the one place in this suite where a
 * whole-boot window is the honest one. Every other console assertion marks
 * before the POST whose effect it observes, because the boot writes thousands
 * of lines and a whole-file grep would let one of them satisfy an assertion
 * about an action taken seconds ago. Here the BOOT IS THE ACTION: the smoke
 * unit is started from the kernel command line and has run to completion long
 * before this phase begins, so a mark taken now would sit AFTER the evidence.
 */

import { openConsole, type ConsoleLog, type Marker } from "../console.ts";
import type { Phase, PhaseContext } from "../runner.ts";

/** What the guest script prefixes every conclusion with. */
const PREFIX = "M7-SMOKE:";

/**
 * How long to wait for the smoke to finish.
 *
 * It waits up to 120s for STATE to be mounted before it does anything, and a
 * TCG guest is slow, so this is that plus room. It is a wait for a unit that
 * started at boot, not for an action this phase took: by the time apid is
 * answering, the usual case is that every line is already there.
 */
const SMOKE_TIMEOUT_MS = 180_000;

/**
 * The conclusions this phase requires, and what each one is evidence OF.
 *
 * Written down HERE rather than derived from whatever the guest happened to
 * print, so that a smoke script which silently stopped emitting a check fails
 * this phase instead of shrinking it. A run that asserted nothing must not read
 * as success -- the same rule the harness applies to its own totals.
 */
const REQUIRED = [
  ["state-mounted", "the STATE partition is mounted, so the key-store paths below are real"],
  ["modprobe-8021q", "the running kernel resolves the 8021q module"],
  ["modprobe-bridge", "the running kernel resolves the bridge module"],
  ["modprobe-wireguard", "the running kernel resolves the wireguard module"],
  ["link-vlan", "the kernel creates a VLAN device on a declared parent"],
  ["link-bridge", "the kernel creates a bridge device"],
  ["link-wireguard", "the kernel creates a WireGuard device"],
  ["netuser-exists", "the image carries the systemd-network account mosd chowns key files to"],
  [
    "keystore-readable",
    "the systemd-network USER can read a 0640 root:systemd-network key under " +
      "<state>/networkd-secrets/ -- run as that user, not inferred from mode bits",
  ],
  [
    "secrets-unreadable",
    "the same user CANNOT read <state>/secrets/, which is why the key store is a sibling " +
      "of it and not a directory under it",
  ],
] as const;

/**
 * Conclusions that are real when present and honestly absent otherwise.
 *
 * `keystore-real-readable` is about a key MOSD generated, which exists only
 * once a WireGuard interface has been configured and reconciled. On the first
 * boot of a fresh disk there is none, and the guest says SKIP rather than
 * inventing one. Requiring it would make this phase depend on 05b's timing and
 * report "not created yet" as EACCES.
 */
const OPTIONAL = [
  [
    "keystore-real-readable",
    "the systemd-network user can read the key mosd itself generated",
  ],
] as const;

/** One `M7-SMOKE: <id> <PASS|FAIL|SKIP> <detail>` line, parsed. */
export interface SmokeLine {
  readonly id: string;
  readonly status: "PASS" | "FAIL" | "SKIP";
  readonly detail: string;
}

/**
 * Parse the smoke's conclusions out of a console window.
 *
 * The LAST line for an id wins. The guest script runs once per boot, but this
 * harness boots the same disk twice and phase 07b reads a console the first
 * boot also wrote to; taking the last leaves this reading the most recent boot
 * rather than an older one under it.
 */
export function parseSmoke(lines: readonly string[]): Map<string, SmokeLine> {
  const found = new Map<string, SmokeLine>();
  for (const line of lines) {
    const at = line.indexOf(PREFIX);
    if (at === -1) continue;
    const rest = line.slice(at + PREFIX.length).trim();
    const match = /^(\S+)\s+(PASS|FAIL|SKIP)(?:\s+(.*))?$/.exec(rest);
    if (match === null) continue;
    const [, id, status, detail] = match;
    if (id === undefined || status === undefined) continue;
    found.set(id, { id, status: status as SmokeLine["status"], detail: detail ?? "" });
  }
  return found;
}

/** The whole-boot window; see the header for why offset zero is right here. */
function bootMarker(): Marker {
  return { label: "the guest's boot", offset: 0, takenAt: 0 };
}

async function waitForSmoke(log: ConsoleLog, marker: Marker): Promise<boolean> {
  const end = await log.waitFor(marker, new RegExp(`${PREFIX} END`), SMOKE_TIMEOUT_MS);
  return end !== null;
}

const phase: Phase = {
  id: "05c-kernel-net",
  title:
    "On the booted image: VLAN, bridge and WireGuard devices, and the key store " +
    "read as the systemd-network user",
  assumes:
    "the guest booted from a disk that run.sh seeded with pkgs/mosd/tests/apid-api/guest/m7-net-smoke.sh " +
    "and a kernel command line that starts it; the smoke runs at boot and is independent of " +
    "every earlier phase's writes",

  async run(ctx: PhaseContext): Promise<void> {
    const { report, config } = ctx;
    const log = openConsole(config.consoleLog);
    const marker = bootMarker();

    if (!log.available) {
      report.skip(
        "the M7 kernel-networking smoke",
        log.unavailableReason ??
          "the console log is unavailable, and this phase has no other channel into the guest",
      );
      return;
    }

    const finished = await waitForSmoke(log, marker);
    const found = parseSmoke(log.linesSince(marker));

    if (found.size === 0) {
      // The mechanism, not the kernel. Distinguished because the repair is a
      // different one: a guest that never ran the script says nothing about
      // whether it could have created the devices, and reporting ten reds here
      // would name ten defects that have not been observed.
      report.fail(
        "the M7 kernel-networking smoke ran on the guest",
        [
          `expected: console lines beginning "${PREFIX}" somewhere in this boot`,
          "actual:   none. The guest never ran pkgs/mosd/tests/apid-api/guest/m7-net-smoke.sh, so NOTHING",
          "          below has been observed -- neither passing nor failing. Check that run.sh",
          "          seeded the script onto STATE and that the kernel command line starts it.",
          ...log.describeWindow(marker),
        ].join("\n"),
      );
      return;
    }

    report.pass(`the M7 smoke ran on the guest (${found.size} conclusion(s) on the console)`);
    if (!finished) {
      // Partial output is not a pass and not a silent truncation: say so, then
      // judge what did arrive. A missing END with conclusions present means the
      // script died part way, and the checks it never reached are absent below.
      report.fail(
        "the M7 smoke ran to completion",
        `expected: a "${PREFIX} END" line within ${SMOKE_TIMEOUT_MS}ms. The script emitted ` +
          `${found.size} conclusion(s) and then stopped, so any check missing below was never ` +
          `performed rather than performed and lost.`,
      );
    }

    for (const [id, what] of REQUIRED) {
      const line = found.get(id);
      if (line === undefined) {
        report.fail(what, `expected: a "${PREFIX} ${id} PASS" line on the console; none was written`);
        continue;
      }
      if (line.status === "PASS") {
        report.pass(`${what} -- ${line.detail}`);
      } else if (line.status === "SKIP") {
        report.skip(what, line.detail);
      } else {
        report.fail(what, `the guest reported: ${line.detail}`);
      }
    }

    for (const [id, what] of OPTIONAL) {
      const line = found.get(id);
      if (line === undefined || line.status === "SKIP") {
        report.skip(what, line?.detail ?? `the guest wrote no "${id}" conclusion this boot`);
      } else if (line.status === "PASS") {
        report.pass(`${what} -- ${line.detail}`);
      } else {
        report.fail(what, `the guest reported: ${line.detail}`);
      }
    }
  },
};

export default phase;
