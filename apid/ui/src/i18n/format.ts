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

/// A device timestamp rendered as an age, in the same buckets the shell uses
/// for read freshness. Anything the device did not date reads as `just now`
/// rather than inventing a distance.
export function formatAge(ageMs: number, t: TFunction) {
  if (!Number.isFinite(ageMs) || ageMs < 60_000) return t('common.age.now')
  const minutes = Math.floor(ageMs / 60_000)
  return minutes < 60
    ? t('common.age.minutes', { count: minutes })
    : t('common.age.hours', { count: Math.floor(minutes / 60) })
}
