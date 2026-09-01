/**
 * A deliberately small HTTP client that behaves like a browser -- and, where a
 * browser's behaviour is the thing under test, models it explicitly instead of
 * delegating to a library that would paper over it.
 *
 * This file has no dependencies for a reason. A conventional test client
 * follows redirects, manages cookies invisibly and normalises request targets.
 * Those three behaviours are exactly what this suite exists to observe: the
 * :80 -> :443 redirect must be asserted and never followed, the session
 * cookie attributes and the API's explicit CSRF header are security contracts, and
 * a traversal probe that the client rewrites before it leaves the process is a
 * test of the client, not of apid.
 */

import * as net from "node:net";
import * as tls from "node:tls";
import type { PeerCertificate } from "node:tls";
import type { Config } from "./config.ts";

const DEFAULT_TIMEOUT_MS = 15_000;

// headers

/** A case-insensitive, repeat-preserving view over one response's headers. */
export class ResponseHeaders {
  readonly #byLower = new Map<string, string[]>();

  constructor(entries: Iterable<readonly [string, string]>) {
    for (const [name, value] of entries) {
      const key = name.toLowerCase();
      const bucket = this.#byLower.get(key);
      if (bucket === undefined) this.#byLower.set(key, [value]);
      else bucket.push(value);
    }
  }

  /** The first value sent under `name`, or undefined if it was not sent. */
  get(name: string): string | undefined {
    return this.#byLower.get(name.toLowerCase())?.[0];
  }

  /** Every value sent under `name`, in wire order. */
  all(name: string): readonly string[] {
    return this.#byLower.get(name.toLowerCase()) ?? [];
  }

  has(name: string): boolean {
    return this.#byLower.has(name.toLowerCase());
  }

