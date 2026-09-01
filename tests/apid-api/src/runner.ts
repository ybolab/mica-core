/**
 * Phases, and their assumptions made structural.
 *
 * This suite runs against ONE boot. A TCG boot with no /dev/kvm on a quiet
 * machine reaches apid's APID_LISTENING line in 60-66s and both readiness
 * signals in 65-72s. Per-test isolation would multiply that boot across dozens
 * of checks until it dominated everything the suite measures, so the phases
 * are ordered and hand state to each other.
 * State coupling is accepted and then made visible, because the failure it
 * invites is reading a phase-4 red as a phase-4 bug when phase 3 is what broke.
 *
 * Two mechanisms carry that:
 *
 *   - `assumes` is a REQUIRED non-empty string on every phase, and the runner
 *     refuses a registry containing an empty one. That turns "each phase states
 *     what it assumes the previous one left behind" from a comment somebody can
 *     delete into a property of the code.
 *   - Once any phase fails, every LATER phase is SKIPPED, and its skip line
 *     quotes the failed phase and this phase's own assumption.
 */

import type { Client } from "./client.ts";
import type { Config } from "./config.ts";
import type { Reporter } from "./report.ts";

export interface PhaseContext {
  readonly client: Client;
  readonly report: Reporter;
  readonly config: Config;
  /** Handed from phase to phase. What a phase leaves here, its `assumes` says. */
  readonly state: Map<string, unknown>;
}

export interface Phase {
  /** Registry id, e.g. "04-network-observation". Also what APID_PHASES selects on. */
  readonly id: string;
  readonly title: string;
  /** REQUIRED and non-empty: what this phase assumes the previous ones left. */
  readonly assumes: string;
  run(ctx: PhaseContext): Promise<void>;
}

/** A phase was registered without stating what it assumes. */
export class EmptyAssumesError extends Error {
  constructor(readonly phaseId: string) {
    super(
      `phase ${JSON.stringify(phaseId)} has an empty "assumes". Every phase in ` +
        `this suite must state what it assumes the previous phase left behind; ` +
        `the runner refuses to run a registry that does not.`,
    );
    this.name = "EmptyAssumesError";
  }
}

/** APID_PHASES named a phase that is not in the registry. */
export class UnknownPhaseError extends Error {
  constructor(readonly requested: string, readonly known: readonly string[]) {
    super(
      `APID_PHASES named ${JSON.stringify(requested)}, which is not a phase. ` +
        `Known phases: ${known.join(", ")}`,
    );
    this.name = "UnknownPhaseError";
  }
}

const WRAP_COLUMNS = 78;
const ASSUMES_PREFIX = "  assumes: ";

/**
 * Run `phases` in registry order, honouring `selection` if given.
 *
 * Throws `EmptyAssumesError` or `UnknownPhaseError` BEFORE running anything: a
 * malformed registry is a programming error, and discovering it after a boot
 * has been spent is the wrong time.
 */
export async function runPhases(
  phases: readonly Phase[],
  ctx: PhaseContext,
  selection?: readonly string[],
): Promise<void> {
  for (const phase of phases) {
    if (phase.assumes.trim() === "") throw new EmptyAssumesError(phase.id);
  }

  const known = phases.map((phase) => phase.id);
  let selected = phases;
  if (selection !== undefined) {
    for (const id of selection) {
      if (!known.includes(id)) throw new UnknownPhaseError(id, known);
    }
    selected = phases.filter((phase) => selection.includes(phase.id));
    const omitted = phases.filter((phase) => !selection.includes(phase.id));
    if (omitted.length > 0) {
      // Loud, because silent truncation reads as "everything was covered".
      ctx.report.note("");
      ctx.report.note(
        `NOTE: APID_PHASES restricted this run to: ${selected.map((p) => p.id).join(", ")}`,
      );
      ctx.report.note(`NOTE: NOT RUN: ${omitted.map((p) => p.id).join(", ")}`);
      ctx.report.note(
        "NOTE: this RESULT is PARTIAL. It says nothing about the phases that were not run.",
      );
    }
    for (const phase of omitted) {
      ctx.report.skipPhase(phase.id, phase.title, phase.assumes, "not selected by APID_PHASES");
    }
  }

  let failedPhaseId: string | undefined;
  for (const phase of selected) {
    if (failedPhaseId !== undefined) {
      ctx.report.skipPhase(
        phase.id,
        phase.title,
        phase.assumes,
        `not run: ${failedPhaseId} failed and this phase assumes ${phase.assumes}`,
      );
      continue;
    }

    ctx.report.note("");
    ctx.report.note(`PHASE ${phase.id}: ${phase.title}`);
    for (const line of wrapAssumes(phase.assumes)) ctx.report.note(line);

    const failuresBefore = ctx.report.failures;
    ctx.report.beginPhase(phase.id, phase.title, phase.assumes);
    try {
      await phase.run(ctx);
    } catch (error) {
      // A throw is a failed check, not a crashed suite: the RESULT line must
      // still be printed, and the later phases must still be skipped rather
      // than vanish. The image verifier learned the same lesson from SIGPIPE.
      ctx.report.fail(`${phase.id} threw instead of reporting`, describeThrow(error));
    }
    ctx.report.endPhase();
    if (ctx.report.failures > failuresBefore) failedPhaseId = phase.id;
  }
}

function describeThrow(error: unknown): string {
  if (error instanceof Error) {
    return [
      `expected: the phase to record its own PASS/FAIL lines`,
      `actual:   ${error.name}: ${error.message}`,
      `stack:    ${(error.stack ?? "").split("\n").slice(1, 4).join(" | ").trim()}`,
    ].join("\n");
  }
  return [
    `expected: the phase to record its own PASS/FAIL lines`,
    `actual:   a non-Error was thrown: ${String(error)}`,
  ].join("\n");
}

/** `  assumes: ...`, wrapped with continuations aligned under the first word. */
export function wrapAssumes(assumes: string): string[] {
  const continuation = " ".repeat(ASSUMES_PREFIX.length);
  const words = assumes.split(/\s+/).filter((word) => word !== "");
  const lines: string[] = [];
  let current = ASSUMES_PREFIX;
  let empty = true;
  for (const word of words) {
    if (!empty && current.length + 1 + word.length > WRAP_COLUMNS) {
      lines.push(current);
      current = continuation + word;
    } else {
      current = empty ? current + word : `${current} ${word}`;
    }
    empty = false;
  }
  lines.push(current);
  return lines;
}
