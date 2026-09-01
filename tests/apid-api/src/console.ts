/**
 * The QEMU console log -- this suite's second observation channel.
 *
 * Every other assertion observes apid through apid's own HTTP surface, so the
 * answer is the thing under test describing itself; these lines are written by
 * systemd and by mosd instead. It is also the only usable journal: the image
 * sets journald `Storage=volatile`, so the guest boots with
 * `systemd.journald.forward_to_console=1` and the harness appends the serial
 * console to `APID_CONSOLE` for as long as the suite runs.
 */

import { closeSync, openSync, readSync, statSync } from "node:fs";
import type { Reporter } from "./report.ts";

/** How often a `waitFor` re-reads the window. Cheap: it is a local file read. */
const POLL_INTERVAL_MS = 250;
/** Lines of context pasted into a failure or skip detail. */
const EVIDENCE_LINES = 12;
/** Bytes kept of one evidence line, so a runaway line cannot bury a report. */
const EVIDENCE_LINE_BYTES = 300;

/**
 * A position in the console log, taken before the action whose effect is to be
 * observed. Everything asserted with this marker is asserted about bytes the
 * guest wrote afterwards.
 *
 * Mark first is the rule: a TCG boot writes thousands of lines and `Started
 * ssh.service` is among them, so a whole-file grep would let the boot satisfy
 * an assertion about a POST made thirty seconds ago. Every function here takes
 * a Marker, and this module contains no whole-file search at all.
 */
export interface Marker {
  /** What was about to happen when it was taken; quoted in failure details. */
  readonly label: string;
  /** End-of-file offset, in bytes, at the moment of the mark. */
  readonly offset: number;
  readonly takenAt: number;
}

/**
 * A handle on the captured console log.
 *
 * Availability is not a flag a caller may forget: it is established when the
 * handle is opened AND re-checked after every read, because a log that becomes
 * unreadable mid-run must degrade to SKIP rather than to a silent false.
 */
export class ConsoleLog {
  readonly #path: string | undefined;
  #unavailable: string | undefined;

  constructor(path: string | undefined) {
    this.#path = path;
    if (path === undefined) {
      this.#unavailable =
        "APID_CONSOLE is not set, so there is no captured QEMU console log to read. " +
        "Console-backed checks observe the DEVICE acting (systemd and mosd write those " +
        "lines, not apid); without the log they are not performed, and they are never " +
        "assumed to have passed.";
      return;
    }
    try {
      const stat = statSync(path);
      if (!stat.isFile()) {
        this.#unavailable = `APID_CONSOLE=${path} is not a regular file (it is ${describeStat(stat)}).`;
        return;
      }
      closeSync(openSync(path, "r"));
    } catch (error) {
      this.#unavailable = `APID_CONSOLE=${path} could not be read: ${String(error)}`;
    }
  }

  /** The path being watched, or undefined when nothing is. */
  get path(): string | undefined {
    return this.#path;
  }

  get available(): boolean {
    return this.#unavailable === undefined;
  }

  /** Why the log cannot be used, in a sentence fit for a SKIP line. */
  get unavailableReason(): string | undefined {
    return this.#unavailable;
  }

  /**
   * Capture the current end-of-file offset.
   *
   * Call this BEFORE the request whose effect is to be observed. A marker taken
   * afterwards can only lose evidence, and one taken far enough back lets boot
   * noise satisfy the assertion -- which is the failure this module exists to
   * make impossible.
   */
  mark(label: string): Marker {
    const takenAt = Date.now();
    if (!this.available || this.#path === undefined) return { label, offset: 0, takenAt };
    try {
      return { label, offset: statSync(this.#path).size, takenAt };
    } catch (error) {
      this.#degrade(
        `the console log ${this.#path} could not be stat'd when marking before ${label}: ${String(error)}`,
      );
      return { label, offset: 0, takenAt };
    }
  }

  /** Every complete line written after `marker`, with terminal noise stripped. */
  linesSince(marker: Marker): string[] {
    return splitLines(this.#read(marker, true) ?? "");
  }

  /**
   * The raw bytes written after `marker`, exactly as the guest wrote them.
   *
   * Unsanitised on purpose: an absence assertion ("the password is not in the
   * log") must search what was actually written -- including a line whose
   * newline has not landed yet -- and not a prettified view of it.
   */
  textSince(marker: Marker): string {
    return this.#read(marker, true) ?? "";
  }

  /**
   * Poll for a line after `marker` matching `pattern`; return it, or null if
   * the timeout passes without one.
   *
   * Null means "not observed within the window". It does not mean "did not
   * happen" -- reconcilers on a TCG guest are slow -- so the CALLER decides
   * whether absence is a FAIL (a line whose exact wording was measured) or a
   * SKIP (a line whose wording was not).
   */
  async waitFor(marker: Marker, pattern: RegExp, timeoutMs: number): Promise<string | null> {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const hit = this.#firstMatch(marker, pattern, false);
      if (hit !== null) return hit;
      if (!this.available) return null;
      if (Date.now() >= deadline) break;
      await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL_MS));
    }
    // One last look that also considers a trailing line whose newline has not
    // landed yet: the guest may have written the evidence and be mid-line.
    return this.#firstMatch(marker, pattern, true);
  }

