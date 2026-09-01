/**
 * The build-time half of `pkgs/mosd/tests/apid-api`: every phase literal that is ALSO
 * stated in `pkgs/mosd/apid/openapi.json`, asserted to agree with it.
 *
 * WHAT THIS EXISTS FOR. A milestone changes a shipped,
 * wire-visible value -- a status, a media type, a response member -- and a phase
 * under `src/phases/` goes on pinning the OLD value as a literal. Nothing fails
 * until somebody boots the image, because the phase only runs under a booted
 * run: `make os-apid-api-test`, forty minutes and a built image away. This file
 * closes the part of that gap that needs no boot, and says nothing about the
 * part that does.
 *
 * THERE ARE EXACTLY TWO COPIES OF EVERY VALUE, AND THIS ASSERTS THEY AGREE.
 * The phase file's literal is read out of the phase file's own bytes -- never
 * imported, never restated here -- and compared against what `openapi.json`
 * declares. A row carries no copy of the status or the media type: it carries
 * only WHERE to read the phase's copy (`anchor`) and WHICH declaration in the
 * document it must agree with (`path`, `method`, `status`). So:
 *
 *   - the document moves and the phase does not  -> red, naming both
 *   - the phase moves and the document does not  -> red, naming both
 *   - both move together                         -> green, correctly
 *   - the anchored assertion is renamed or deleted -> red, naming the anchor
 *
 * That last one is deliberate. `docs/verify-index.sh`'s header argues at length
 * that a check which silently matches nothing is worse than no check, because
 * it reports the same green either way. An anchor that finds 0 sites, or 2, is
 * a hard failure here rather than a skipped row.
 *
 * SCHEMA ROWS DO carry the member names they assert, which is a third copy --
 * and that is sound rather than sloppy, because the row's names must be found
 * IN THE PHASE'S OWN SOURCE SPAN before they are looked up in the document. A
 * stale name in this table can therefore only produce a false RED, never a
 * false green.
 *
 * WHAT IS NOT HERE. Every pin whose agreement cannot be asserted against a
 * committed artefact: the static SPA surface (`/`, `/ui`, `/healthz` --
 * `openapi.json` documents `/api/` and nothing else), the error
 * `code` VALUES (`ApiErrorDetail.code` is declared an open set with no enum, so
 * no artefact states which code a given route and status answers), response
 * HEADERS (`openapi.json` carries no `headers` member anywhere -- measured:
 * zero occurrences), the `/api/v1/state/...` bodies (declared `ResourceValue`,
 * i.e. any JSON), and everything timing-, console- or reboot-shaped. Those are
 * the section 4, not this file's silence.
 *
 * Runs with no network, no docker, no QEMU and no image:
 *
 *     bun run spec-pins          # from pkgs/mosd/tests/apid-api
 *     make os-apid-api-spec-pins # from the repository root, in the pinned bun
 */

const REPO_ROOT = new URL("../../../../../", import.meta.url);
const OPENAPI = new URL("pkgs/mosd/apid/openapi.json", REPO_ROOT);
const HARNESS_ROOT = new URL("../", import.meta.url);

/** Where the phase's copy of the value is read from. */
type Span = "call" | "line";

interface StatusPin {
  readonly kind: "status";
  readonly file: string;
  readonly anchor: string;
  readonly path: string;
  readonly method: string;
  readonly what: string;
}

interface MediaPin {
  readonly kind: "media";
  readonly file: string;
  readonly anchor: string;
  readonly path: string;
  readonly method: string;
  readonly status: string;
  readonly what: string;
}

interface SchemaPin {
  readonly kind: "schema";
  readonly file: string;
  readonly anchor: string;
  readonly span: Span;
  readonly schema: string;
  /** required: in `required`. present: in `properties`. optional: in
   *  `properties` and NOT in `required`. absent: in neither. exact: the schema's
   *  `required` AND `properties` are exactly these names and no others. */
  readonly mode: "required" | "present" | "optional" | "absent" | "exact";
  readonly names: readonly string[];
  readonly what: string;
}

type Pin = StatusPin | MediaPin | SchemaPin;

