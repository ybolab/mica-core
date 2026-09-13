import { describe, expect, it } from 'vitest'
import { connectionState, freshnessAge } from './connection'

describe('shell connection state', () => {
  it('is connected while the daemon answers ok', () => {
    expect(connectionState({ isError: false, failureCount: 0, micad: 'ok' })).toBe('connected')
  })

  it('is reconnecting while a read is retrying and the previous answer still stands', () => {
    expect(connectionState({ isError: false, failureCount: 2, micad: 'ok' })).toBe('reconnecting')
  })

  it('is offline once the read has failed outright', () => {
    expect(connectionState({ isError: true, failureCount: 3, micad: 'ok' })).toBe('offline')
  })

  /// A daemon that answers but reports itself unwell is not a transport
  /// problem, so it must not read as a retry that might still succeed.
  it('is offline when the daemon answers and reports itself not ok', () => {
    expect(connectionState({ isError: false, failureCount: 0, micad: 'degraded' })).toBe('offline')
  })

  it('is connected before the first answer arrives', () => {
    expect(connectionState({ isError: false, failureCount: 0, micad: undefined })).toBe('connected')
  })
})

describe('read freshness', () => {
  /// Bucketed rather than counted in seconds: a live second counter would make
  /// every appearance baseline depend on when it was taken.
  it('calls anything under a minute just now', () => {
    expect(freshnessAge(0)).toEqual({ unit: 'now' })
    expect(freshnessAge(59_000)).toEqual({ unit: 'now' })
  })

  it('reports whole minutes up to an hour', () => {
    expect(freshnessAge(60_000)).toEqual({ unit: 'minutes', value: 1 })
    expect(freshnessAge(3_540_000)).toEqual({ unit: 'minutes', value: 59 })
  })

  it('reports whole hours beyond that', () => {
    expect(freshnessAge(3_600_000)).toEqual({ unit: 'hours', value: 1 })
    expect(freshnessAge(9_000_000)).toEqual({ unit: 'hours', value: 2 })
  })

  it('treats a clock that ran backwards as a fresh read', () => {
    expect(freshnessAge(-5_000)).toEqual({ unit: 'now' })
  })
})
