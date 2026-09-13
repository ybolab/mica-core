import type { TFunction } from 'i18next'

export type ConnectionState = 'connected' | 'reconnecting' | 'offline'

/// The three states the prototype's chrome distinguishes, derived from the one
/// health read the shell already makes. `failureCount` is React Query's retry
/// counter: it rises while a read is being retried and the previous answer is
/// still on screen, which is exactly the prototype's reconnecting case.
export function connectionState(health: { isError: boolean; failureCount: number; micad: string | undefined }): ConnectionState {
  if (health.isError) return 'offline'
  if (health.micad !== undefined && health.micad !== 'ok') return 'offline'
  return health.failureCount > 0 ? 'reconnecting' : 'connected'
}

export type Freshness = { unit: 'now' } | { unit: 'minutes' | 'hours'; value: number }

/// Bucketed on purpose. The prototype prints a fixed "12 s ago", and a real
/// second counter would make the console's appearance depend on the moment it
/// was rendered, including in the committed screenshot baselines.
export function freshnessAge(ageMs: number): Freshness {
  if (ageMs < 60_000) return { unit: 'now' }
  const minutes = Math.floor(ageMs / 60_000)
  return minutes < 60 ? { unit: 'minutes', value: minutes } : { unit: 'hours', value: Math.floor(minutes / 60) }
}

/// The freshness of a read, as the shell and the page headers print it.
export function freshnessLabel(updatedAt: number, t: TFunction) {
  if (!updatedAt) return t('shell.updated.now')
  const age = freshnessAge(Date.now() - updatedAt)
  return age.unit === 'now' ? t('shell.updated.now') : t(`shell.updated.${age.unit}`, { count: age.value })
}
