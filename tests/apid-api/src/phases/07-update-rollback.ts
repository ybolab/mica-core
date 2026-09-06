/**
 * Phase 07 -- the rollback verdict a real device computes, and the refusal it
 * answers a rollback request with.
 *
 * WHAT ONLY A BOOT CAN SHOW. `rollback_eligibility` in
 * `pkgs/mosd/mosd/src/rauc.rs` is exhaustively unit-tested over synthesised
 * slot lists. What no unit test reaches is the chain that produces those
 * lists on a device: RAUC's daemon answering `GetSlotStatus` over the system
 * bus, mosd folding it into the live-state `update` document, and apid serving
 * the `rollback` object out of the same read that carries `slots`,
 * `booted_slot` and `primary`. A break anywhere in that chain looks, from a
 * unit test, exactly like a working system.
 *
 * THE REFUSAL IS THE REACHABLE CASE, AND THAT IS THE POINT. A factory image
 * has written slot A only, so the alternate rootfs carries no bundle version
 * and no install timestamp -- `alternate_never_installed`, the guard's
 * fail-closed answer to a slot there is no system in. This phase therefore
 * asserts the CODE of a 409 rather than the success of a rollback. Driving a
 * permitted rollback would take a real install into the alternate slot and a
 * second boot, which this one-boot harness cannot afford; it stays a bench
 * item, named in the workstream's report.
 *
 * AND IT DOES NOT GUESS WHICH REFUSAL. The reason is read out of
 * `GET /api/v1/update` first and the refusal's error code is required to be
 * exactly what apid's `rollback_reason_code` maps that reason to. So this
 * passes for whichever verdict the guest's slot state produces, and still goes
 * red if mosd's word and apid's code stop agreeing -- which is the seam
 * between the two that nothing else here crosses.
 */

import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";
import { CSRF_STATE } from "./02-session.ts";

/**
 * The refusal vocabulary apid serves, transcribed from `ROLLBACK_REASONS` in
 * `pkgs/mosd/apid/src/update_api.rs`.
 *
 * A second copy on purpose, and never imported: a reason outside this list is
 * answered as the generic `rollback_refused`, so the mapping this phase
 * asserts is only a real assertion while the list it compares against comes
 * from somewhere other than the code under test.
 */
const KNOWN_REASONS = [
  "no_alternate_slot",
  "alternate_is_booted_slot",
  "alternate_never_installed",
  "alternate_marked_bad",
  "alternate_is_newer",
  "install_order_unknown",
  "booted_slot_not_confirmed",
] as const;

function parseObject(body: string): Record<string, JsonValue> | undefined {
  try {
    const value = JSON.parse(body) as JsonValue;
    return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

function asObject(value: JsonValue | undefined): Record<string, JsonValue> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
}

const phase: Phase = {
  id: "07-update-rollback",
  title: "the update state carries a rollback verdict, and a rollback is refused by name",
  assumes:
    "02 left an authenticated browser session and its CSRF token in phase state, and this guest " +
    "has taken no update, so its alternate rootfs slot has never been installed into",

  async run({ client, report, state }: PhaseContext): Promise<void> {
    const csrf = state.get(CSRF_STATE);
    if (typeof csrf !== "string") {
      report.fail("the rollback phase received the browser CSRF token", `csrf=${typeof csrf}`);
      return;
    }

    const update = await client.get("/api/v1/update");
    report.expectStatus(update, 200, "GET /api/v1/update returns the device's update state");
    const rollback = asObject(parseObject(update.body)?.["rollback"]);
    if (rollback === undefined) {
      report.fail(
        "GET /api/v1/update carries the `rollback` object beside the slot state",
        `actual body: ${update.body}`,
      );
      return;
    }
    // Three members and no fourth: `RollbackEligibility::permitted` is derived
    // from `reason`, so a body offering a fourth would be offering a second
    // place for the two to disagree.
    // Four members since RFCT-321 added `explanation` -- the sentence a
    // refusal carries only where it adds something the reason code does not
    // already say, which is null for the reasons a factory guest produces.
    // It is asserted here rather than left to the subset above because the
    // route's own description in `openapi.json` names the members, and a
    // document that names three while the daemon serves four is published API
    // text that is wrong.
    report.check(
      ["target", "permitted", "reason", "explanation"].every((name) => Object.hasOwn(rollback, name)) &&
        Object.keys(rollback).length === 4,
      "the rollback verdict is `target`, `permitted`, `reason` and `explanation`, and nothing else",
      `actual rollback: ${JSON.stringify(rollback)}`,
    );

    const permitted = rollback["permitted"];
    const reason = rollback["reason"];
    report.check(
      permitted === false,
      "a guest that has never taken an update does not permit a rollback",
      `actual rollback: ${JSON.stringify(rollback)}`,
    );
    report.check(
      typeof reason === "string" && reason.length > 0,
      "the refused verdict names its reason rather than refusing silently",
      `actual rollback: ${JSON.stringify(rollback)}`,
    );

    if (permitted !== false || typeof reason !== "string") {
      // Not a fail: the two checks above already recorded what is wrong. This
      // is the guard against the one destructive thing this suite could do --
      // condemning the slot the guest is running from -- and it must not
      // happen on a device whose state was not what this phase read.
      report.skip(
        "POST /api/v1/update/rollback is refused with the reason the state document carries",
        `the update state does not describe a refused rollback (${JSON.stringify(rollback)}), so ` +
          "requesting one could mark the booted slot bad",
      );
      return;
    }

    const refused = await client.request("POST", "/api/v1/update/rollback", {
      headers: { "X-CSRF-Token": csrf },
    });
    report.expectStatus(
      refused,
      409,
      "POST /api/v1/update/rollback is a conflict, not a validation failure",
    );
    const expectedCode = (KNOWN_REASONS as readonly string[]).includes(reason)
      ? reason
      : "rollback_refused";
    report.expectJson(
      refused,
      { error: { code: expectedCode, source: "apid" } },
      "the refusal's error code is mosd's own verdict, mapped through apid's vocabulary",
      { subset: true },
    );
  },
};

export default phase;