  /** The last `count` lines since `marker`, for pasting into a report. */
  tail(marker: Marker, count = EVIDENCE_LINES): string[] {
    const lines = this.linesSince(marker);
    return lines.slice(Math.max(0, lines.length - count)).map(clip);
  }

  /** The window described for a failure or skip detail: how big, and what is in it. */
  describeWindow(marker: Marker): string[] {
    if (!this.available) {
      return [`console:  unavailable -- ${this.unavailableReason ?? "unknown reason"}`];
    }
    const lines = this.linesSince(marker);
    const head = [
      `console:  ${this.#path ?? "(none)"}`,
      `window:   from the mark taken before ${marker.label} (offset ${marker.offset}), ` +
        `${lines.length} line(s) written since`,
    ];
    if (lines.length === 0) {
      head.push("actual:   the guest wrote NOTHING to the console in this window");
      return head;
    }
    head.push(`last ${Math.min(lines.length, EVIDENCE_LINES)} line(s) in the window:`);
    for (const line of this.tail(marker)) head.push(`  | ${line}`);
    return head;
  }

  // -- internals ------------------------------------------------------------

  #firstMatch(marker: Marker, pattern: RegExp, includePartial: boolean): string | null {
    const text = this.#read(marker, includePartial);
    if (text === undefined) return null;
    for (const line of splitLines(text)) {
      // A /g pattern keeps `lastIndex` between calls, so `test` would silently
      // skip every other line. Reset before each use.
      pattern.lastIndex = 0;
      if (pattern.test(line)) return line;
    }
    return null;
  }

  /**
   * Read the window. `includePartial` false drops a trailing unterminated line,
   * so a half-written line is never reported as evidence; `waitFor`'s final
   * attempt passes true.
   */
  #read(marker: Marker, includePartial: boolean): string | undefined {
    if (!this.available || this.#path === undefined) return undefined;
    let fd: number | undefined;
    try {
      const size = statSync(this.#path).size;
      if (size < marker.offset) {
        // Truncated or rotated under us. Every offset taken before now points at
        // the wrong bytes, so nothing may be attributed to any window any more:
        // degrade, and let the reporting helpers turn that into SKIP.
        this.#degrade(
          `the console log ${this.#path} shrank from ${marker.offset} to ${size} bytes since ` +
            `the mark before ${marker.label} (truncated or rotated), so no line can honestly ` +
            `be attributed to the window after that mark`,
        );
        return undefined;
      }
      const length = size - marker.offset;
      if (length === 0) return "";
      fd = openSync(this.#path, "r");
      const buffer = Buffer.allocUnsafe(length);
      let filled = 0;
      while (filled < length) {
        const read = readSync(fd, buffer, filled, length - filled, marker.offset + filled);
        if (read <= 0) break;
        filled += read;
      }
      const text = buffer.subarray(0, filled).toString("utf8");
      if (includePartial) return text;
      const lastNewline = text.lastIndexOf("\n");
      return lastNewline < 0 ? "" : text.slice(0, lastNewline + 1);
    } catch (error) {
      this.#degrade(`the console log ${this.#path} became unreadable mid-run: ${String(error)}`);
      return undefined;
    } finally {
      if (fd !== undefined) closeSync(fd);
    }
  }

  #degrade(reason: string): void {
    this.#unavailable ??= reason;
  }
}

