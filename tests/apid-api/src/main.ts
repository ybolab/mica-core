/**
 * The suite's entry point and the ordered phase registry.
 *
 * Every phase module is imported here, up front, so adding work means filling
 * in one phase module rather than editing a shared file.
 *
 * The order is fixed by design, not by convenience: it runs from the cheapest
 * and least destructive assertion to the one that takes the guest down, so a
 * failure early costs nothing and a failure late has everything before it
 * already recorded.
 */

import { Client } from "./client.ts";
import { loadConfigOrExit } from "./config.ts";
import { Reporter } from "./report.ts";
import { runPhases, type Phase, type PhaseContext } from "./runner.ts";

import spaBoundary from "./phases/01-spa-boundary.ts";
import session from "./phases/02-session.ts";
import apiManagement from "./phases/03-api-management.ts";
import networkObservation from "./phases/04-network-observation.ts";
import kernelNet from "./phases/05c-kernel-net.ts";

export const PHASES: readonly Phase[] = [
  spaBoundary,
  session,
  apiManagement,
  networkObservation,
  kernelNet,
];

async function main(): Promise<void> {
  const config = loadConfigOrExit();
  const report = new Reporter({
    host: config.host,
    resultJsonPath: config.resultJson,
    negative: config.negative,
  });
  const client = new Client(config);

  report.note(
    `apid black-box suite against ${client.origin("https")} (plain HTTP on :${config.httpPort})`,
  );
  if (config.negative !== undefined) {
    report.note(
      `NOTE: APID_NEGATIVE=${config.negative} -- a check will be inverted; this run should be RED.`,
    );
  }

  const ctx: PhaseContext = { client, report, config, state: new Map<string, unknown>() };
  try {
    await runPhases(PHASES, ctx, config.phases);
  } catch (error) {
    // Registry-level refusals (an empty `assumes`, an unknown APID_PHASES id)
    // land here. They are reported as a failed check so the RESULT line is
    // still printed and the exit code is still meaningful.
    report.fail(
      "the phase registry and the phase selection are usable",
      error instanceof Error ? `${error.name}: ${error.message}` : String(error),
    );
  }

  report.finish();
}

await main();
