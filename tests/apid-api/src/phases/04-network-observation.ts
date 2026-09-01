/** The network resource separates configured intent from current observation. */

import type { JsonValue } from "../report.ts";
import type { Phase, PhaseContext } from "../runner.ts";

function record(value: JsonValue | undefined): Record<string, JsonValue> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value : undefined;
}

const phase: Phase = {
  id: "04-network-observation",
  title: "the network API reports current interfaces, counts and link states",
  assumes: "02 left an authenticated session; 03 proved it still reaches management API reads",

  async run({ client, report }: PhaseContext): Promise<void> {
    const response = await client.get("/api/v1/network");
    report.expectStatus(response, 200, "GET /api/v1/network returns configured and observed state");

    let body: Record<string, JsonValue> | undefined;
    try {
      body = record(JSON.parse(response.body) as JsonValue);
    } catch {
      body = undefined;
    }
    if (body === undefined) {
      report.fail("the network overview is a JSON object", `actual body: ${response.body}`);
      return;
    }

    const observed = record(body["observed"]);
    report.check(
      record(body["configured"]) !== undefined &&
        typeof body["configuredCount"] === "number" &&
        record(body["observed"]) !== undefined,
      "the NetworkOverview carries \"configured\", \"configuredCount\" and \"observed\"",
      `actual body: ${response.body}`,
    );
    if (observed === undefined) return;

    const available = observed["available"];
    const interfaceCount = observed["interfaceCount"];
    const interfaces = observed["interfaces"];
    report.check(
      available === true,
      "live systemd-networkd observation is available in the booted guest",
      `actual observed object: ${JSON.stringify(observed)}`,
    );
    report.check(
      typeof observed["interfaceCount"] === "number" &&
        observed["interfaceCount"] > 0 &&
        Array.isArray(observed["interfaces"]),
      "the observed network has a positive \"interfaceCount\" and an \"interfaces\" list",
      `actual observed object: ${JSON.stringify(observed)}`,
    );
    if (!Array.isArray(interfaces)) return;
    report.check(
      interfaceCount === interfaces.length,
      "the observed interface count equals the number of interface detail rows",
      `count=${String(interfaceCount)}; rows=${interfaces.length}`,
    );

    for (const [index, value] of interfaces.entries()) {
      const iface = record(value);
      const name = iface?.["name"];
      const hasState = [
        "administrativeState",
        "operationalState",
        "carrierState",
        "addressState",
        "onlineState",
      ].some((key) => typeof iface?.[key] === "string");
      report.check(
        typeof name === "string" && name.length > 0 && hasState,
        `observed interface ${index + 1} names the link and reports at least one current state`,
        `actual row: ${JSON.stringify(value)}`,
      );
      report.check(
        Array.isArray(iface?.["addresses"]),
        `observed interface ${typeof name === "string" ? name : index + 1} carries its address list`,
        `actual row: ${JSON.stringify(value)}`,
      );
    }
  },
};

export default phase;