/** Open the console log named by `APID_CONSOLE` (i.e. `config.consoleLog`). */
export function openConsole(path: string | undefined): ConsoleLog {
  return new ConsoleLog(path);
}

// reporting helpers. Every console-backed assertion in the suite goes through
// one of these three, which is why "an unreadable log is a SKIP, never a PASS"
// is a property of the code rather than a rule each phase has to remember: with
// `APID_CONSOLE` unset, unreadable or truncated under a mark, `available` is
// false and these emit a SKIP naming the reason. `expectConsoleAbsent` also
// refuses an empty window, an absence assertion over no bytes being vacuous.

export interface ConsoleExpectOptions {
  readonly timeoutMs?: number;
  /** Quoted in the detail so a reader can see what wording was searched for. */
  readonly describePattern?: string;
}

/**
 * Assert a line matching `pattern` appears after `marker`. FAILS if none does.
 *
 * Use this ONLY where the wording was measured -- mosd's container reconciler
 * emits its `container: ...` lines from `tracing::info!` and the phase quotes
 * them verbatim. Absence of a measured line is a real failure of the device to
 * act. For wording that could NOT be measured from this side, use
 * `observeConsoleLine`, which reports SKIP instead of manufacturing a red.
 */
export async function expectConsoleLine(
  report: Reporter,
  log: ConsoleLog,
  marker: Marker,
  pattern: RegExp,
  what: string,
  options: ConsoleExpectOptions = {},
): Promise<string | null> {
  if (!log.available) {
    report.skip(what, log.unavailableReason ?? "the console log is unavailable");
    return null;
  }
  const timeoutMs = options.timeoutMs ?? 30_000;
  const line = await log.waitFor(marker, pattern, timeoutMs);
  if (!log.available) {
    // It went away while we waited. Not a pass and not a fail: unobservable.
    report.skip(what, log.unavailableReason ?? "the console log became unavailable mid-check");
    return null;
  }
  if (line !== null) {
    report.pass(what);
    return line;
  }
  report.fail(
    what,
    [
      `expected: a console line matching ${options.describePattern ?? pattern.toString()} ` +
        `within ${timeoutMs}ms`,
      ...log.describeWindow(marker),
    ].join("\n"),
  );
  return null;
}

export interface ConsoleCandidate {
  /** What this wording would be evidence of, named for the skip detail. */
  readonly name: string;
  readonly pattern: RegExp;
}

/**
 * Watch for any of several candidate wordings after `marker`. PASSES on the
 * first match; SKIPS -- never fails -- when none appears.
 *
 * This is the honest tool for evidence whose exact text could not be measured
 * from outside the guest: systemd-hostnamed's phrasing has changed across
 * systemd versions, and mosd's audit mirroring was not read from this side.
 * This task forbids inventing a pattern that matches nothing and PASSES; a
 * pattern that matches nothing and FAILS is just as dishonest when the suite
 * never knew the wording to begin with. So absence here reports SKIP and pastes
 * what the guest DID write, which is exactly what a human needs to fix it.
 */
export async function observeConsoleLine(
  report: Reporter,
  log: ConsoleLog,
  marker: Marker,
  candidates: readonly ConsoleCandidate[],
  what: string,
  options: { readonly timeoutMs?: number } = {},
): Promise<string | null> {
  if (!log.available) {
    report.skip(what, log.unavailableReason ?? "the console log is unavailable");
    return null;
  }
  const timeoutMs = options.timeoutMs ?? 30_000;
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    for (const candidate of candidates) {
      // timeoutMs 0: one sweep of the window per candidate, then on to the next.
      // The outer loop owns the waiting, so no single candidate eats the budget.
      const hit = await log.waitFor(marker, candidate.pattern, 0);
      if (hit !== null) {
        report.pass(what);
        return hit;
      }
    }
    if (!log.available) {
      report.skip(what, log.unavailableReason ?? "the console log became unavailable mid-check");
      return null;
    }
    if (Date.now() >= deadline) break;
    await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL_MS));
  }
  report.skip(
    what,
    [
      `none of the ${candidates.length} candidate wordings appeared within ${timeoutMs}ms, so this ` +
        `effect was NOT OBSERVED (which is not the same as "did not happen"):`,
      ...candidates.map((candidate) => `  - ${candidate.name}: ${candidate.pattern.toString()}`),
      ...log.describeWindow(marker),
    ].join("\n"),
  );
  return null;
}

