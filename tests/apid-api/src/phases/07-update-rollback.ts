/** Native deployment identity, confirmation and manual rollback on a fresh signed image. */
import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";
import { CSRF_STATE } from "./02-session.ts";

function object(value: unknown): Record<string, JsonValue> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, JsonValue> : undefined;
}
function body(text: string): Record<string, JsonValue> | undefined {
  try { return object(JSON.parse(text)); } catch { return undefined; }
}
const isId = (value: unknown): value is string => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);

const phase: Phase = {
  id: "07-update-rollback",
  title: "native deployment confirmation and retained-fallback rollback",
  assumes: "02 left an authenticated browser session; the fresh complete image has two signed factory deployments and no installed update",
  async run({ client, report, state }: PhaseContext): Promise<void> {
    const csrf = state.get(CSRF_STATE);
    if (typeof csrf !== "string") {
      report.fail("the update phase received its CSRF token", `csrf=${typeof csrf}`);
      return;
    }
    let update = await client.get("/api/v1/update");
    for (let attempt = 0; attempt < 30 && object(body(update.body)?.rollback)?.permitted !== true; attempt++) {
      await Bun.sleep(2000);
      update = await client.get("/api/v1/update");
    }
    report.expectStatus(update, 200, "native update state is available over the real API and D-Bus");
    const document = body(update.body);
    const boot = object(document?.boot);
    const native = object(document?.state);
    const rollback = object(document?.rollback);
    const running = boot?.deploymentId;
    const target = rollback?.target;
    report.check(isId(running) && isId(boot?.kernelId) && isId(boot?.rootfsId)
      && boot?.contentVerified === true && boot?.secureBoot === true && boot?.bootVerified === true,
    "the API reports authenticated current boot and component identities", JSON.stringify(boot));
    report.check(native?.current === running && native?.candidate === null && isId(native?.fallback)
      && native.fallback !== running && rollback?.permitted === true && rollback.reason === null && target === native.fallback,
    "health confirmed the running deployment and retained the second factory deployment", JSON.stringify({ native, rollback }));
    if (!isId(running) || !isId(target) || rollback?.permitted !== true) return;
    const headers = { "X-CSRF-Token": csrf };
    const malformed = await client.request("POST", "/api/v1/update/confirm", {
      headers, contentType: "application/json", body: JSON.stringify({ deploymentId: "invalid" }),
    });
    report.expectStatus(malformed, 400, "confirmation refuses a malformed deployment ID");
    const csrfRefused = await client.request("POST", "/api/v1/update/rollback");
    report.expectStatus(csrfRefused, 403, "rollback requires browser CSRF authorization");
    const confirm = await client.request("POST", "/api/v1/update/confirm", { headers, contentType: "application/json", body: JSON.stringify({ deploymentId: running }) });
    report.expectStatus(confirm, 200, "reconfirmation reaches the native backend and succeeds idempotently");
    const rolled = await client.request("POST", "/api/v1/update/rollback", { headers });
    report.expectStatus(rolled, 200, "manual rollback commits the retained fallback without rebooting the guest");
    report.expectJson(rolled, { deploymentId: running, target, nextStep: "POST /api/v1/actions/reboot" },
      "rollback reports the exact rejected deployment and retained target");
    const after = await client.get("/api/v1/update");
    const result = body(after.body);
    const failed = object(result?.state)?.failed;
    const verdict = object(result?.rollback);
    report.check(Array.isArray(failed) && failed.includes(running) && verdict?.permitted === false,
      "native status records the failed deployment and refuses a second rollback", JSON.stringify(result));
    const repeated = await client.request("POST", "/api/v1/update/rollback", { headers });
    report.expectStatus(repeated, 409, "repeating rollback is refused by the current backend state");
    report.expectJson(repeated, { error: { code: verdict?.reason ?? null, source: "apid" } },
      "the repeated request carries the backend refusal reason", { subset: true });
  },
};
export default phase;
