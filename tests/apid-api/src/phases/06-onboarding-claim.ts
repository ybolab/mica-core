/**
 * Phase 06 -- the claim, and the provisioning record of a device no document
 * ever reached.
 *
 * WHY THIS IS HERE AND NOT A RUST TEST. `pkgs/mosd/apid/src/tests/claim.rs`
 * and `.../provisioning_api.rs` drive these routes against a fake settings
 * backend, which is the right place to enumerate their branches. What they
 * cannot say is that a REAL device, booted from the image this tree assembles,
 * carries the record that `POST /api/v1/setup` wrote and reads it back through
 * mosd's own store. That is what one boot buys: `via: "setup"` here is the
 * claim record surviving a D-Bus round trip and a TOML save, not a value a
 * fake handed back.
 *
 * WHAT THE FACTORY GUEST CAN AND CANNOT SHOW. The provisioning IMPORT path is
 * unreachable from this harness: `mos-provisioning-import` runs before anything
 * is listening and reads a medium, and there is no route into this guest to
 * stage one. So the assertion here is the other direction -- a device that was
 * never offered a document reports every import member `null` -- which is the
 * state the shipped image boots in and the baseline any future import phase
 * would be measured against. Importing a document on a live device stays a
 * bench item.
 */

import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";

function parseObject(body: string): Record<string, JsonValue> | undefined {
  try {
    const value = JSON.parse(body) as JsonValue;
    return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

const phase: Phase = {
  id: "06-onboarding-claim",
  title: "the device is claimed through setup, and no provisioning document ever reached it",
  assumes:
    "02 claimed this factory-fresh device with POST /api/v1/setup and left an authenticated " +
    "browser session; no medium carrying a provisioning document has ever been offered to it",

  async run({ client, report, config }: PhaseContext): Promise<void> {
    const provisioning = await client.get("/api/v1/provisioning/status");
    report.expectStatus(
      provisioning,
      200,
      "GET /api/v1/provisioning/status reports this device's import record",
    );
    // Exact and not a subset: ProvisioningStatus has four members and the
    // absence of a fifth is part of what is being asserted. `unclaimed` is
    // false because phase 02 claimed the device -- the same fact
    // `GET /api/v1/session` states, read here through the surface that decides
    // whether a document offered on a medium would be applied at all.
    // The member names are spelled in QUOTES so `spec-pins.sh` can read this
    // assertion's own copy of them out of the phase's bytes and compare it
    // against `components.schemas.ProvisioningStatus`. An unquoted key is
    // invisible to that gate.
    report.expectJson(
      provisioning,
      { "documentVersion": null, "documentDigest": null, "lastImport": null, "unclaimed": false },
      "a device no document ever reached reports every import member null, and is not unclaimed",
    );

    const claim = await client.get("/api/v1/claim");
    report.expectStatus(claim, 200, "GET /api/v1/claim reports which channel claimed the device");
    report.expectJson(
      claim,
      { "state": "claimed", "via": "setup", "rotationRequired": false },
      "the claim this device carries names setup as its channel and demands no rotation",
      { subset: true },
    );
    // `at` is the device clock's reading when the claim committed, so its
    // VALUE cannot be pinned -- but its presence and type can, and an absent
    // `at` on a claimed device is the shape a claim by document takes.
    const claimBody = parseObject(claim.body);
    const at = claimBody?.["at"];
    report.check(
      typeof at === "number" && at > 0,
      'the claim record carries the clock reading "at" it committed under',
      `actual body: ${claim.body}`,
    );

    // The second setup. This is the assertion the harness is uniquely placed
    // to make: whether a device is in setup mode is decided by what is IN its
    // settings store, and this is the same store the first setup wrote to a
    // real filesystem one phase ago.
    const again = await client.request("POST", "/api/v1/setup", {
      body: JSON.stringify({ password: config.adminPassword }),
      contentType: "application/json",
      sendCookies: false,
    });
    report.expectStatus(again, 409, "POST /api/v1/setup on a claimed device is a conflict");
    report.expectJson(
      again,
      { error: { code: "already_configured", source: "apid" } },
      "the second setup is refused as already_configured, not as a validation failure",
      { subset: true },
    );
  },
};

export default phase;
