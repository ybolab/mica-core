/**
 * Phase 08 -- the reset tiers, and the presence gate holding on a real device.
 *
 * LAST, because it is the only phase that leaves the guest changed: a staged
 * tier 1 is an intent record mosd carries out on the next boot. Nothing here
 * reboots, so nothing is applied; the assertion is the staging and the record,
 * which is what `docs/design/recovery.md` §2.2 makes the whole of the commit.
 *
 * THE HIGHEST-VALUE ASSERTION IN THIS SUITE IS THE 403. Tier 3 and
 * `POST /api/v1/recovery/credential` are gated on a physical-presence
 * assertion, and `pkgs/mosd/apid/src/tests/reset.rs` proves the gate against a
 * `FakePresence`. A fake proves the branch; it does not prove that the SHIPPED
 * reader refuses on a device where no physical action was taken. That is
 * exactly the gap this phase closes: the shipped reader opens this board's own
 * recovery declaration on a real booted root and both gated operations are
 * refused with `presence_required`. Neither shipped board declares a physical
 * recovery action (`docs/design/recovery.md` §4), so the refusal a guest
 * produces is "this board declares none" -- the device's permanent state until
 * a board declares one and its BSP implements it, which makes asserting it
 * from the outside the strongest statement available.
 *
 * WHAT IS NOT ASSERTED, AND WHY. The applied side of a tier: staging is one
 * write, and seeing it carried out needs a second boot this one-boot harness
 * cannot afford. The gate's PASSING direction needs a board that declares an
 * action and a BSP that implements it, which no shipped board has. Both are
 * named as bench items in the workstream's report rather than approximated
 * here.
 */

import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";
import { CSRF_STATE } from "./02-session.ts";

function parseObject(body: string): Record<string, JsonValue> | undefined {
  try {
    const value = JSON.parse(body) as JsonValue;
    return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

const phase: Phase = {
  id: "08-reset-recovery",
  title: "a reset tier stages as an intent, and the presence-gated ones are refused",
  assumes:
    "02 left an authenticated browser session and its CSRF token in phase state, 06 confirmed the " +
    "claim demands no rotation (which would refuse every mutation), and nothing has asserted " +
    "physical presence at this guest",

  async run({ client, report, state }: PhaseContext): Promise<void> {
    const csrf = state.get(CSRF_STATE);
    if (typeof csrf !== "string") {
      report.fail("the reset phase received the browser CSRF token", `csrf=${typeof csrf}`);
      return;
    }
    const csrfHeader = { "X-CSRF-Token": csrf };

    // -- tier 1: an authenticated management action ---------------------------
    const staged = await client.request("POST", "/api/v1/reset", {
      body: JSON.stringify({ tier: "configuration" }),
      contentType: "application/json",
      headers: csrfHeader,
    });
    report.expectStatus(staged, 202, "POST /api/v1/reset stages a configuration reset");
    report.expectJson(
      staged,
      { "tier": "configuration", "applies": "next-boot" },
      "the staged tier says when it runs, and it is never now",
    );

    // The intent, read back out of the settings store it was written to. This
    // is the half a route test cannot reach: the record crossed the bus, was
    // saved as TOML by mosd and parsed back.
    const record = await client.get("/api/v1/settings/reset");
    report.expectStatus(record, 200, "GET /api/v1/settings/reset reads the staged intent back");
    report.expectJson(
      record,
      { tier: "configuration" },
      "the settings tree carries the tier that was staged",
      { subset: true },
    );
    const requested = parseObject(record.body)?.["requested"];
    report.check(
      typeof requested === "number" && requested > 0,
      'the staged intent records the clock reading "requested" it committed under',
      `actual body: ${record.body}`,
    );

    // -- tier 3: refused, because nothing at this device asserted presence ----
    const fullFactory = await client.request("POST", "/api/v1/reset", {
      body: JSON.stringify({ tier: "full-factory" }),
      contentType: "application/json",
      headers: csrfHeader,
    });
    report.expectStatus(
      fullFactory,
      403,
      "POST /api/v1/reset refuses full-factory on a device with no presence asserted",
    );
    report.expectJson(
      fullFactory,
      { error: { code: "presence_required", source: "apid" } },
      "the tier-3 refusal names the presence gate, not authentication",
      { subset: true },
    );

    // The refusal happens before the write, so tier 1 is still what is staged.
    // Asserted rather than assumed: a refused tier that had replaced the
    // staged one would be a silent downgrade of what the next boot does.
    const afterRefusal = await client.get("/api/v1/settings/reset");
    report.expectStatus(afterRefusal, 200, "the staged intent survives the refused tier");
    report.expectJson(
      afterRefusal,
      { tier: "configuration" },
      "a refused full-factory stages nothing and replaces nothing",
      { subset: true },
    );

    // -- credential recovery: presence is the authority, and the only one -----
    //
    // With the session first. §5.2 turns an authenticated caller away BEFORE
    // presence is consulted, and the two refusals share a status, so a phase
    // that only sent the anonymous request could not tell the ordering apart.
    const asOperator = await client.request("POST", "/api/v1/recovery/credential", {
      headers: csrfHeader,
    });
    report.expectStatus(
      asOperator,
      403,
      "POST /api/v1/recovery/credential refuses a caller holding a working credential",
    );
    report.expectJson(
      asOperator,
      { error: { code: "authenticated_session", source: "apid" } },
      "an authenticated caller is sent to change-password, not through recovery",
      { subset: true },
    );

    const anonymous = await client.request("POST", "/api/v1/recovery/credential", {
      sendCookies: false,
    });
    report.expectStatus(
      anonymous,
      403,
      "POST /api/v1/recovery/credential is refused with no presence asserted at the device",
    );
    report.expectJson(
      anonymous,
      { error: { code: "presence_required", source: "apid" } },
      "the shipped presence reader finds no assertion on a booted device and says so",
      { subset: true },
    );
  },
};

export default phase;