const BOUNDARY = "src/phases/01-spa-boundary.ts";
const SESSION = "src/phases/02-session.ts";
const MANAGEMENT = "src/phases/03-api-management.ts";
const NETWORK_PHASE = "src/phases/04-network-observation.ts";

const SESSION_PATH = "/api/v1/session";
const SETUP = "/api/v1/setup";
const SETTINGS = "/api/v1/settings/{path}";
const NETWORK = "/api/v1/network";
const UI = "/api/v1/ui";
const UI_ACTIVE = "/api/v1/ui/active";
const TASK = "/api/v1/tasks/{id}";
const VERSIONS = "/api/versions";

const PINS: readonly Pin[] = [
  // -- statuses: `paths.<path>.<method>.responses.<status>` must exist --------
  { kind: "status", file: BOUNDARY, anchor: "an unauthenticated GET /api/versions answers", method: "get", path: VERSIONS, what: "the discovery route" },
  { kind: "status", file: BOUNDARY, anchor: "GET /api/v1/session exposes bootstrap state", method: "get", path: SESSION_PATH, what: "the unauthenticated session bootstrap" },
  { kind: "status", file: SESSION, anchor: "POST /api/v1/setup rejects a password", method: "post", path: SETUP, what: "setup validation" },
  { kind: "status", file: SESSION, anchor: "POST /api/v1/setup configures the device", method: "post", path: SETUP, what: "first-run setup" },
  { kind: "status", file: SESSION, anchor: "DELETE /api/v1/session without CSRF", method: "delete", path: SESSION_PATH, what: "logout CSRF refusal" },
  { kind: "status", file: SESSION, anchor: "DELETE /api/v1/session with CSRF", method: "delete", path: SESSION_PATH, what: "logout" },
  { kind: "status", file: SESSION, anchor: "POST /api/v1/session logs in", method: "post", path: SESSION_PATH, what: "JSON login" },
  { kind: "status", file: MANAGEMENT, anchor: "GET /api/v1/ui reports UI selection", method: "get", path: UI, what: "UI status" },
  { kind: "status", file: MANAGEMENT, anchor: "PUT /api/v1/ui/active without CSRF", method: "put", path: UI_ACTIVE, what: "custom UI selector CSRF refusal" },
  { kind: "status", file: MANAGEMENT, anchor: "PUT /api/v1/ui/active reports no retained custom UI", method: "put", path: UI_ACTIVE, what: "unavailable retained custom UI" },
  { kind: "status", file: MANAGEMENT, anchor: "GET /api/v1/settings/hostname reads", method: "get", path: SETTINGS, what: "session settings read" },
  { kind: "status", file: MANAGEMENT, anchor: "cookie-authenticated settings PUT without CSRF", method: "put", path: SETTINGS, what: "settings CSRF refusal" },
  { kind: "status", file: MANAGEMENT, anchor: "same settings PUT with CSRF", method: "put", path: SETTINGS, what: "settings write" },
  { kind: "status", file: MANAGEMENT, anchor: "GET /api/v1/tasks/{id} exposes", method: "get", path: TASK, what: "task lookup" },
  { kind: "status", file: MANAGEMENT, anchor: "setup bearer reads the same management API", method: "get", path: SETTINGS, what: "bearer settings read" },
  { kind: "status", file: MANAGEMENT, anchor: "API management read with no credential", method: "get", path: UI, what: "anonymous management refusal" },
  { kind: "status", file: NETWORK_PHASE, anchor: "GET /api/v1/network returns configured", method: "get", path: NETWORK, what: "network overview" },

  // -- media types: the response declares `content.<media>` ------------------
  { kind: "media", file: BOUNDARY, anchor: "the unauthenticated /api/versions answer is typed", method: "get", path: VERSIONS, status: "200", what: "the discovery route's media type" },

  // -- response members: `components.schemas.<name>` -------------------------
  { kind: "schema", file: SESSION, anchor: 'const token = setupBody?.["token"]', span: "line", schema: "SetupToken", mode: "required", names: ["token"], what: "setup's bearer token" },
  { kind: "schema", file: SESSION, anchor: 'const csrfToken = setupBody?.["csrfToken"]', span: "line", schema: "SetupToken", mode: "required", names: ["csrfToken"], what: "setup's browser CSRF token" },
  { kind: "schema", file: SESSION, anchor: "login returns authenticated state", span: "call", schema: "SessionStatus", mode: "required", names: ["state"], what: "session state" },
  { kind: "schema", file: SESSION, anchor: 'const loginCsrf = loginBody?.["csrfToken"]', span: "line", schema: "SessionStatus", mode: "optional", names: ["csrfToken"], what: "authenticated session CSRF token" },
  { kind: "schema", file: MANAGEMENT, anchor: 'const taskId = acceptedBody?.["taskId"]', span: "line", schema: "TaskAccepted", mode: "required", names: ["taskId"], what: "accepted write task id" },
  { kind: "schema", file: MANAGEMENT, anchor: "factory UiStatus has no optional availableCustom candidate", span: "call", schema: "UiStatus", mode: "optional", names: ["availableCustom"], what: "retained custom UI candidate" },
  { kind: "schema", file: NETWORK_PHASE, anchor: "NetworkOverview carries", span: "call", schema: "NetworkOverview", mode: "required", names: ["configured", "configuredCount", "observed"], what: "network overview members" },
  { kind: "schema", file: NETWORK_PHASE, anchor: "observed network has a positive", span: "call", schema: "ObservedNetwork", mode: "required", names: ["interfaceCount", "interfaces"], what: "observed interface inventory" },
];

