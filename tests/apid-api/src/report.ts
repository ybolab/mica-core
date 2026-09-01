/**
 * Reporting in this repository's house format -- the same shape
 * `the verification contract` prints: one `PASS:`/`FAIL:` line per assertion and a
 * final `RESULT: PASS (n/m checks)`.
 *
 * Two rules are load-bearing and are why this is a module rather than a pair
 * of console.log calls:
 *
 *   - Totals are DYNAMIC. A hand-maintained expected count drifts, and the
 *     first thing that drifts is downward, which reads as a clean run.
 *   - A SKIP is NOT a pass. It gets its own line, it is excluded from the
 *     RESULT denominator, and it never contributes to the numerator. Counting
 *     skips as passes turns an unmade assertion into a green one.
 */

import { writeFileSync } from "node:fs";
import { parseSetCookie, type HttpResponse } from "./client.ts";

export type CheckStatus = "pass" | "fail" | "skip";

export interface CheckRecord {
  readonly name: string;
  readonly status: CheckStatus;
  readonly detail?: string;
}

export interface PhaseRecord {
  readonly id: string;
  readonly title: string;
  readonly assumes: string;
  status: CheckStatus;
  readonly checks: CheckRecord[];
}

export interface ReporterOptions {
  readonly host: string;
  /** Where each line goes. Defaults to stdout; the selftest captures instead. */
  readonly out?: (line: string) => void;
  readonly resultJsonPath?: string | undefined;
  /** See `APID_NEGATIVE` below. */
  readonly negative?: string | undefined;
  /** Default true. The selftest's probe reporters must not poison the exit code. */
  readonly setExitCode?: boolean;
}

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };

const SCHEMA_VERSION = 1;
const DETAIL_INDENT = "    ";
const BODY_SNIPPET = 240;

export class Reporter {
  readonly #out: (line: string) => void;
  readonly #host: string;
  readonly #resultJsonPath: string | undefined;
  readonly #negative: string | undefined;
  readonly #setExitCode: boolean;
  readonly #startedAt = new Date().toISOString();
  readonly #phases: PhaseRecord[] = [];
  #current: PhaseRecord | undefined;
  #negativeApplied = false;
  #passes = 0;
  #failures = 0;
  #skips = 0;

  constructor(options: ReporterOptions) {
    this.#out = options.out ?? ((line) => console.log(line));
    this.#host = options.host;
    this.#resultJsonPath = options.resultJsonPath;
    this.#negative = options.negative;
    this.#setExitCode = options.setExitCode ?? true;
  }

  get passes(): number {
    return this.#passes;
  }
  get failures(): number {
    return this.#failures;
  }
  get skips(): number {
    return this.#skips;
  }
  get phases(): readonly PhaseRecord[] {
    return this.#phases;
  }

  /** The most recent check. The selftest asserts on its status and message. */
  lastCheck(): CheckRecord | undefined {
    return this.#current?.checks.at(-1);
  }

  /** An unadorned line: phase headers, and the loud notices about partial runs. */
  note(line: string): void {
    this.#out(line);
  }

  beginPhase(id: string, title: string, assumes: string): void {
    this.#current = { id, title, assumes, status: "pass", checks: [] };
    this.#phases.push(this.#current);
  }

  endPhase(): void {
    const phase = this.#current;
    this.#current = undefined;
    if (phase === undefined || phase.status === "fail") return;
    // A phase that asserted NOTHING must not read as a pass. This mirrors the
    // whole-run zero-check guard: `beginPhase` starts a phase optimistically at
    // "pass" and only a
    // FAIL moves it, so a phase with nothing but skips kept the optimism.
    if (!phase.checks.some((check) => check.status === "pass")) {
      phase.status = "skip";
    }
  }

  /** Record a whole phase that never ran. Reported, never silently dropped. */
  skipPhase(id: string, title: string, assumes: string, why: string): void {
    const record: PhaseRecord = { id, title, assumes, status: "skip", checks: [] };
    this.#phases.push(record);
    this.#skips += 1;
    record.checks.push({ name: id, status: "skip", detail: why });
    this.#out(`SKIP: ${id} -- ${why}`);
  }

  pass(text: string): boolean {
    return this.#record("pass", text, undefined);
  }