  /** Lowercased names, in first-seen order. Used to name what WAS sent when an
   *  expected header is absent -- "absent" alone is a useless failure line. */
  names(): readonly string[] {
    return [...this.#byLower.keys()];
  }

  toJSON(): Record<string, string[]> {
    return Object.fromEntries([...this.#byLower].map(([k, v]) => [k, [...v]]));
  }
}

// cookies

export interface ParsedCookie {
  /** The whole `Set-Cookie` line as it arrived, for attribute assertions. */
  readonly raw: string;
  readonly name: string;
  readonly value: string;
  /** Attribute name (lowercased) -> value; "" for valueless flags. */
  readonly attributes: ReadonlyMap<string, string>;
}

/** Split one `Set-Cookie` line. Returns undefined if it carries no `name=`. */
export function parseSetCookie(raw: string): ParsedCookie | undefined {
  const parts = raw.split(";");
  const first = parts[0];
  if (first === undefined) return undefined;
  const eq = first.indexOf("=");
  if (eq < 0) return undefined;
  const name = first.slice(0, eq).trim();
  if (name === "") return undefined;

  const attributes = new Map<string, string>();
  for (const part of parts.slice(1)) {
    const trimmed = part.trim();
    if (trimmed === "") continue;
    const split = trimmed.indexOf("=");
    if (split < 0) attributes.set(trimmed.toLowerCase(), "");
    else {
      attributes.set(
        trimmed.slice(0, split).trim().toLowerCase(),
        trimmed.slice(split + 1).trim(),
      );
    }
  }

  return { raw, name, value: first.slice(eq + 1).trim(), attributes };
}

/**
 * A cookie jar that honours deletion.
 *
 * apid clears the session with the same cookie carrying `Max-Age=0`
 * (pkgs/mosd/apid/src/session.rs, `clear_cookie`). A jar that merely overwrote the
 * value would leave the name present and make the logout assertion pass while
 * asserting nothing -- so `Max-Age` <= 0 removes the entry outright. `Expires`
 * in the past is honoured too; apid does not use it, but a browser does and
 * this jar claims to behave like one.
 */
export class CookieJar {
  readonly #cookies = new Map<string, ParsedCookie>();
  #lastSetCookie: readonly string[] = [];

  /** Absorb every `Set-Cookie` line off one response. */
  ingest(lines: readonly string[]): void {
    if (lines.length > 0) this.#lastSetCookie = [...lines];
    for (const line of lines) {
      const cookie = parseSetCookie(line);
      if (cookie === undefined) continue;
      if (isDeletion(cookie)) this.#cookies.delete(cookie.name);
      else this.#cookies.set(cookie.name, cookie);
    }
  }

  get(name: string): ParsedCookie | undefined {
    return this.#cookies.get(name);
  }

  clear(): void {
    this.#cookies.clear();
  }

  names(): readonly string[] {
    return [...this.#cookies.keys()];
  }

  /** The `Cookie:` request header value, or undefined when the jar is empty. */
  header(): string | undefined {
    if (this.#cookies.size === 0) return undefined;
    return [...this.#cookies.values()].map((c) => `${c.name}=${c.value}`).join("; ");
  }

  /** The raw `Set-Cookie` lines off the most recent response that carried any. */
  lastSetCookie(): readonly string[] {
    return this.#lastSetCookie;
  }
}

function isDeletion(cookie: ParsedCookie): boolean {
  const maxAge = cookie.attributes.get("max-age");
  if (maxAge !== undefined) {
    const seconds = Number(maxAge);
    if (Number.isFinite(seconds) && seconds <= 0) return true;
  }
  const expires = cookie.attributes.get("expires");
  if (expires !== undefined) {
    const when = Date.parse(expires);
    if (Number.isFinite(when) && when <= Date.now()) return true;
  }
  return false;
}

// errors

/**
 * `follow()` was asked to follow a redirect that leaves the authority the
 * request was made to.
 *
 * This is not defensive style. apid's :80 -> :443 redirect is a 308 whose
 * Location names the GUEST's port 443 (measured 2026-08-24). Through a port
 * forward that authority is not reachable from here, so a client that follows
 * it hangs until timeout and the failure reads as "apid is down". Refusing
 * loudly turns a hung run into one line.
 */
export class CrossAuthorityRedirectError extends Error {
  constructor(
    readonly requestedAuthority: string,
    readonly locationAuthority: string,
    readonly location: string,
  ) {
    super(
      `refusing to follow a redirect off the requested authority: requested ` +
        `${requestedAuthority}, Location names ${locationAuthority} (${location}). ` +
        `apid's :80 -> :443 redirect names the GUEST's port 443, which is not ` +
        `reachable through the port forward; following it hangs.`,
    );
    this.name = "CrossAuthorityRedirectError";
  }
}

/** `follow()` was handed a response with no `Location` to follow. */
export class RedirectWithoutLocationError extends Error {
  constructor(readonly status: number) {
    super(`cannot follow a ${status} response: it carries no Location header`);
    this.name = "RedirectWithoutLocationError";
  }
}

/** A TLS request was aimed at a host other than the configured test host. */
export class ForeignHostError extends Error {
  constructor(readonly attempted: string, readonly configured: string) {
    super(
      `refusing to relax certificate verification for ${attempted}: this suite ` +
        `only ever talks to ${configured}`,
    );
    this.name = "ForeignHostError";
  }
}

// responses and form fields

export type Scheme = "https" | "http";

export interface HttpResponse {
  readonly method: string;
  /** Exactly what was put on the wire after the method. */
  readonly requestTarget: string;
  /** The absolute URL of the request, used by `follow()` to compare authority. */
  readonly url: string;
  readonly status: number;
  readonly statusText: string;
  readonly headers: ResponseHeaders;
  /** Every `Set-Cookie` line, unfolded -- `Headers.get` would comma-join them. */
  readonly setCookie: readonly string[];
  readonly body: string;
  readonly transport: "fetch" | "socket";
}

/**
 * An HTML checkbox that is not ticked sends NOTHING. It does not send "off",
 * it does not send an empty string: the field is simply absent from the body.
 * apid reads its checkboxes as `Option<String>` on exactly that assumption, so
 * a phase that posts `dhcp=""` to mean "unchecked" is posting a ticked box
 * with an empty value and is testing something else entirely.
 */
export const UNCHECKED: unique symbol = Symbol("unchecked");

export type FormValue = string | typeof UNCHECKED;
export type FormFields = Readonly<Record<string, FormValue>>;

/** Express a checkbox honestly: ticked sends `value`, unticked sends nothing. */
export function checkbox(ticked: boolean, value = "on"): FormValue {
  return ticked ? value : UNCHECKED;
}

export function encodeForm(fields: FormFields): string {
  const params = new URLSearchParams();
  for (const [name, value] of Object.entries(fields)) {
    if (value === UNCHECKED) continue;
    params.append(name, value);
  }
  return params.toString();
}

export interface RequestOptions {
  readonly scheme?: Scheme;
  readonly headers?: Readonly<Record<string, string>>;
  readonly body?: string;
  readonly contentType?: string;
  readonly timeoutMs?: number;
  /** Default true. Set false to prove a route rejects an unauthenticated caller. */
  readonly sendCookies?: boolean;
}

export interface RawOptions extends RequestOptions {
  readonly method?: string;
  /** Override the `Host:` header. Absent means host[:port] of the target. */
  readonly hostHeader?: string;
}

export interface CertificateInfo {
  readonly authorized: boolean;
  readonly authorizationError: string | undefined;
  readonly subject: PeerCertificate["subject"] | undefined;
  readonly issuer: PeerCertificate["issuer"] | undefined;
  readonly valid_from: string | undefined;
  readonly valid_to: string | undefined;
  readonly raw: PeerCertificate | undefined;
}

// SNI

/**
 * The SNI server name to send for `host`, or undefined when there is none.
 *
 * SNI carries a `host_name` and never an IP literal (RFC 6066 section 3), and
 * bun -- like node, so this is not a bun quirk -- enforces that by throwing at
 * `tls.connect()` synchronously, before a socket is opened. Measured under bun
 * 1.4.0 in `oven/bun:1`: `TypeError [ERR_INVALID_ARG_VALUE]: The property
 * 'options.servername' Setting the TLS ServerName to an IP address is not
 * permitted.` for both `127.0.0.1` and `::1`; a hostname, or the property
 * omitted, does not throw. `config.host` is always an IP literal here, so
 * sending it as `servername` makes `inspectCertificate()` and every raw HTTPS
 * write throw at call time. Dropping it costs nothing: verification is already
 * relaxed for `config.host` (see `#tlsOptionsFor`), and apid serves one
 * self-signed certificate with no name-based virtual hosting. `net.isIP` returns
 * 0 for a non-IP-literal.
 */
export function sniServerName(host: string): string | undefined {
  return net.isIP(host) === 0 ? host : undefined;
}

// the client

export class Client {
  readonly jar = new CookieJar();
  readonly #config: Config;
  #forceFreshNext = false;

  constructor(config: Config) {
    this.#config = config;
  }

  get config(): Config {
    return this.#config;
  }

  portFor(scheme: Scheme): number {
    return scheme === "https" ? this.#config.httpsPort : this.#config.httpPort;
  }

  origin(scheme: Scheme = "https"): string {
    return `${scheme}://${this.#config.host}:${this.portFor(scheme)}`;
  }

  /**
   * Guarantee the NEXT request opens a fresh TCP connection.
   *
   * `fetch` pools connections, so a backoff window measured over one kept-alive
   * socket measures apid's handler and not its accept path. When this flag is
   * set the next request is issued over the raw socket transport, which always
   * dials and always sends `Connection: close`.
   */
  freshConnection(): void {
    this.#forceFreshNext = true;
  }

  async get(path: string, options: RequestOptions = {}): Promise<HttpResponse> {
    return this.request("GET", path, options);
  }

  /** A form-encoded POST, the way a browser submits apid's forms. */
  async post(
    path: string,
    fields: FormFields,
    options: RequestOptions = {},
  ): Promise<HttpResponse> {
    return this.request("POST", path, {
      ...options,
      body: encodeForm(fields),
      contentType: "application/x-www-form-urlencoded",
    });
  }

  /** A request against the :80 listener. Never follows; see `follow`. */
  async http(path: string, options: RequestOptions = {}): Promise<HttpResponse> {
    return this.request("GET", path, { ...options, scheme: "http" });
  }

  async request(
    method: string,
    path: string,
    options: RequestOptions = {},
  ): Promise<HttpResponse> {
    const scheme = options.scheme ?? "https";
    const headers = this.#buildHeaders(options);
    const fresh = this.#forceFreshNext;
    this.#forceFreshNext = false;

    const response = fresh
      ? await this.#socket(method, path, scheme, headers, options)
      : await this.#fetch(method, path, scheme, headers, options);
    this.jar.ingest(response.setCookie);
    return response;
  }

  /**
   * Put `requestTarget` on the wire VERBATIM: no URL object, no percent-decode,
   * no dot-segment collapse.
   *
   * `fetch()` normalises the target before it leaves the process -- measured
   * 2026-08-24: `/%2e%2e%2fetc%2fpasswd` and `/../../etc/passwd` are both
   * rewritten. Every assertion about the SHAPE of a path must come through
   * here, or it is an assertion about what the client sent, not about what
   * apid did with it.
   */
  async raw(requestTarget: string, options: RawOptions = {}): Promise<HttpResponse> {
    const scheme = options.scheme ?? "https";
    const headers = this.#buildHeaders(options);
    this.#forceFreshNext = false;
    const response = await this.#socket(
      options.method ?? "GET",
      requestTarget,
      scheme,
      headers,
      options,
      options.hostHeader,
    );
    this.jar.ingest(response.setCookie);
    return response;
  }

  /**
   * Follow one redirect, explicitly.
   *
   * Every request this client makes is issued with `redirect: "manual"`, so
   * following is never something that happens by accident. This refuses any
   * Location that names a different authority -- see CrossAuthorityRedirectError.
   */
  async follow(response: HttpResponse, options: RequestOptions = {}): Promise<HttpResponse> {
    const location = response.headers.get("location");
    if (location === undefined) throw new RedirectWithoutLocationError(response.status);

    const from = new URL(response.url);
    const to = new URL(location, from);
    if (authorityOf(to) !== authorityOf(from)) {
      throw new CrossAuthorityRedirectError(authorityOf(from), authorityOf(to), location);
    }

    const scheme: Scheme = to.protocol === "http:" ? "http" : "https";
    return this.request("GET", to.pathname + to.search, { ...options, scheme });
  }

  /**
   * Open a TLS connection to the HTTPS port and report what the peer presented.
   *
   * Measured 2026-08-24 against apid's self-signed certificate: `authorized`
   * is false and `authorizationError` is 'DEPTH_ZERO_SELF_SIGNED_CERT'.
   */
  async inspectCertificate(timeoutMs = DEFAULT_TIMEOUT_MS): Promise<CertificateInfo> {
    const host = this.#config.host;
    const port = this.#config.httpsPort;
    // Spread, not `servername: undefined`: the property must be ABSENT. See
    // sniServerName -- an IP literal here throws before a socket is opened.
    const servername = sniServerName(host);
    return new Promise<CertificateInfo>((resolve, reject) => {
      let settled = false;
      const socket = tls.connect({
        host,
        port,
        ...(servername === undefined ? {} : { servername }),
        // The connection must COMPLETE so the certificate can be read, hence
        // rejectUnauthorized: false. `authorized`/`authorizationError` are
        // still populated by the handshake, which is the whole point.
        ...this.#tlsOptionsFor(host),
      });
      socket.setTimeout(timeoutMs);
      const finish = (fn: () => void): void => {
        if (settled) return;
        settled = true;
        socket.destroy();
        fn();
      };
      socket.once("secureConnect", () => {
        const cert = socket.getPeerCertificate();
        const present = cert !== null && typeof cert === "object" && "subject" in cert;
        const rawError: unknown = socket.authorizationError;
        finish(() =>
          resolve({
            authorized: socket.authorized,
            authorizationError: describeAuthorizationError(rawError),
            subject: present ? cert.subject : undefined,
            issuer: present ? cert.issuer : undefined,
            valid_from: present ? cert.valid_from : undefined,
            valid_to: present ? cert.valid_to : undefined,
            raw: present ? cert : undefined,
          }),
        );
      });
      socket.once("timeout", () =>
        finish(() => reject(new Error(`TLS handshake with ${host}:${port} timed out after ${timeoutMs}ms`))),
      );
      socket.once("error", (error: Error) => finish(() => reject(error)));
    });
  }

  // -- internals ------------------------------------------------------------

  #buildHeaders(options: RequestOptions): Record<string, string> {
    const headers: Record<string, string> = { Accept: "*/*", ...options.headers };
    if (options.sendCookies !== false) {
      const cookie = this.jar.header();
      if (cookie !== undefined) headers["Cookie"] = cookie;
    }
    if (options.contentType !== undefined) headers["Content-Type"] = options.contentType;
    return headers;
  }

  /**
   * TLS options for one request, scoped to the configured test host.
   *
   * NOT `NODE_TLS_REJECT_UNAUTHORIZED`: that variable is process-global, so it
   * would disable verification for every host this process ever talks to,
   * which is not "accept the device's self-signed certificate". Verification
   * is relaxed here for `config.host` alone; any other host keeps it, and
   * `#assertTestHost` makes reaching a different host an error rather than a
   * silently-unverified connection.
   */
  #tlsOptionsFor(host: string): { rejectUnauthorized: boolean } {
    return { rejectUnauthorized: host !== this.#config.host };
  }

  #assertTestHost(host: string): void {
    if (host !== this.#config.host) throw new ForeignHostError(host, this.#config.host);
  }

  async #fetch(
    method: string,
    path: string,
    scheme: Scheme,
    headers: Record<string, string>,
    options: RequestOptions,
  ): Promise<HttpResponse> {
    const host = this.#config.host;
    if (scheme === "https") this.#assertTestHost(host);
    const url = `${this.origin(scheme)}${path}`;

    // Assembled as a variable, not an inline literal: `tls` is bun's own fetch
    // extension and TypeScript's excess-property check only fires on literals.
    const init = {
      method,
      headers,
      body: options.body,
      // Never, ever automatic. See `follow`.
      redirect: "manual" as const,
      signal: AbortSignal.timeout(options.timeoutMs ?? DEFAULT_TIMEOUT_MS),
      tls: this.#tlsOptionsFor(host),
    };
    const response = await fetch(url, init);
    const setCookie = setCookiesFrom(response.headers);
    const entries: Array<readonly [string, string]> = [];
    for (const [name, value] of response.headers.entries()) {
      if (name.toLowerCase() === "set-cookie") continue;
      entries.push([name, value]);
    }
    for (const line of setCookie) entries.push(["set-cookie", line]);

    return {
      method,
      requestTarget: path,
      url,
      status: response.status,
      statusText: response.statusText,
      headers: new ResponseHeaders(entries),
      setCookie,
      body: await response.text(),
      transport: "fetch",
    };
  }

  async #socket(
    method: string,
    target: string,
    scheme: Scheme,
    headers: Record<string, string>,
    options: RequestOptions,
    hostHeader?: string,
  ): Promise<HttpResponse> {
    const host = this.#config.host;
    const port = this.portFor(scheme);
    if (scheme === "https") this.#assertTestHost(host);
    const raw = await socketRequest({
      host,
      port,
      secure: scheme === "https",
      tlsOptions: this.#tlsOptionsFor(host),
      method,
      target,
      hostHeader: hostHeader ?? defaultHostHeader(scheme, host, port),
      headers,
      body: options.body,
      timeoutMs: options.timeoutMs ?? DEFAULT_TIMEOUT_MS,
    });
    return {
      method,
      requestTarget: target,
      url: `${this.origin(scheme)}${target.startsWith("/") ? target : `/${target}`}`,
      status: raw.status,
      statusText: raw.statusText,
      headers: raw.headers,
      setCookie: raw.setCookie,
      body: raw.body,
      transport: "socket",
    };
  }
}

// low-level socket transport

interface SocketRequestInit {
  readonly host: string;
  readonly port: number;
  readonly secure: boolean;
  readonly tlsOptions: { rejectUnauthorized: boolean };
  readonly method: string;
  readonly target: string;
  readonly hostHeader: string;
  readonly headers: Readonly<Record<string, string>>;
  readonly body: string | undefined;
  readonly timeoutMs: number;
}

interface SocketResponse {
  readonly status: number;
  readonly statusText: string;
  readonly headers: ResponseHeaders;
  readonly setCookie: readonly string[];
  readonly body: string;
}

/**
 * One request, one fresh socket, read to EOF.
 *
 * `Connection: close` is unconditional: the response is delimited by the close,
 * which removes any dependence on Content-Length being present and makes every
 * call here a genuine new connection for the backoff phase to measure.
 */
export async function socketRequest(init: SocketRequestInit): Promise<SocketResponse> {
  const lines = [`${init.method} ${init.target} HTTP/1.1`, `Host: ${init.hostHeader}`];
  for (const [name, value] of Object.entries(init.headers)) {
    if (name.toLowerCase() === "host" || name.toLowerCase() === "connection") continue;
    lines.push(`${name}: ${value}`);
  }
  const bodyBytes = init.body === undefined ? undefined : Buffer.from(init.body, "utf8");
  if (bodyBytes !== undefined) lines.push(`Content-Length: ${bodyBytes.length}`);
  lines.push("Connection: close");
  const head = Buffer.from(`${lines.join("\r\n")}\r\n\r\n`, "latin1");

  const chunks = await new Promise<Buffer[]>((resolve, reject) => {
    const collected: Buffer[] = [];
    let settled = false;
    const done = (fn: () => void): void => {
      if (settled) return;
      settled = true;
      fn();
    };

    const socket = init.secure
      ? tls.connect({
          host: init.host,
          port: init.port,
          // Absent, not undefined, when the host is an IP -- see sniServerName.
          ...(sniServerName(init.host) === undefined ? {} : { servername: init.host }),
          ...init.tlsOptions,
        })
      : net.connect({ host: init.host, port: init.port });

    socket.setTimeout(init.timeoutMs);
    socket.once("timeout", () => {
      socket.destroy();
      done(() =>
        reject(
          new Error(
            `${init.method} ${init.target} to ${init.host}:${init.port} timed out after ${init.timeoutMs}ms`,
          ),
        ),
      );
    });
    socket.once("error", (error: Error) => done(() => reject(error)));
    socket.on("data", (chunk: Buffer) => collected.push(chunk));
    socket.once("close", () => done(() => resolve(collected)));

    const onReady = (): void => {
      socket.write(head);
      if (bodyBytes !== undefined) socket.write(bodyBytes);
    };
    if (init.secure) socket.once("secureConnect", onReady);
    else socket.once("connect", onReady);
  });

  return parseHttpResponse(Buffer.concat(chunks));
}

export function parseHttpResponse(buffer: Buffer): SocketResponse {
  const separator = buffer.indexOf("\r\n\r\n");
  if (separator < 0) {
    throw new Error(
      `the peer closed without a complete HTTP head (${buffer.length} bytes): ` +
        JSON.stringify(buffer.subarray(0, 200).toString("latin1")),
    );
  }
  const headText = buffer.subarray(0, separator).toString("latin1");
  const headLines = headText.split("\r\n");
  const statusLine = headLines[0] ?? "";
  const match = /^HTTP\/\d(?:\.\d)? (\d{3})(?: (.*))?$/.exec(statusLine);
  if (match === null) {
    throw new Error(`not an HTTP status line: ${JSON.stringify(statusLine)}`);
  }
  const status = Number(match[1]);
  const statusText = match[2] ?? "";

  const entries: Array<readonly [string, string]> = [];
  const setCookie: string[] = [];
  for (const line of headLines.slice(1)) {
    const colon = line.indexOf(":");
    if (colon < 0) continue;
    const name = line.slice(0, colon).trim();
    const value = line.slice(colon + 1).trim();
    entries.push([name, value]);
    if (name.toLowerCase() === "set-cookie") setCookie.push(value);
  }
  const headers = new ResponseHeaders(entries);

  let body = buffer.subarray(separator + 4);
  // hyper streams responses of unknown length chunked even with Connection:
  // close, so a suite that skipped this would assert on hex length prefixes.
  if ((headers.get("transfer-encoding") ?? "").toLowerCase().includes("chunked")) {
    body = dechunk(body);
  }

  return { status, statusText, headers, setCookie, body: body.toString("utf8") };
}

function dechunk(buffer: Buffer): Buffer {
  const out: Buffer[] = [];
  let offset = 0;
  for (;;) {
    const crlf = buffer.indexOf("\r\n", offset);
    if (crlf < 0) break;
    const sizeField = buffer.subarray(offset, crlf).toString("latin1").split(";")[0] ?? "";
    const size = Number.parseInt(sizeField.trim(), 16);
    if (!Number.isFinite(size) || size <= 0) break;
    const start = crlf + 2;
    out.push(buffer.subarray(start, start + size));
    offset = start + size + 2;
  }
  return Buffer.concat(out);
}

// helpers

/** scheme://host[:port], with the scheme's default port elided as a URL does. */
export function authorityOf(url: URL): string {
  return `${url.protocol}//${url.host}`;
}

function defaultHostHeader(scheme: Scheme, host: string, port: number): string {
  const isDefault = (scheme === "https" && port === 443) || (scheme === "http" && port === 80);
  return isDefault ? host : `${host}:${port}`;
}

function setCookiesFrom(headers: Headers): string[] {
  // `Headers.get("set-cookie")` comma-joins repeats, which corrupts a cookie
  // carrying an Expires date. getSetCookie() is the unfolded accessor.
  const accessor = (headers as { getSetCookie?: () => string[] }).getSetCookie;
  if (typeof accessor === "function") return accessor.call(headers);
  const single = headers.get("set-cookie");
  return single === null ? [] : [single];
}

function describeAuthorizationError(value: unknown): string | undefined {
  if (value === null || value === undefined) return undefined;
  if (typeof value === "string") return value;
  // Node types this as Error, but in practice it is the OpenSSL reason code.
  if (value instanceof Error) return value.message;
  return String(value);
}