// -- reporting, in the register run.sh and the verification contract use ---------

let passes = 0;
let failures = 0;

function pass(what: string): void {
  passes += 1;
  console.log(`PASS: ${what}`);
}

function fail(what: string, detail: readonly string[]): void {
  failures += 1;
  console.log(`FAIL: ${what}`);
  for (const line of detail) console.log(`    ${line}`);
}

// -- reading the phase's own copy --------------------------------------------

const sources = new Map<string, string>();

async function sourceOf(file: string): Promise<string> {
  const held = sources.get(file);
  if (held !== undefined) return held;
  const text = await Bun.file(new URL(file, HARNESS_ROOT)).text();
  sources.set(file, text);
  return text;
}

function lineOf(source: string, index: number): number {
  return source.slice(0, index).split("\n").length;
}

/**
 * The single occurrence of `anchor`, or a sentence saying why there is not one.
 *
 * Zero and two are both errors: a renamed assertion and a duplicated one each
 * mean this row no longer names one place in the phase, and a row that quietly
 * matched nothing would print the same green as a row that matched and agreed.
 */
function locate(source: string, anchor: string): { at: number } | { why: string } {
  const first = source.indexOf(anchor);
  if (first === -1) {
    return {
      why: `no occurrence of the anchor -- the assertion it names was renamed or deleted, so this row reads nothing`,
    };
  }
  const second = source.indexOf(anchor, first + 1);
  if (second !== -1) {
    return {
      why: `${countOccurrences(source, anchor)} occurrences of the anchor (first at line ${lineOf(source, first)}, next at line ${lineOf(source, second)}) -- an anchor must name ONE site`,
    };
  }
  return { at: first };
}

function countOccurrences(source: string, needle: string): number {
  let count = 0;
  let at = source.indexOf(needle);
  while (at !== -1) {
    count += 1;
    at = source.indexOf(needle, at + 1);
  }
  return count;
}

/** The whole source line holding `at`. */
function lineSpan(source: string, at: number): string {
  const start = source.lastIndexOf("\n", at) + 1;
  const end = source.indexOf("\n", at);
  return source.slice(start, end === -1 ? source.length : end);
}

/**
 * The `report.<something>(..)` call holding `at`, parens balanced.
 *
 * Walks back to the nearest `report.` before the anchor -- every assertion in
 * this harness goes through the reporter, so that is the statement boundary --
 * and forward to the paren that closes it.
 */