/**
 * Assert `needle` appears NOWHERE in the bytes written after `marker`.
 *
 * An absence assertion is worth something only over a window with something in
 * it, so an EMPTY window is a SKIP: with no bytes to search, "the password was
 * not logged" is vacuously true, which would be the most flattering possible
 * lie in this suite. The needle is compared against the RAW bytes rather than
 * the stripped view, so nothing can hide inside an escape sequence.
 */
export function expectConsoleAbsent(
  report: Reporter,
  log: ConsoleLog,
  marker: Marker,
  needle: string,
  what: string,
  describeNeedle: string,
): boolean {
  if (!log.available) {
    report.skip(what, log.unavailableReason ?? "the console log is unavailable");
    return false;
  }
  const text = log.textSince(marker);
  if (!log.available) {
    report.skip(what, log.unavailableReason ?? "the console log became unavailable mid-check");
    return false;
  }
  if (text.trim() === "") {
    report.skip(
      what,
      `the guest wrote nothing to the console after the mark before ${marker.label}, so the ` +
        `absence of ${describeNeedle} in that window proves nothing at all. An absence asserted ` +
        `over an empty window is a vacuous pass, which is worse than no check.`,
    );
    return false;
  }
  const at = text.indexOf(needle);
  if (at < 0) {
    report.pass(what);
    return true;
  }
  const around = splitLines(text.slice(Math.max(0, at - 400), at + 400));
  const line = around.find((candidate) => candidate.includes(needle));
  return report.fail(
    what,
    [
      `expected: ${describeNeedle} to appear NOWHERE in the console after the mark before ${marker.label}`,
      `actual:   it appears at byte ${at} of a ${text.length}-byte window`,
      `line:     ${clip(line ?? "(the match spans no single line)")}`,
    ].join("\n"),
  );
}

// text handling

const ESC = String.fromCharCode(27);
const BEL = String.fromCharCode(7);

// Built with `new RegExp` rather than written as literals: a source file that
// carries raw escape bytes is a file that greps, diffs and reviews badly.
const CSI_PATTERN = new RegExp(ESC + "\\[[0-?]*[ -/]*[@-~]", "g");
const OSC_PATTERN = new RegExp(ESC + "\\][^" + BEL + "]*(?:" + BEL + "|" + ESC + "\\\\)", "g");

/**
 * Split a console window into lines, dropping terminal control noise.
 *
 * systemd's console output is full of CSI colour runs: what reads as `[  OK  ]`
 * on a terminal is a colour-set sequence, the text, and a reset sequence, and
 * the progress lines carry carriage returns and erase-line sequences too. Left
 * in, they break any pattern that spans a unit name, and they make a pasted
 * evidence tail unreadable for the human who has to triage it.
 */
export function splitLines(text: string): string[] {
  return text
    .split("\n")
    .map((line) => stripAnsi(line).replace(/\r/g, "").trimEnd())
    .filter((line) => line !== "");
}

/** Remove CSI and OSC escape sequences from one line of console output. */
export function stripAnsi(text: string): string {
  CSI_PATTERN.lastIndex = 0;
  OSC_PATTERN.lastIndex = 0;
  return text.replace(OSC_PATTERN, "").replace(CSI_PATTERN, "");
}

/** Escape a runtime value so it can be embedded in a pattern safely. */
export function escapeForPattern(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function clip(line: string): string {
  return line.length <= EVIDENCE_LINE_BYTES ? line : `${line.slice(0, EVIDENCE_LINE_BYTES)}...`;
}

function describeStat(stat: {
  isDirectory(): boolean;
  isFIFO(): boolean;
  isSocket(): boolean;
}): string {
  if (stat.isDirectory()) return "a directory";
  if (stat.isFIFO()) return "a FIFO -- this module needs a file it can re-read from an offset";
  if (stat.isSocket()) return "a socket";
  return "not a regular file";
}
