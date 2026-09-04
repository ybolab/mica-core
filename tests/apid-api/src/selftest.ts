/**
 * The proof that this suite can fail.
 *
 * A check only ever observed passing is not evidence here -- the shape
 * `the verification contract`, `docs-verify-test` and `os-layout-lint-test`
 * establish. Every assertion helper, the cookie jar, the redirect refusal, the
 * verbatim writer and the phase runner are driven against wrong inputs and each
 * must fail with its own message, with positive controls alongside because a
 * helper hardwired to fail satisfies the negatives alone. It needs no network,
 * docker, QEMU or image: the peer is a stub HTTP server on loopback over
 * `node:net`, carrying no private key, so `inspectCertificate()` is exercised
 * against apid in phase 01 instead -- a stated limit, taken over a TLS key that
 * would be a secret-scan alarm and an expiry cliff.
 */
import * as net from "node:net";

import {
  Client,
  CookieJar,
  CrossAuthorityRedirectError,
  UNCHECKED,
  checkbox,
  encodeForm,
  sniServerName,
} from "./client.ts";
import { ConfigError, loadConfig, type Config } from "./config.ts";
import { Reporter } from "./report.ts";
import { EmptyAssumesError, runPhases, type Phase, type PhaseContext } from "./runner.ts";
import {
  ESP_GRUB_CFG,
  NoLinuxLineError,
  applyKernelAppend,
  espOffsetBytes,
  readBoardEnv,
  type AppendResult,
} from "./qemu.ts";

// the stub peer

interface StubReply {
  readonly status?: number;
  readonly statusText?: string;
  readonly headers?: ReadonlyArray<readonly [string, string]>;
  readonly body?: string;
  /** Reply with the request body, for asserting what the client encoded. */
  readonly echoBody?: boolean;
}

const APID_SHAPED_COOKIE =
  "apid_session=deadbeef.cafebabe; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=86400";

const ROUTES = new Map<string, StubReply>([
  ["/status-500", { status: 500, statusText: "Internal Server Error", body: "boom" }],
  ["/text", { body: "hello selftest" }],
  ["/cookie-good", { headers: [["Set-Cookie", APID_SHAPED_COOKIE]], body: "ok" }],
  [
    "/cookie-no-httponly",
    {
      // Everything apid sends except HttpOnly. Every functional assertion in
      // the suite still passes against this; only a dedicated attribute check
      // notices, which is why one exists.
      headers: [
        ["Set-Cookie", "apid_session=deadbeef.cafebabe; Path=/; Secure; SameSite=Lax; Max-Age=86400"],
      ],
      body: "ok",
    },
  ],
  [
    "/redirect-foreign",
    {
      status: 308,
      statusText: "Permanent Redirect",
      // The shape of apid's :80 -> :443 redirect: an authority that is not
      // reachable through the port forward.
      headers: [["Location", "https://127.0.0.1:443/"]],
    },
  ],
  ["/redirect-local", { status: 303, statusText: "See Other", headers: [["Location", "/landed"]] }],
  ["/landed", { body: "landed here" }],
  [
    "/json-envelope",
    {
      status: 404,
      statusText: "Not Found",
      headers: [["Content-Type", "application/json"]],
      body: JSON.stringify({ error: { code: "nope", message: "wrong", source: "stub" } }),
    },
  ],
  ["/not-json", { status: 404, statusText: "Not Found", body: "<!doctype html><p>nope" }],
  ["/echo-form", { echoBody: true }],
]);

interface Stub {
  readonly port: number;
  close(): void;
}

async function startStub(): Promise<Stub> {
  const server = net.createServer((socket) => {
    let buffer = Buffer.alloc(0);
    let answered = false;
    socket.on("error", () => undefined);
    socket.on("data", (chunk: Buffer) => {
      if (answered) return;
      buffer = Buffer.concat([buffer, chunk]);
      const separator = buffer.indexOf("\r\n\r\n");
      if (separator < 0) return;

      const head = buffer.subarray(0, separator).toString("latin1");
      const headLines = head.split("\r\n");
      const requestLine = headLines[0] ?? "";
      const firstSpace = requestLine.indexOf(" ");
      const lastSpace = requestLine.lastIndexOf(" ");
      if (firstSpace < 0 || lastSpace <= firstSpace) return;

      const method = requestLine.slice(0, firstSpace);
      // The request target, taken as bytes off the request line. No URL
      // object, no percent-decode, no dot-segment collapse -- if this stub
      // normalised, the verbatim-target case below would be testing the stub.
      const target = requestLine.slice(firstSpace + 1, lastSpace);

      const received = new Map<string, string>();
      for (const line of headLines.slice(1)) {
        const colon = line.indexOf(":");
        if (colon < 0) continue;
        received.set(line.slice(0, colon).trim().toLowerCase(), line.slice(colon + 1).trim());
      }

      const declared = Number(received.get("content-length") ?? "0");
      const wanted = Number.isFinite(declared) ? declared : 0;
      const bodyStart = separator + 4;
      if (buffer.length - bodyStart < wanted) return;
      answered = true;

      const body = buffer.subarray(bodyStart, bodyStart + wanted).toString("utf8");
      const reply = ROUTES.get(target) ?? { body: `echo ${target}` };
      const payload = Buffer.from(reply.echoBody === true ? body : (reply.body ?? ""), "utf8");
      const lines = [
        `HTTP/1.1 ${reply.status ?? 200} ${reply.statusText ?? "OK"}`,
        `Content-Length: ${payload.length}`,
        `X-Received-Target: ${target}`,
        `X-Received-Method: ${method}`,
        `X-Received-Cookie: ${received.get("cookie") ?? ""}`,
        "Connection: close",
      ];
      for (const [name, value] of reply.headers ?? []) lines.push(`${name}: ${value}`);
      socket.end(Buffer.concat([Buffer.from(`${lines.join("\r\n")}\r\n\r\n`, "latin1"), payload]));
    });
  });

  const port = await new Promise<number>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        reject(new Error(`the stub server bound to an unusable address: ${String(address)}`));
        return;
      }
      resolve(address.port);
    });
  });

  return { port, close: () => server.close() };
}

