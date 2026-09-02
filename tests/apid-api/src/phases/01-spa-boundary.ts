/** The public route boundary before the device has been configured. */

import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";

const HTML = /^text\/html(?:;.*)?$/i;
const JAVASCRIPT = /^(?:text|application)\/javascript(?:;.*)?$/i;

function json(body: string): JsonValue | undefined {
  try {
    return JSON.parse(body) as JsonValue;
  } catch {
    return undefined;
  }
}

const phase: Phase = {
  id: "01-spa-boundary",
  title: "the built-in SPA and JSON API own separate route trees",
  assumes: "a factory-fresh guest is answering HTTPS and has no active custom UI bundle",

  async run({ client, report }: PhaseContext): Promise<void> {
    const redirected = await client.http("/_ui/network?source=http");
    report.expectStatus(redirected, 308, "plain HTTP is redirected permanently to HTTPS");
    report.expectHeaderMatches(
      redirected,
      "location",
      /^https:\/\/[^/]+\/_ui\/network\?source=http$/,
      "the HTTP redirect preserves the SPA path and query",
    );

    const health = await client.get("/healthz");
    report.expectStatus(health, 200, "GET /healthz answers the listener health probe");
    report.expectBodyContains(health, "ok", "the listener health probe has its fixed body");

    const root = await client.get("/");
    report.expectStatus(root, 303, "GET / enters the built-in UI when no custom bundle is active");
    report.expectHeader(root, "location", "/_ui/", "the default root target is /_ui/");

    // The two paths the router DECLARES: `/_ui/` is its own route and
    // `/_ui/<anything>` is the `{*path}` route the SPA's client-side routing
    // needs. `/_ui` without the trailing slash is not asserted because it is
    // not one of them -- pinning a spelling the router does not declare is how
    // this phase came to pin `/ui` for a build that had moved.
    let indexBody = "";
    for (const path of ["/_ui/", "/_ui/network"] as const) {
      const response = await client.get(path);
      report.expectStatus(response, 200, `GET ${path} serves the built-in SPA`);
      report.expectHeaderMatches(
        response,
        "content-type",
        HTML,
        `GET ${path} is HTML rather than a management response`,
      );
      report.expectBodyContains(response, "<title>mos console</title>", `GET ${path} serves the shipped SPA index`);
      if (path === "/_ui/") indexBody = response.body;
    }

    // The entry chunk is read OUT OF THE SERVED INDEX rather than pinned by
    // name. Its filename carries a content hash, so any literal here is a
    // literal that goes stale on the next UI build and reports a 404 as
    // "the SPA does not bootstrap" -- which is the shape of the failure this
    // phase had. Following the index is also what a browser does.
    const entry = /<script[^>]+src="(\/_ui\/assets\/[^"]+\.js)"/.exec(indexBody)?.[1];
    if (entry === undefined) {
      report.fail(
        "the served SPA index names its entry module",
        `no <script src="/_ui/assets/....js"> in the index served at /_ui/ (${indexBody.length} bytes)`,
      );
    } else {
      const app = await client.get(entry);
      report.expectStatus(app, 200, `GET ${entry} serves the embedded application`);
      report.expectHeaderMatches(app, "content-type", JAVASCRIPT, "the embedded application is JavaScript");
      report.expectBodyContains(
        app,
        "/api/v1/session",
        "the shipped SPA bootstraps through the root-relative session API",
      );
    }

    const versions = await client.get("/api/versions", { sendCookies: false });
    report.expectStatus(versions, 200, "an unauthenticated GET /api/versions answers 200");
    report.expectHeaderMatches(
      versions,
      "content-type",
      /^application\/json(?:;.*)?$/i,
      "the unauthenticated /api/versions answer is typed application/json",
    );

    const session = await client.get("/api/v1/session", { sendCookies: false });
    report.expectStatus(session, 200, "GET /api/v1/session exposes bootstrap state without a credential");
    report.check(
      (json(session.body) as Record<string, JsonValue> | undefined)?.["state"] === "setup",
      "the initial session state is setup",
      `actual body: ${session.body}`,
    );

    const missing = await client.get("/api/v1/not-a-route", { sendCookies: false });
    report.expectStatus(missing, 404, "an unknown API path has the API's JSON 404");
    report.expectJson(
      missing,
      { error: { code: "not_found", source: "apid" } },
      "the unknown API path uses the standard error envelope",
      { subset: true },
    );

    for (const path of [
      "/containers/enable",
      "/mqtt/enable",
      "/setup",
      "/login",
      "/logout",
      "/hostname",
      "/network",
      "/ssh/enable",
      "/power/reboot",
      "/builtin/tokens",
    ] as const) {
      const response = await client.post(path, { enabled: "on" });
      report.expectStatus(response, 405, `POST ${path} is not a management route`);
    }
  },
};

export default phase;
