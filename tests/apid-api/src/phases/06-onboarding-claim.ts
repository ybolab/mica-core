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
    // The import record's own members, by value. A SUBSET now, because
    // PLAN-070 F9 extended this route with the baked/operator/effective
    // reading -- but the absence of an unexpected member is still asserted,
    // below, over the key set rather than over the whole object. Pinning the
    // whole object would pin `bakedDigests`, whose values are content hashes
    // that move with every build, and a phase that must be edited on every
    // rebuild stops being read.
    //
    // The member names are spelled in QUOTES so `spec-pins.sh` can read this
    // assertion's own copy of them out of the phase's bytes and compare it
    // against `components.schemas.ProvisioningStatus`. An unquoted key is
    // invisible to that gate.
    report.expectJson(
      provisioning,
      { "documentVersion": null, "documentDigest": null, "lastImport": null, "unclaimed": false },
      "a device no document ever reached reports every import member null, and is not unclaimed",
      { subset: true },
    );

    // The half the subset above gives up, kept as its own assertion: the
    // member set is exactly these eight, so one added or removed without a
    // decision turns this red. This was an exact-object assertion until F9
    // legitimately grew the route, and the drift went unnoticed because
    // nothing booted the image between -- `spec-pins.sh` compares the names a
    // phase pins against the schema and is structurally unable to see a
    // member the phase never named.
    const status = JSON.parse(provisioning.body) as Record<string, unknown>;
    const members = Object.keys(status).sort().join(",");
    const expectedMembers = [
      "baked", "bakedDigests", "documentDigest", "documentVersion",
      "effective", "lastImport", "operator", "unclaimed",
    ].join(",");
    report.check(
      members === expectedMembers,
      "GET /api/v1/provisioning/status carries exactly the members the contract names",
      `expected: ${expectedMembers}\nactual:   ${members}`,
    );

    // F9's gate, driven from outside the process for the first time: the
    // baked, operator and effective readings are distinguishable. This guest
    // has overridden nothing, so `operator` is empty and `effective` must
    // equal the baked values -- the case that shows the three come from one
    // resolution rather than each being read from somewhere of its own.
    const layer = (name: string): Record<string, unknown> =>
      (status[name] ?? {}) as Record<string, unknown>;
    const layerValue = (name: string, group: string, key: string): string =>
      JSON.stringify(((layer(name)[group] ?? {}) as Record<string, unknown>)[key]);
    report.check(
      Object.keys(layer("operator")).length === 0,
      "a device whose operator has overridden nothing reports an empty operator layer",
      `actual: ${JSON.stringify(status["operator"])}`,
    );
    report.check(
      layerValue("effective", "update", "channel") === layerValue("baked", "update", "channel")
        && layerValue("effective", "update", "source") === layerValue("baked", "update", "source")
        && layerValue("effective", "fleet", "url") === layerValue("baked", "fleet", "url"),
      "with nothing overridden the effective channel, source and fleet address are the baked ones",
      `baked:     ${JSON.stringify({ update: layer("baked")["update"], fleet: layer("baked")["fleet"] })}\n`
        + `effective: ${JSON.stringify({ update: layer("effective")["update"], fleet: layer("effective")["fleet"] })}`,
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