function callSpan(source: string, at: number): string | undefined {
  const start = source.lastIndexOf("report.", at);
  if (start === -1) return undefined;
  const open = source.indexOf("(", start);
  if (open === -1 || open > at) return undefined;
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    const c = source[i];
    if (c === "(") depth += 1;
    else if (c === ")") {
      depth -= 1;
      if (depth === 0) return source.slice(start, i + 1);
    }
  }
  return undefined;
}

/**
 * Blank out every string, template and regex literal in a span.
 *
 * The status extractor below counts INTEGER LITERALS, and every one of these
 * assertions also spells its status out in the prose a red line prints -- "is
 * 401", "answers 200". Those are characters inside a string; masking is what
 * keeps them from being read as a second pinned value and turning every row
 * into "expected exactly one, found two".
 */
function maskLiterals(span: string): string {
  let out = "";
  let i = 0;
  while (i < span.length) {
    const c = span[i] ?? "";
    if (c === '"' || c === "'" || c === "`") {
      const quote = c;
      out += " ";
      i += 1;
      while (i < span.length) {
        const d = span[i] ?? "";
        if (d === "\\") {
          out += "  ";
          i += 2;
          continue;
        }
        if (d === quote) {
          out += " ";
          i += 1;
          break;
        }
        out += d === "\n" ? "\n" : " ";
        i += 1;
      }
      continue;
    }
    if (c === "/" && span[i + 1] === "^") {
      // A regex literal, which these assertions only ever open with `^`.
      out += " ";
      i += 1;
      while (i < span.length) {
        const d = span[i] ?? "";
        if (d === "\\") {
          out += "  ";
          i += 2;
          continue;
        }
        if (d === "/") {
          out += " ";
          i += 1;
          break;
        }
        out += d === "\n" ? "\n" : " ";
        i += 1;
      }
      continue;
    }
    out += c;
    i += 1;
  }
  return out;
}

/** Every integer literal in [100, 599] left after masking. */
function statusLiterals(span: string): number[] {
  const found: number[] = [];
  for (const match of maskLiterals(span).matchAll(/\b(\d{3})\b/g)) {
    const value = Number(match[1]);
    if (value >= 100 && value <= 599) found.push(value);
  }
  return found;
}

// -- reading the document's copy ---------------------------------------------

interface Document {
  readonly paths: Record<string, Record<string, { responses?: Record<string, { content?: Record<string, unknown> }> }>>;
  readonly components: { schemas: Record<string, { required?: string[]; properties?: Record<string, unknown> }> };
}

function operationOf(doc: Document, path: string, method: string): Record<string, { content?: Record<string, unknown> }> | string {
  const item = doc.paths[path];
  if (item === undefined) {
    return `openapi.json declares no path ${path} at all. Declared: ${Object.keys(doc.paths).join(", ")}`;
  }
  const operation = item[method];
  if (operation === undefined) {
    return `openapi.json declares ${path} but not the ${method.toUpperCase()} on it. Declared methods: ${Object.keys(item).join(", ").toUpperCase()}`;
  }
  return operation.responses ?? {};
}

// -- the checks ---------------------------------------------------------------

async function checkStatus(pin: StatusPin, doc: Document): Promise<void> {
  const what = `${pin.file}: ${pin.what} pins the status ${pin.method.toUpperCase()} ${pin.path} declares`;
  const source = await sourceOf(pin.file);
  const found = locate(source, pin.anchor);
  if ("why" in found) {
    fail(what, [`anchor:   ${JSON.stringify(pin.anchor)}`, `actual:   ${found.why}`]);
    return;
  }
  const line = lineOf(source, found.at);
  const span = callSpan(source, found.at);
  if (span === undefined) {
    fail(what, [
      `${pin.file}:${line}`,
      `actual:   the anchor is not inside a report.<..>(..) call, so there is no assertion to read a status out of`,
    ]);
    return;
  }
  const literals = statusLiterals(span);
  if (literals.length !== 1) {
    fail(what, [
      `${pin.file}:${line}`,
      `expected: exactly one HTTP status literal in the anchored assertion`,
      `actual:   ${literals.length === 0 ? "none" : literals.join(", ")}`,
      `span:     ${flatten(span)}`,
    ]);
    return;
  }
  const pinned = String(literals[0]);
  const responses = operationOf(doc, pin.path, pin.method);
  if (typeof responses === "string") {
    fail(what, [`${pin.file}:${line}`, `pinned:   ${pinned}`, `actual:   ${responses}`]);
    return;
  }
  if (!Object.hasOwn(responses, pinned)) {
    fail(what, [
      `${pin.file}:${line}`,
      `pinned:   ${pinned}   (the literal in the phase file, right now)`,
      `pointer:  /paths/${escapePointer(pin.path)}/${pin.method}/responses/${pinned}`,
      `declared: ${Object.keys(responses).join(", ")}   (what openapi.json says this operation answers)`,
      `read as:  the phase asserts a status openapi.json does not declare for this route.`,
      `          Either a milestone moved the shipped status and this phase still pins the`,
      `          old one, or the phase was pointed at the wrong route.`,
    ]);
    return;
  }
  pass(`${what} (${pinned})`);
}

