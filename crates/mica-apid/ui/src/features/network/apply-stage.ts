import type { ConnectionState } from '@/features/shell/connection'

export const applyStages = ['saved', 'applying', 'reconnecting', 'applied'] as const

export type ApplyStage = (typeof applyStages)[number] | 'failed'

/// Where an accepted interface change has got to. The prototype shows four
/// steps; each one here is observed rather than timed. `reconnecting` is the
/// case the prototype cares about most — the change took the device away from
/// this browser — and it is read from the shell's own health state, not
/// guessed from how long the task has been running.
export function applyStage(
  task: { status?: string; outcome?: string } | undefined,
  connection: ConnectionState,
): ApplyStage {
  if (task?.status === 'finished') return task.outcome === 'succeeded' ? 'applied' : 'failed'
  if (connection !== 'connected') return 'reconnecting'
  return task === undefined ? 'saved' : 'applying'
}

/// The step's own state, for a strip that shows all four at once.
export function stageState(step: (typeof applyStages)[number], stage: ApplyStage): 'done' | 'active' | 'pending' {
  if (stage === 'failed') return step === 'saved' ? 'done' : step === 'applying' ? 'active' : 'pending'
  const reached = applyStages.indexOf(stage)
  const index = applyStages.indexOf(step)
  if (index < reached) return 'done'
  return index === reached ? (stage === 'applied' ? 'done' : 'active') : 'pending'
}