  fail(text: string, detail?: string): boolean {
    return this.#record("fail", text, detail);
  }

  skip(text: string, why: string): void {
    this.#phase().checks.push({ name: text, status: "skip", detail: why });
    this.#skips += 1;
    this.#out(`SKIP: ${text} -- ${why}`);
  }

  /** The primitive every helper below funnels through. Returns true on pass. */
  check(ok: boolean, what: string, detail?: string): boolean {
    return ok ? this.pass(what) : this.fail(what, detail);
  }

  // -- assertion helpers ----------------------------------------------------
  //
  // Each helper owns its failure message and names the expected AND the actual
  // value in the terms of the thing it checked. A shared generic message makes
  // a red run unreadable: "assertion failed" tells a reader to go and reproduce
  // the whole run by hand.

  expectStatus(
    response: HttpResponse,
    expected: number | readonly number[],
    what: string,
  ): boolean {
    const allowed = typeof expected === "number" ? [expected] : [...expected];
    const ok = allowed.includes(response.status);
    return this.check(
      ok,
      what,
      ok
        ? undefined
        : [
            `expected: status ${allowed.join(" or ")}`,
            `actual:   status ${response.status}${response.statusText === "" ? "" : ` ${response.statusText}`}`,
            `request:  ${response.method} ${response.requestTarget}`,
          ].join("\n"),
    );
  }

  expectHeader(
    response: HttpResponse,
    name: string,
    expected: string,
    what: string,
  ): boolean {
    const actual = response.headers.get(name);
    const ok = actual === expected;
    return this.check(
      ok,
      what,
      ok
        ? undefined
        : [
            `header:   ${name}`,
            `expected: ${JSON.stringify(expected)}`,
            `actual:   ${describeHeaderValue(actual, response)}`,
            `request:  ${response.method} ${response.requestTarget}`,
          ].join("\n"),
    );
  }

  expectHeaderMatches(
    response: HttpResponse,
    name: string,
    pattern: RegExp,
    what: string,
  ): boolean {
    const actual = response.headers.get(name);
    const ok = actual !== undefined && pattern.test(actual);
    return this.check(
      ok,
      what,
      ok
        ? undefined
        : [
            `header:   ${name}`,
            `expected: a value matching ${pattern.toString()}`,
            `actual:   ${describeHeaderValue(actual, response)}`,
            `request:  ${response.method} ${response.requestTarget}`,
          ].join("\n"),
    );
  }

  expectBodyContains(response: HttpResponse, needle: string, what: string): boolean {
    const ok = response.body.includes(needle);
    return this.check(
      ok,
      what,
      ok
        ? undefined
        : [
            `expected: a body containing ${JSON.stringify(needle)}`,
            `actual:   ${response.body.length} bytes, beginning ${JSON.stringify(snippet(response.body))}`,
            `request:  ${response.method} ${response.requestTarget}`,
          ].join("\n"),
    );
  }

  /**
   * Compare the body, parsed as JSON, against `expected`. With
   * `{ subset: true }` extra keys in the response are tolerated; by default
   * the match is exact, which is what an envelope assertion wants.
   */
  expectJson(
    response: HttpResponse,
    expected: JsonValue,
    what: string,
    options: { readonly subset?: boolean } = {},
  ): boolean {
    let actual: JsonValue;
    try {
      actual = JSON.parse(response.body) as JsonValue;
    } catch (error) {
      return this.fail(
        what,
        [
          `expected: JSON ${JSON.stringify(expected)}`,
          `actual:   a body that is not JSON (${String(error)})`,
          `body:     ${JSON.stringify(snippet(response.body))}`,
        ].join("\n"),
      );
    }
    const ok = deepMatch(expected, actual, options.subset === true);
    return this.check(
      ok,
      what,
      ok
        ? undefined
        : [
            `expected: ${options.subset === true ? "JSON containing " : "JSON "}${JSON.stringify(expected)}`,
            `actual:   JSON ${JSON.stringify(actual)}`,
            `request:  ${response.method} ${response.requestTarget}`,
          ].join("\n"),
    );
  }