async function checkMedia(pin: MediaPin, doc: Document): Promise<void> {
  const what = `${pin.file}: ${pin.what} pins a media type ${pin.method.toUpperCase()} ${pin.path} ${pin.status} declares`;
  const source = await sourceOf(pin.file);
  const found = locate(source, pin.anchor);
  if ("why" in found) {
    fail(what, [`anchor:   ${JSON.stringify(pin.anchor)}`, `actual:   ${found.why}`]);
    return;
  }
  const line = lineOf(source, found.at);
  const span = callSpan(source, found.at);
  if (span === undefined) {
    fail(what, [`${pin.file}:${line}`, `actual:   the anchor is not inside a report.<..>(..) call`]);
    return;
  }
  // `\/` in a regex literal is the same media type as `/` in a string; the
  // backslashes come out before the comparison rather than being spelled twice.
  const flat = span.replace(/\\/g, "");
  const responses = operationOf(doc, pin.path, pin.method);
  if (typeof responses === "string") {
    fail(what, [`${pin.file}:${line}`, `actual:   ${responses}`]);
    return;
  }
  const response = responses[pin.status];
  if (response === undefined) {
    fail(what, [
      `${pin.file}:${line}`,
      `expected: a ${pin.status} response on ${pin.method.toUpperCase()} ${pin.path}`,
      `declared: ${Object.keys(responses).join(", ")}`,
    ]);
    return;
  }
  const declared = Object.keys(response.content ?? {});
  const pinned = declared.filter((media) => flat.includes(media));
  if (pinned.length === 0) {
    fail(what, [
      `${pin.file}:${line}`,
      `expected: the anchored assertion to name one of the media types openapi.json declares`,
      `declared: ${declared.join(", ") || "(the response declares no content at all)"}`,
      `pointer:  /paths/${escapePointer(pin.path)}/${pin.method}/responses/${pin.status}/content`,
      `span:     ${flatten(span)}`,
      `read as:  the phase asserts a Content-Type this response is not documented to send.`,
    ]);
    return;
  }
  pass(`${what} (${pinned.join(", ")})`);
}