// probes: a captured Reporter, so a helper's failure can be inspected

interface Probe {
  readonly reporter: Reporter;
  readonly lines: string[];
}

function probe(): Probe {
  const lines: string[] = [];
  const reporter = new Reporter({
    host: "probe",
    out: (line) => lines.push(line),
    // A probe must never touch the real exit code: it is *expected* to be red.
    setExitCode: false,
  });
  reporter.beginPhase("probe", "probe", "n/a");
  return { reporter, lines };
}

const outer = new Reporter({ host: "127.0.0.1 (local stub)" });
/** Every negative case's failure message, to prove they are not one generic string. */
const failureMessages = new Map<string, string>();

/**
 * Run `drive` against a deliberately-wrong input and require that it produced a
 * FAIL naming each of `mustMention`. Any of: passing, staying silent, or
 * failing with a message that does not name the values, is itself a failure.
 */
async function requireRejected(
  name: string,
  drive: (reporter: Reporter) => void | Promise<void>,
  mustMention: readonly string[],
): Promise<void> {
  const p = probe();
  try {
    await drive(p.reporter);
  } catch (error) {
    outer.fail(name, [`expected: a recorded FAIL`, `actual:   the helper threw: ${String(error)}`].join("\n"));
    return;
  }
  const last = p.reporter.lastCheck();
  if (last === undefined) {
    outer.fail(name, [`expected: a recorded FAIL`, `actual:   the helper recorded no check at all`].join("\n"));
    return;
  }
  if (last.status !== "fail") {
    outer.fail(
      name,
      [
        `expected: the helper to record a FAIL on wrong input`,
        `actual:   it recorded "${last.status}" for ${JSON.stringify(last.name)}`,
      ].join("\n"),
    );
    return;
  }
  const detail = last.detail ?? "";
  const missing = mustMention.filter((token) => !detail.toLowerCase().includes(token.toLowerCase()));
  if (missing.length > 0) {
    outer.fail(
      name,
      [
        `expected: a failure message naming ${mustMention.join(", ")}`,
        `missing:  ${missing.join(", ")}`,
        `actual:   ${JSON.stringify(detail)}`,
      ].join("\n"),
    );
    return;
  }
  failureMessages.set(name, detail);
  outer.pass(name);
}

/**
 * `applyKernelAppend` with a refusal turned into a value, so that one case
 * driving it into a refusal does not take the rest of the phase down with it --
 * a pattern that matches nothing makes it throw, and that is exactly what the
 * cases below are measuring.
 */
function appended(text: string, append: string): AppendResult & { readonly refusal: string } {
  try {
    return { ...applyKernelAppend(text, append), refusal: "" };
  } catch (error) {
    return { text, applied: false, matched: 0, refusal: String(error) };
  }
}

/** The other direction: a CORRECT input must be accepted, or the negatives above
 *  are satisfied by a helper that simply always fails. */
function requireAccepted(name: string, drive: (reporter: Reporter) => void): void {
  const p = probe();
  drive(p.reporter);
  const last = p.reporter.lastCheck();
  outer.check(
    last?.status === "pass",
    name,
    last?.status === "pass"
      ? undefined
      : [
          `expected: the helper to record a PASS on correct input`,
          `actual:   ${last === undefined ? "no check at all" : `"${last.status}": ${last.detail ?? ""}`}`,
        ].join("\n"),
  );
}

// the cases

const stub = await startStub();
const config: Config = loadConfig({
  APID_HOST: "127.0.0.1",
  APID_HTTP_PORT: String(stub.port),
  APID_HTTPS_PORT: String(stub.port),
});
const client = new Client(config);