  /**
   * Poll until `probe` returns true. For the reboot phase, where the device is
   * legitimately unreachable for a while and "unreachable" must not be a
   * failure until the deadline passes.
   */
  async expectEventually(
    what: string,
    probe: () => boolean | Promise<boolean>,
    options: { readonly timeoutMs?: number; readonly intervalMs?: number } = {},
  ): Promise<boolean> {
    const timeoutMs = options.timeoutMs ?? 60_000;
    const intervalMs = options.intervalMs ?? 1_000;
    const deadline = Date.now() + timeoutMs;
    let attempts = 0;
    let lastError: string | undefined;
    for (;;) {
      attempts += 1;
      try {
        if (await probe()) return this.pass(what);
        lastError = undefined;
      } catch (error) {
        lastError = String(error);
      }
      if (Date.now() >= deadline) break;
      await new Promise((resolve) => setTimeout(resolve, intervalMs));
    }
    return this.fail(
      what,
      [
        `expected: the condition to hold within ${timeoutMs}ms`,
        `actual:   still false after ${attempts} probe(s) every ${intervalMs}ms`,
        `last:     ${lastError ?? "the probe returned false without throwing"}`,
      ].join("\n"),
    );
  }

  /**
   * Assert the attributes on one `Set-Cookie` line.
   *
   * `SameSite=Lax` and the API's CSRF token defend different request paths;
   * `HttpOnly` prevents scripts from reading the session credential. Losing
   * any cookie attribute is silent in functional checks, hence this helper.
   *
   * `required` entries are either a flag (`"HttpOnly"`) or `name=value`
   * (`"SameSite=Lax"`); names and values compare case-insensitively, as a
   * user agent treats them.
   */
  expectCookieAttributes(
    setCookieLine: string | undefined,
    cookieName: string,
    required: readonly string[],
    what: string,
  ): boolean {
    if (setCookieLine === undefined) {
      return this.fail(
        what,
        [
          `cookie:   ${cookieName}`,
          `expected: a Set-Cookie line carrying ${required.join(", ")}`,
          `actual:   no Set-Cookie line was offered at all`,
        ].join("\n"),
      );
    }
    const cookie = parseSetCookie(setCookieLine);
    if (cookie === undefined || cookie.name !== cookieName) {
      return this.fail(
        what,
        [
          `cookie:   ${cookieName}`,
          `expected: a Set-Cookie line named ${cookieName}`,
          `actual:   ${JSON.stringify(setCookieLine)}`,
        ].join("\n"),
      );
    }
    const missing = required.filter((wanted) => !hasAttribute(cookie.attributes, wanted));
    return this.check(
      missing.length === 0,
      what,
      missing.length === 0
        ? undefined
        : [
            `cookie:   ${cookieName}`,
            `expected: attributes ${required.join(", ")}`,
            `missing:  ${missing.join(", ")}`,
            `actual:   ${JSON.stringify(setCookieLine)}`,
          ].join("\n"),
    );
  }

  // -- finishing ------------------------------------------------------------

  finish(): void {
    if (this.#negative !== undefined && !this.#negativeApplied) {
      // A typo in APID_NEGATIVE would otherwise leave the run green and be
      // read as "the inverted check still passed", i.e. as evidence.
      //
      // Disarm the inverter first. This failure's own text quotes the string
      // being searched for, so it matches itself: without this line the
      // inverter flips the "nothing matched" failure into a PASS and a typo'd
      // APID_NEGATIVE reads as a clean run. The selftest caught exactly that.
      this.#negativeApplied = true;
      this.fail(
        `APID_NEGATIVE named ${JSON.stringify(this.#negative)} but no check matched it`,
        [
          `expected: exactly one check whose "<phase>: <text>" contains that string`,
          `actual:   none of the ${this.#passes + this.#failures} checks matched`,
        ].join("\n"),
      );
    }

    if (this.#resultJsonPath !== undefined) {
      try {
        writeFileSync(this.#resultJsonPath, `${JSON.stringify(this.#buildResult(), null, 2)}\n`);
      } catch (error) {
        this.fail(
          `the machine-readable result was written to ${this.#resultJsonPath}`,
          [`expected: a writable path`, `actual:   ${String(error)}`].join("\n"),
        );
      }
    }

    const total = this.#passes + this.#failures;
    if (this.#skips > 0) {
      this.#out(
        `NOTE: ${this.#skips} check(s)/phase(s) were SKIPPED and are counted as neither pass nor fail.`,
      );
    }
    this.#out(
      `RESULT: ${this.#failures === 0 ? "PASS" : "FAIL"} (${this.#passes}/${total} checks)`,
    );
    if (this.#setExitCode && this.#failures > 0) process.exitCode = 1;
  }

