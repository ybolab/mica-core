import type { TFunction } from 'i18next'

const knownStateKeys = {
  available: 'common.states.available',
  unavailable: 'common.states.unavailable',
  enabled: 'common.states.enabled',
  disabled: 'common.states.disabled',
  unknown: 'common.states.unknown',
  finished: 'common.states.finished',
  succeeded: 'common.states.succeeded',
  failed: 'common.states.failed',
  pending: 'common.states.pending',
  running: 'common.states.running',
  queued: 'common.states.queued',
  routable: 'common.states.routable',
  carrier: 'common.states.carrier',
  degraded: 'common.states.degraded',
} as const

export function formatKnownState(value: string, t: TFunction) {
  const key = knownStateKeys[value as keyof typeof knownStateKeys]
  return key ? t(key) : value
}