try {
  // -- assertion helpers reject wrong answers, each in its own words ---------
  outer.beginPhase(
    "selftest-negative",
    "every assertion helper rejects a deliberately wrong answer",
    "the stub peer is listening on loopback",
  );
  outer.note("");
  outer.note("PHASE selftest-negative: every assertion helper rejects a deliberately wrong answer");

  const five00 = await client.http("/status-500");
  await requireRejected(
    "expectStatus rejects a 500 where 200 was required, naming both",
    (r) => void r.expectStatus(five00, 200, "the route answers 200"),
    ["200", "500"],
  );

  const text = await client.http("/text");
  await requireRejected(
    "expectHeader rejects an absent header, naming the header",
    (r) => void r.expectHeader(text, "location", "/ui", "the route sends a Location"),
    ["location", "absent"],
  );

  await requireRejected(
    "expectHeaderMatches rejects a non-matching value, naming the header and the pattern",
    (r) => void r.expectHeaderMatches(text, "x-received-method", /^POST$/, "the method was POST"),
    ["x-received-method", "POST", "GET"],
  );

  await requireRejected(
    "expectBodyContains rejects a body without the needle, naming the needle",
    (r) => void r.expectBodyContains(text, "Set a password", "the body offers the setup form"),
    ["Set a password"],
  );

  const envelope = await client.http("/json-envelope");
  await requireRejected(
    "expectJson rejects a mismatched envelope, printing expected and actual",
    (r) =>
      void r.expectJson(
        envelope,
        { error: { code: "not_found", message: "no API route at /api/nope", source: "apid" } },
        "the /api 404 envelope is exact",
      ),
    ["not_found", "nope", "apid", "stub"],
  );

  const notJson = await client.http("/not-json");
  await requireRejected(
    "expectJson rejects a body that is not JSON at all, quoting the body",
    (r) => void r.expectJson(notJson, { error: null }, "the /api 404 envelope is JSON"),
    ["not JSON", "doctype"],
  );

  await requireRejected(
    "expectEventually rejects a condition that never holds, naming the timeout",
    async (r) => {
      await r.expectEventually("the device comes back", () => false, {
        timeoutMs: 120,
        intervalMs: 40,
      });
    },
    ["120ms", "probe"],
  );

  // (c) A Set-Cookie missing HttpOnly. Everything else about it is correct.
  const weakCookie = await client.http("/cookie-no-httponly");
  await requireRejected(
    "the cookie-attribute assertion rejects a Set-Cookie missing HttpOnly",
    (r) =>
      void r.expectCookieAttributes(
        weakCookie.setCookie[0],
        "apid_session",
        ["Path=/", "HttpOnly", "Secure", "SameSite=Lax", "Max-Age=86400"],
        "the session cookie carries every attribute apid promises",
      ),
    ["HttpOnly", "apid_session"],
  );

  outer.check(
    new Set(failureMessages.values()).size === failureMessages.size,
    "each helper's failure message is its own, not one shared generic string",
    [
      `expected: ${failureMessages.size} distinct failure messages`,
      `actual:   ${new Set(failureMessages.values()).size} distinct`,
    ].join("\n"),
  );
  outer.endPhase();

  // -- and accept right answers ---------------------------------------------
  outer.beginPhase(
    "selftest-positive",
    "the same helpers accept a correct answer",
    "the negative cases above ran, so a helper that always fails would have passed them",
  );
  outer.note("");
  outer.note("PHASE selftest-positive: the same helpers accept a correct answer");

  requireAccepted("expectStatus accepts the status that was required", (r) =>
    void r.expectStatus(text, 200, "the route answers 200"),
  );
  requireAccepted("expectHeader accepts the header that was sent", (r) =>
    void r.expectHeader(text, "x-received-method", "GET", "the request was a GET"),
  );
  requireAccepted("expectHeaderMatches accepts a matching value", (r) =>
    void r.expectHeaderMatches(text, "x-received-target", /^\/text$/, "the target was /text"),
  );
  requireAccepted("expectBodyContains accepts a body carrying the needle", (r) =>
    void r.expectBodyContains(text, "hello selftest", "the body is the stub's"),
  );
  requireAccepted("expectJson accepts a matching envelope", (r) =>
    void r.expectJson(
      envelope,
      { error: { code: "nope", message: "wrong", source: "stub" } },
      "the envelope is exact",
    ),
  );
  const goodCookie = await client.http("/cookie-good");
  requireAccepted("the cookie-attribute assertion accepts apid's own cookie shape", (r) =>
    void r.expectCookieAttributes(
      goodCookie.setCookie[0],
      "apid_session",
      ["Path=/", "HttpOnly", "Secure", "SameSite=Lax", "Max-Age=86400"],
      "the session cookie carries every attribute apid promises",
    ),
  );
  outer.endPhase();

  // -- the client's browser behaviours --------------------------------------
  outer.beginPhase(
    "selftest-client",
    "the jar deletes, the follower refuses, and raw() does not rewrite",
    "the stub peer echoes the request target it actually received on the wire",
  );
  outer.note("");
  outer.note("PHASE selftest-client: the jar deletes, the follower refuses, and raw() does not rewrite");

  // (d) Max-Age=0 is a DELETION. Driven straight at the jar.
  const jar = new CookieJar();
  jar.ingest([APID_SHAPED_COOKIE]);
  const stored = jar.get("apid_session") !== undefined;
  jar.ingest(["apid_session=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"]);
  const dropped = jar.get("apid_session") === undefined;
  outer.check(
    stored && dropped,
    "the jar treats Max-Age=0 as a deletion, so a cleared session cookie is gone",
    [
      `expected: stored=true then get()=undefined after Max-Age=0`,
      `actual:   stored=${stored}, gone-after-clear=${dropped} (names now: ${jar.names().join(", ") || "none"})`,
    ].join("\n"),
  );

  // The jar is useless if it stores but never sends.
  await client.http("/cookie-good");
  const echoed = await client.http("/echo-cookie");
  outer.check(
    (echoed.headers.get("x-received-cookie") ?? "").includes("apid_session=deadbeef.cafebabe"),
    "the jar sends what it stored on the next request",
    [
      `expected: a Cookie header carrying apid_session=deadbeef.cafebabe`,
      `actual:   ${JSON.stringify(echoed.headers.get("x-received-cookie") ?? "<absent>")}`,
    ].join("\n"),
  );
  client.jar.clear();
  outer.check(
    client.jar.get("apid_session") === undefined,
    "jar.clear() empties the jar",
    `actual:   names still present: ${client.jar.names().join(", ") || "none"}`,
  );

  // (e) A Location on a different authority is refused, not followed.
  const foreign = await client.http("/redirect-foreign");
  let refusal: unknown;
  try {
    await client.follow(foreign);
  } catch (error) {
    refusal = error;
  }
  outer.check(
    refusal instanceof CrossAuthorityRedirectError,
    "follow() refuses a Location on a different authority instead of hanging on it",
    [
      `expected: CrossAuthorityRedirectError for Location https://127.0.0.1:443/`,
      `actual:   ${refusal === undefined ? "it followed the redirect" : String(refusal)}`,
    ].join("\n"),
  );

  // ...and the refusal is specific, not "follow() always throws".
  const local = await client.http("/redirect-local");
  let landed: string | undefined;
  try {
    landed = (await client.follow(local)).body;
  } catch (error) {
    landed = `threw: ${String(error)}`;
  }
  outer.check(
    landed === "landed here",
    "follow() does follow a same-authority Location, so the refusal above is specific",
    [`expected: the body "landed here"`, `actual:   ${JSON.stringify(landed)}`].join("\n"),
  );

  // (f) The verbatim request target. If bun ever starts normalising what raw()
  // writes, this goes red here rather than the traversal phase silently
  // asserting on a string the client rewrote.
  const encodedTraversal = "/%2e%2e%2fetc%2fpasswd";
  const rawEncoded = await client.raw(encodedTraversal, { scheme: "http" });
  outer.check(
    rawEncoded.headers.get("x-received-target") === encodedTraversal,
    "raw() delivers a percent-encoded traversal target byte-for-byte",
    [
      `expected: the peer to receive ${JSON.stringify(encodedTraversal)}`,
      `actual:   it received ${JSON.stringify(rawEncoded.headers.get("x-received-target") ?? "<absent>")}`,
    ].join("\n"),
  );

  const dotTraversal = "/../../etc/passwd";
  const rawDots = await client.raw(dotTraversal, { scheme: "http" });
  outer.check(
    rawDots.headers.get("x-received-target") === dotTraversal,
    "raw() delivers dot segments without collapsing them",
    [
      `expected: the peer to receive ${JSON.stringify(dotTraversal)}`,
      `actual:   it received ${JSON.stringify(rawDots.headers.get("x-received-target") ?? "<absent>")}`,
    ].join("\n"),
  );

  // ...and the reason raw() has to exist at all: fetch() rewrites the target
  // before it leaves the process (measured 2026-08-24).
  const viaFetch = await client.http(dotTraversal);
  outer.check(
    viaFetch.headers.get("x-received-target") !== dotTraversal,
    "fetch() does NOT deliver dot segments verbatim -- which is why raw() exists",
    [
      `expected: fetch to rewrite ${JSON.stringify(dotTraversal)} before the wire`,
      `actual:   the peer received it unchanged; raw() may no longer be necessary`,
    ].join("\n"),
  );

  // An unticked checkbox sends nothing, not "off".
  const posted = await client.post(
    "/echo-form",
    { password: "pw", hostname: "mos", dhcp: checkbox(false), dns: UNCHECKED },
    { scheme: "http" },
  );
  outer.check(
    posted.body === "password=pw&hostname=mos",
    "an unticked checkbox is omitted from the form body, as a browser omits it",
    [
      `expected: the body "password=pw&hostname=mos"`,
      `actual:   ${JSON.stringify(posted.body)}`,
      `encoded:  ${JSON.stringify(encodeForm({ dhcp: checkbox(true) }))} when ticked`,
    ].join("\n"),
  );

  // freshConnection() must still produce a usable response, over a new socket.
  client.freshConnection();
  const fresh = await client.http("/text");
  outer.check(
    fresh.transport === "socket" && fresh.status === 200 && fresh.body === "hello selftest",
    "freshConnection() forces the next request onto a new socket and still parses",
    [
      `expected: transport=socket, status=200, body="hello selftest"`,
      `actual:   transport=${fresh.transport}, status=${fresh.status}, body=${JSON.stringify(fresh.body)}`,
    ].join("\n"),
  );
  outer.endPhase();

  // -- the runner ------------------------------------------------------------
  outer.beginPhase(
    "selftest-runner",
    "the runner refuses an unstated assumption and contains a failure",
    "the phases above proved the reporter records what it is told",
  );
  outer.note("");
  outer.note("PHASE selftest-runner: the runner refuses an unstated assumption and contains a failure");

  const context = (reporter: Reporter): PhaseContext => ({
    client,
    report: reporter,
    config,
    state: new Map<string, unknown>(),
  });

  // (g) An empty `assumes` is refused before anything runs.
  const nameless: Phase = {
    id: "99-nameless",
    title: "a phase that never says what it assumes",
    assumes: "   ",
    async run() {
      throw new Error("this phase must never be reached");
    },
  };
  const refusalProbe = probe();
  let registryError: unknown;
  try {
    await runPhases([nameless], context(refusalProbe.reporter));
  } catch (error) {
    registryError = error;
  }
  outer.check(
    registryError instanceof EmptyAssumesError &&
      registryError.message.includes("99-nameless") &&
      refusalProbe.reporter.passes === 0,
    "the runner refuses a phase whose `assumes` is empty, before running anything",
    [
      `expected: EmptyAssumesError naming 99-nameless, and nothing run`,
      `actual:   ${registryError === undefined ? "the phase ran" : String(registryError)}`,
    ].join("\n"),
  );

  // (h) A failing phase skips every later phase; a SKIP is not a PASS.
  const made = (id: string, verdict: "pass" | "fail"): Phase => ({
    id,
    title: `synthetic ${id}`,
    assumes: `TODO: synthetic ${id} assumes the phase before it succeeded.`,
    async run(ctx) {
      if (verdict === "pass") ctx.report.pass(`${id} check`);
      else ctx.report.fail(`${id} check`, "deliberately red");
    },
  });
  const containment = probe();
  await runPhases(
    [made("01-a", "pass"), made("02-b", "fail"), made("03-c", "pass"), made("04-d", "pass")],
    context(containment.reporter),
  );
  containment.reporter.finish();
  const output = containment.lines.join("\n");
  const resultLine = containment.lines.find((line) => line.startsWith("RESULT:"));

  outer.check(
    containment.reporter.passes === 1 &&
      containment.reporter.failures === 1 &&
      containment.reporter.skips === 2,
    "a phase failure skips every later phase instead of running or passing them",
    [
      `expected: pass=1, fail=1, skip=2`,
      `actual:   pass=${containment.reporter.passes}, fail=${containment.reporter.failures}, skip=${containment.reporter.skips}`,
      `output:   ${JSON.stringify(output)}`,
    ].join("\n"),
  );
  outer.check(
    output.includes(
      "SKIP: 03-c -- not run: 02-b failed and this phase assumes TODO: synthetic 03-c",
    ) && !output.includes("PASS: 03-c"),
    "a skipped phase says which phase failed and what it had assumed, and prints no PASS",
    [
      `expected: a SKIP line for 03-c naming 02-b and 03-c's own assumption, and no PASS: 03-c`,
      `actual:   ${JSON.stringify(output)}`,
    ].join("\n"),
  );
  outer.check(
    resultLine === "RESULT: FAIL (1/2 checks)",
    "the RESULT line counts neither skipped phase as a pass",
    [
      `expected: "RESULT: FAIL (1/2 checks)" -- 2 skips excluded from both numerator and denominator`,
      `actual:   ${JSON.stringify(resultLine ?? "<no RESULT line>")}`,
    ].join("\n"),
  );

  // APID_NEGATIVE: the same proof, available on a live run in one command.
  const inverted = new Reporter({
    host: "probe",
    out: () => undefined,
    negative: "a check that would pass",
    setExitCode: false,
  });
  inverted.beginPhase("probe", "probe", "n/a");
  inverted.pass("a check that would pass");
  inverted.finish();
  outer.check(
    inverted.failures === 1 && inverted.passes === 0,
    "APID_NEGATIVE inverts a matching check, so a live run can be made red on demand",
    [`expected: pass=0, fail=1`, `actual:   pass=${inverted.passes}, fail=${inverted.failures}`].join("\n"),
  );

  const unmatched = new Reporter({
    host: "probe",
    out: () => undefined,
    negative: "no check says this",
    setExitCode: false,
  });
  unmatched.beginPhase("probe", "probe", "n/a");
  unmatched.pass("something else entirely");
  unmatched.finish();
  outer.check(
    unmatched.failures === 1,
    "APID_NEGATIVE that matches nothing fails the run rather than passing quietly",
    [`expected: fail=1 (a typo must not read as evidence)`, `actual:   fail=${unmatched.failures}`].join("\n"),
  );
  outer.endPhase();

  // -- the environment contract ---------------------------------------------
  outer.beginPhase(
    "selftest-config",
    "the environment contract fails loudly and early",
    "nothing; loadConfig is pure",
  );
  outer.note("");
  outer.note("PHASE selftest-config: the environment contract fails loudly and early");

  let missingHost: unknown;
  try {
    loadConfig({});
  } catch (error) {
    missingHost = error;
  }
  outer.check(
    missingHost instanceof ConfigError &&
      missingHost.message.includes("APID_HOST") &&
      missingHost.message.includes("127.0.0.1"),
    "a missing APID_HOST is refused with a message naming the variable and loopback",
    [
      `expected: ConfigError naming APID_HOST and 127.0.0.1`,
      `actual:   ${missingHost === undefined ? "it was accepted" : String(missingHost)}`,
    ].join("\n"),
  );

  let badPort: unknown;
  try {
    loadConfig({ APID_HOST: "10.0.0.1", APID_HTTPS_PORT: "not-a-port" });
  } catch (error) {
    badPort = error;
  }
  outer.check(
    badPort instanceof ConfigError && badPort.message.includes("APID_HTTPS_PORT"),
    "a malformed port is refused before a boot is spent on it",
    [
      `expected: ConfigError naming APID_HTTPS_PORT`,
      `actual:   ${badPort === undefined ? "it was accepted" : String(badPort)}`,
    ].join("\n"),
  );

  const defaults = loadConfig({ APID_HOST: "10.0.0.1" });
  outer.check(
    defaults.httpsPort === 18443 &&
      defaults.httpPort === 18080 &&
      defaults.adminPassword === "mos-e2e-admin-pw" &&
      defaults.hostnameTarget === "mos-e2e-renamed" &&
      defaults.phases === undefined,
    "the documented defaults are the defaults",
    [
      `expected: 18443/18080, mos-e2e-admin-pw, mos-e2e-renamed, all phases`,
      `actual:   ${defaults.httpsPort}/${defaults.httpPort}, ${defaults.adminPassword}, ${defaults.hostnameTarget}, ${String(defaults.phases)}`,
    ].join("\n"),
  );
  outer.endPhase();

  // -- SNI is never an IP literal -------------------------------------------
  //
  // This phase is PURE: no TLS, no key, no socket, not even the stub. That is
  // the point. The header above records a deliberate decision to carry no
  // private key, and the direct cost of that decision was that NOTHING in this
  // repository ever reached `tls.connect()` -- so `servername: <an IP>` shipped
  // and threw synchronously on the first live run, in `inspectCertificate()`
  // and in every raw HTTPS write, before a byte left the process. `APID_HOST`
  // is always an IP literal here, so that was every live run, all nine phases.
  //
  // Testing the pure decision function restores the guard without reopening the
  // no-key decision: `sniServerName` is what the two `tls.connect()` call sites
  // consult, and it is total, synchronous and network-free.
  outer.beginPhase(
    "selftest-sni",
    "SNI is omitted for an IP literal and sent for a hostname",
    "nothing; sniServerName is pure -- no TLS, no key, no socket",
  );
  outer.note("");
  outer.note("PHASE selftest-sni: SNI is omitted for an IP literal and sent for a hostname");

  // The three shapes APID_HOST takes: loopback, the QEMU user-net guest
  // address, and a container address on the shared docker network.
  const ipv4Hosts = ["127.0.0.1", "10.0.2.15", "172.18.0.8"];
  const ipv4Sent = ipv4Hosts.filter((host) => sniServerName(host) !== undefined);
  outer.check(
    ipv4Sent.length === 0,
    "an IPv4 literal yields no server name, so tls.connect() is never handed one",
    [
      `expected: undefined for ${ipv4Hosts.join(", ")}`,
      `actual:   ${ipv4Sent.length === 0 ? "undefined for all of them" : `a name for ${ipv4Sent.join(", ")}`}`,
    ].join("\n"),
  );

  const ipv6Hosts = ["::1", "2001:0db8:0000:0000:0000:ff00:0042:8329"];
  const ipv6Sent = ipv6Hosts.filter((host) => sniServerName(host) !== undefined);
  outer.check(
    ipv6Sent.length === 0,
    "an IPv6 literal yields no server name, in both the compressed and full forms",
    [
      `expected: undefined for ${ipv6Hosts.join(", ")}`,
      `actual:   ${ipv6Sent.length === 0 ? "undefined for both" : `a name for ${ipv6Sent.join(", ")}`}`,
    ].join("\n"),
  );

  // The positive control. Without it, a helper hardwired to return undefined
  // would satisfy both cases above -- and would silently drop SNI against a
  // device addressed by name.
  const nameHosts = ["mos.local", "localhost", "example.test"];
  const nameWrong = nameHosts.filter((host) => sniServerName(host) !== host);
  outer.check(
    nameWrong.length === 0,
    "a hostname is passed through unchanged, so SNI still goes out when it is legal",
    [
      `expected: each of ${nameHosts.join(", ")} returned unchanged`,
      `actual:   ${nameWrong.length === 0 ? "each returned unchanged" : nameWrong.map((h) => `${h} -> ${String(sniServerName(h))}`).join("; ")}`,
    ].join("\n"),
  );
  outer.endPhase();

  // -- the boot engine's four defects, as fixtures --------------------------
  //
  // src/qemu.ts is the port of the shell boot engine this harness used to call
  // out to, and the record four defects in the block of it that
  // rewrites the disk copy's ESP grub.cfg. Three of them (5.2, 5.3, 5.4) made
  // `make os-apid-api-test` unable to reach a boot AT ALL, because this harness
  // always sets MOS_QEMU_APPEND -- its readiness signal is the journald console
  // line that append produces -- and every one of them was found by running the
  // thing rather than by reading it. Each is a case below, with a positive
  // control beside it.
  //
  // No docker, no QEMU and no image: the offset is arithmetic over a board
  // layout and the rewrite is text, so both are driven on strings here. That is
  // the whole reason they are factored out of the run path.
  outer.beginPhase(
    "selftest-qemu",
    "the boot engine computes the ESP offset and rewrites the linux line correctly",
    "nothing; espOffsetBytes and applyKernelAppend are pure -- no docker, no QEMU, no image",
  );
  outer.note("");
  outer.note("PHASE selftest-qemu: the boot engine computes the ESP offset and rewrites the linux line correctly");

  // (5.2) The offset is the ESP's, and never the boot slot's. On x64 those are
  // two different partitions and only one holds a grub.cfg: the ESP at 1 MiB
  // carries EFI/mos/grub.cfg, BOOT-A at 65 MiB has no EFI directory at all.
  const layout = readBoardEnv(
    ["# boards/x64/board.env, cut to the two keys this decision reads",
      "ESP_START_MIB=1",
      "BOOT_A_START_MIB=65",
      "ESP_OFFSET_BYTES=$((ESP_START_MIB * MIB_BYTES))",
      ""].join("\n"),
  );
  outer.check(
    espOffsetBytes(layout) === 1 * 1048576,
    "the ESP grub.cfg offset is ESP_START_MIB * 1048576, on a layout that also defines BOOT_A_START_MIB",
    [
      `expected: 1048576 (1 MiB, the ESP)`,
      `actual:   ${espOffsetBytes(layout)}${espOffsetBytes(layout) === 65 * 1048576 ? " -- that is BOOT_A_START_MIB, the partition with no EFI directory" : ""}`,
    ].join("\n"),
  );

  // ...and a swap of the two keys is caught, which a reader of BOOT_A_START_MIB
  // would pass: with the values exchanged it would answer 1048576 here and look
  // right. The function has to follow the key, not the number.
  const swapped = readBoardEnv("ESP_START_MIB=65\nBOOT_A_START_MIB=1\n");
  outer.check(
    espOffsetBytes(swapped) === 65 * 1048576,
    "swapping ESP_START_MIB and BOOT_A_START_MIB moves the offset, so the key is what is read",
    [
      `expected: 68157440 -- it follows ESP_START_MIB wherever the layout puts it`,
      `actual:   ${espOffsetBytes(swapped)}${espOffsetBytes(swapped) === 1048576 ? " -- it read BOOT_A_START_MIB" : ""}`,
    ].join("\n"),
  );

  // A layout that stopped defining the key refuses by name. There is no default
  // offset that could be right, and 0 would read the GPT as a filesystem.
  let noEsp: unknown;
  try {
    espOffsetBytes(readBoardEnv("BOOT_A_START_MIB=65\n"));
  } catch (error) {
    noEsp = error;
  }
  outer.check(
    noEsp instanceof Error && noEsp.message.includes("ESP_START_MIB") && noEsp.message.includes(ESP_GRUB_CFG),
    "a layout defining no ESP_START_MIB is refused, naming the key and the file read at that offset",
    [
      `expected: an Error naming ESP_START_MIB and ${ESP_GRUB_CFG}`,
      `actual:   ${noEsp === undefined ? "it returned an offset" : String(noEsp)}`,
    ].join("\n"),
  );

  // (5.3) The indentation. boards/x64/grub.cfg indents both linux lines with
  // EIGHT spaces and always has, so the four-space pattern the shell carried
  // matched neither and the count came back 0 on every run.
  const GRUB_CFG = [
    'menuentry "mos slot A" --id A {',
    "    else",
    "        linux (${slot_a_root})/vmlinuz dm-mod.create=\"rootfs,,,ro,0 100 verity 1\"",
    "    fi",
    "}",
    'menuentry "mos slot B" --id B {',
    "        linux (${slot_b_root})/vmlinuz dm-mod.create=\"rootfs,,,ro,0 100 verity 1\"",
    "}",
    "",
  ].join("\n");
  const APPEND = "systemd.journald.forward_to_console=1";
  const eight = appended(GRUB_CFG, APPEND);
  outer.check(
    eight.matched === 2 && eight.text.split("\n").filter((l) => l.includes(APPEND)).length === 2,
    "an eight-space linux line is matched and rewritten, on both slots",
    [
      `expected: 2 linux lines matched and 2 carrying the append`,
      `actual:   ${eight.matched} matched, ${eight.text.split("\n").filter((l) => l.includes(APPEND)).length} carrying it`,
      `          ${eight.refusal === "" ? "no refusal" : eight.refusal}`,
    ].join("\n"),
  );
  outer.check(
    /^ {4}linux /m.test(GRUB_CFG) === false,
    "the four-space pattern the shell original carried would have matched nothing in that same file",
    [
      `expected: /^ {4}linux /m to match nothing -- the template indents with eight`,
      `actual:   it matched, so this fixture no longer reproduces the measured defect`,
    ].join("\n"),
  );

  // (5.4) Zero matches is a refusal that says so. In the shell this guard was
  // UNREACHABLE: `before=$(grep -c ...)` under `set -e` aborted the shell on a
  // count of zero, before the guard on the next line could run, so a completely
  // unmatched pattern presented as a prepare step that printed nothing and
  // failed. There is no such trap in TypeScript, so what is asserted here is the
  // semantic: no linux line means an error naming the file and saying the append
  // would have added nothing.
  let noLinux: unknown;
  try {
    applyKernelAppend("menuentry \"mos slot A\" --id A {\n    echo no kernel here\n}\n", APPEND);
  } catch (error) {
    noLinux = error;
  }
  outer.check(
    noLinux instanceof NoLinuxLineError &&
      noLinux.message.includes(ESP_GRUB_CFG) &&
      noLinux.message.includes("added nothing"),
    "a grub.cfg with no linux line is refused, naming the ESP grub.cfg and saying the append would have added nothing",
    [
      `expected: NoLinuxLineError naming ${ESP_GRUB_CFG} and "added nothing"`,
      `actual:   ${noLinux === undefined ? "it returned quietly, and the boot would have looked normal" : String(noLinux)}`,
    ].join("\n"),
  );
  // The positive control: the same call on a file that HAS a linux line must
  // not throw, or the refusal above is satisfied by a function that always does.
  let threwOnGoodInput = false;
  try {
    applyKernelAppend(GRUB_CFG, APPEND);
  } catch {
    threwOnGoodInput = true;
  }
  outer.check(
    threwOnGoodInput === false,
    "a grub.cfg that does have a linux line is not refused, so the refusal above is specific",
    `actual:   it threw on a file carrying two linux lines`,
  );

  // (5.5, last bullet) Idempotency. The append is applied on the prepare AND on
  // every boot that reuses the disk, so an unconditional one lands the same
  // arguments two or three times over a run. For forward_to_console=1 a repeat
  // is the same value twice and costs nothing; for `systemd.run=` systemd takes
  // each occurrence as another ExecStart and RUNS THE COMMAND AGAIN. Measured
  // 2026-08-28: a duplicated systemd.run executed the seeded script twice, back
  // to back, on one boot.
  const RUN_APPEND = 'systemd.run="/bin/bash /mnt/state/m7-net-smoke.sh"';
  const once = appended(GRUB_CFG, RUN_APPEND);
  const twice = appended(once.text, RUN_APPEND);
  const occurrences = twice.text.split(RUN_APPEND).length - 1;
  outer.check(
    once.applied === true && twice.applied === false && twice.text === once.text && occurrences === 2,
    "applying the same append twice adds it once, so a reuse boot does not run a seeded systemd.run= a second time",
    [
      `expected: applied=true then applied=false, the text unchanged, and 2 occurrences (one per slot)`,
      `actual:   applied=${once.applied} then ${twice.applied}, text ${twice.text === once.text ? "unchanged" : "REWRITTEN"}, ${occurrences} occurrences`,
    ].join("\n"),
  );

  // ...and the already-there test is a FIXED STRING, not a pattern. `.` and `*`
  // are ordinary characters in a kernel command line and are everywhere in
  // these values; read as a pattern, `systemd.run=x` matches `systemdXrunAx`
  // and the append is skipped as already present against a line that does not
  // carry it.
  const dotDecoy = GRUB_CFG.replace(/vmlinuz/g, "vmlinuz systemdXrun=x");
  const dot = appended(dotDecoy, "systemd.run=x");
  outer.check(
    dot.applied === true && dot.text.includes("systemd.run=x"),
    "an append carrying `.` is looked for as itself, so a line that matches it only as a pattern does not suppress it",
    [
      `expected: applied=true -- the line carries "systemdXrun=x", which is not "systemd.run=x"`,
      `actual:   applied=${dot.applied}; the append was read as a pattern, matched the decoy and was skipped`,
    ].join("\n"),
  );
  const starDecoy = GRUB_CFG.replace(/vmlinuz/g, "vmlinuz mos.debug=b");
  const star = appended(starDecoy, "mos.debug=a*b");
  outer.check(
    star.applied === true && star.text.includes("mos.debug=a*b"),
    "an append carrying `*` is likewise a literal: a line it would match only as a pattern is still appended to",
    [
      `expected: applied=true -- the line carries "mos.debug=b", which "a*b" matches only as a pattern`,
      `actual:   applied=${star.applied}; the append was read as a pattern and skipped`,
    ].join("\n"),
  );
  outer.endPhase();
} finally {
  stub.close();
}

outer.note("");
outer.finish();