async function checkSchema(pin: SchemaPin, doc: Document): Promise<void> {
  const what = `${pin.file}: ${pin.what} agrees with components.schemas.${pin.schema}`;
  const source = await sourceOf(pin.file);
  const found = locate(source, pin.anchor);
  if ("why" in found) {
    fail(what, [`anchor:   ${JSON.stringify(pin.anchor)}`, `actual:   ${found.why}`]);
    return;
  }
  const line = lineOf(source, found.at);
  const span = pin.span === "line" ? lineSpan(source, found.at) : callSpan(source, found.at);
  if (span === undefined) {
    fail(what, [`${pin.file}:${line}`, `actual:   the anchor is not inside a report.<..>(..) call`]);
    return;
  }
  // The row's names are checked against the PHASE first. A name this table
  // claims but the phase does not spell is a stale row, and it goes red here
  // rather than being looked up in the document and passing on the strength of
  // this file alone.
  const unspelled = pin.names.filter((name) => !span.includes(`"${name}"`));
  if (unspelled.length > 0) {
    fail(what, [
      `${pin.file}:${line}`,
      `expected: the anchored source to spell ${pin.names.map((n) => JSON.stringify(n)).join(", ")}`,
      `actual:   it does not spell ${unspelled.map((n) => JSON.stringify(n)).join(", ")}`,
      `span:     ${flatten(span)}`,
      `read as:  this row is stale -- the phase renamed the member it pins.`,
    ]);
    return;
  }

  const schema = doc.components.schemas[pin.schema];
  if (schema === undefined) {
    fail(what, [
      `${pin.file}:${line}`,
      `actual:   openapi.json declares no schema named ${pin.schema}. Declared: ${Object.keys(doc.components.schemas).join(", ")}`,
    ]);
    return;
  }
  const required = schema.required ?? [];
  const properties = Object.keys(schema.properties ?? {});
  const pointer = `/components/schemas/${pin.schema}`;

  const wrong: string[] = [];
  for (const name of pin.names) {
    if (pin.mode === "required" && !required.includes(name)) wrong.push(`${name} is not in required`);
    if (pin.mode === "present" && !properties.includes(name)) wrong.push(`${name} is not a property`);
    if (pin.mode === "optional" && !properties.includes(name)) wrong.push(`${name} is not a property`);
    if (pin.mode === "optional" && required.includes(name)) wrong.push(`${name} IS in required, so the phase's "absent here" assertion contradicts the document`);
    if (pin.mode === "absent" && properties.includes(name)) wrong.push(`${name} IS a property, and the phase asserts no response ever carries it`);
  }
  if (pin.mode === "exact") {
    const names = [...pin.names].sort().join(", ");
    if ([...required].sort().join(", ") !== names) wrong.push(`required is [${required.join(", ")}], not [${names}]`);
    if ([...properties].sort().join(", ") !== names) wrong.push(`properties are [${properties.join(", ")}], not [${names}]`);
  }
  if (wrong.length > 0) {
    fail(what, [
      `${pin.file}:${line}`,
      `pinned:   ${pin.names.map((n) => JSON.stringify(n)).join(", ")} (${pin.mode})`,
      `pointer:  ${pointer}`,
      `declared: required = [${required.join(", ")}]; properties = [${properties.join(", ")}]`,
      ...wrong.map((line) => `actual:   ${line}`),
      `read as:  the phase asserts a body shape openapi.json no longer declares.`,
    ]);
    return;
  }
  pass(`${what} (${pin.mode}: ${pin.names.join(", ")})`);
}

function escapePointer(path: string): string {
  return path.replace(/~/g, "~0").replace(/\//g, "~1");
}

function flatten(text: string): string {
  const one = text.replace(/\s+/g, " ").trim();
  return one.length <= 200 ? one : `${one.slice(0, 200)}...`;
}

// -- the run ------------------------------------------------------------------

async function main(): Promise<void> {
  console.log(`note: reading ${OPENAPI.pathname}`);
  let doc: Document;
  try {
    doc = JSON.parse(await Bun.file(OPENAPI).text()) as Document;
  } catch (error) {
    console.log("FAIL: the committed OpenAPI document is readable JSON");
    console.log(`    ${OPENAPI.pathname}: ${error instanceof Error ? error.message : String(error)}`);
    console.log("RESULT: FAIL (0/1 checks)");
    process.exitCode = 1;
    return;
  }

  for (const pin of PINS) {
    if (pin.kind === "status") await checkStatus(pin, doc);
    else if (pin.kind === "media") await checkMedia(pin, doc);
    else await checkSchema(pin, doc);
  }

  const total = passes + failures;
  // Zero checks is a failure, for the reason run.sh gives about its own totals:
  // "no FAIL lines" is equally true of a run in which nothing executed at all.
  if (total === 0) {
    console.log("FAIL: the pin table is not empty");
    console.log("    PINS carries no rows, so this run asserted nothing and must not read as success.");
    console.log("RESULT: FAIL (0/1 checks)");
    process.exitCode = 1;
    return;
  }
  console.log(`RESULT: ${failures === 0 ? "PASS" : "FAIL"} (${passes}/${total} checks)`);
  if (failures > 0) process.exitCode = 1;
}

await main();