  #buildResult(): JsonValue {
    return {
      schemaVersion: SCHEMA_VERSION,
      host: this.#host,
      startedAt: this.#startedAt,
      finishedAt: new Date().toISOString(),
      phases: this.#phases.map((phase) => ({
        id: phase.id,
        title: phase.title,
        assumes: phase.assumes,
        status: phase.status,
        checks: phase.checks.map((check) => ({
          name: check.name,
          status: check.status,
          detail: check.detail ?? null,
        })),
      })),
      totals: { pass: this.#passes, fail: this.#failures, skip: this.#skips },
    };
  }

  #phase(): PhaseRecord {
    if (this.#current === undefined) {
      this.#current = { id: "(no phase)", title: "", assumes: "n/a", status: "pass", checks: [] };
      this.#phases.push(this.#current);
    }
    return this.#current;
  }

  #record(status: "pass" | "fail", text: string, detail: string | undefined): boolean {
    const phase = this.#phase();

    // APID_NEGATIVE inverts the verdict of the first matching check, so that
    // "this suite can go red" is a thing anyone can demonstrate on a live run
    // in one command, rather than a claim in a report.
    let effective = status;
    if (
      this.#negative !== undefined &&
      !this.#negativeApplied &&
      `${phase.id}: ${text}`.toLowerCase().includes(this.#negative.toLowerCase())
    ) {
      this.#negativeApplied = true;
      effective = status === "pass" ? "fail" : "pass";
      this.#out(
        `NOTE: APID_NEGATIVE inverted this check (${status} -> ${effective}); this run is expected to be RED.`,
      );
      detail = detail ?? "verdict inverted by APID_NEGATIVE";
    }

    phase.checks.push({ name: text, status: effective, detail });
    if (effective === "pass") {
      this.#passes += 1;
      this.#out(`PASS: ${text}`);
    } else {
      this.#failures += 1;
      phase.status = "fail";
      this.#out(`FAIL: ${text}`);
      if (detail !== undefined) {
        for (const line of detail.split("\n")) this.#out(`${DETAIL_INDENT}${line}`);
      }
    }
    return effective === "pass";
  }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

function describeHeaderValue(actual: string | undefined, response: HttpResponse): string {
  if (actual !== undefined) return JSON.stringify(actual);
  const present = response.headers.names();
  return `<absent> (headers present: ${present.length === 0 ? "none" : present.join(", ")})`;
}

function snippet(body: string): string {
  return body.length <= BODY_SNIPPET ? body : `${body.slice(0, BODY_SNIPPET)}...`;
}

function hasAttribute(attributes: ReadonlyMap<string, string>, wanted: string): boolean {
  const eq = wanted.indexOf("=");
  if (eq < 0) return attributes.has(wanted.toLowerCase());
  const name = wanted.slice(0, eq).trim().toLowerCase();
  const value = wanted.slice(eq + 1).trim().toLowerCase();
  const actual = attributes.get(name);
  return actual !== undefined && actual.toLowerCase() === value;
}

export function deepMatch(expected: JsonValue, actual: JsonValue, subset: boolean): boolean {
  if (expected === null || actual === null) return expected === actual;
  if (Array.isArray(expected) || Array.isArray(actual)) {
    if (!Array.isArray(expected) || !Array.isArray(actual)) return false;
    if (expected.length !== actual.length) return false;
    return expected.every((item, index) => {
      const other = actual[index];
      return other !== undefined && deepMatch(item, other, subset);
    });
  }
  if (typeof expected === "object" || typeof actual === "object") {
    if (typeof expected !== "object" || typeof actual !== "object") return false;
    const expectedKeys = Object.keys(expected);
    if (!subset && expectedKeys.length !== Object.keys(actual).length) return false;
    return expectedKeys.every((key) => {
      if (!Object.hasOwn(actual, key)) return false;
      const want = expected[key];
      const got = actual[key];
      if (want === undefined || got === undefined) return want === got;
      return deepMatch(want, got, subset);
    });
  }
  return expected === actual;
}
