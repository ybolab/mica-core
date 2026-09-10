import { describe, expect, it } from 'vitest'
import { ApiError } from '@/shared/lib/http'
import { queryClient } from './query-client'

describe('the shared query client', () => {
  /// A 401 is an answer, not an outage: retrying it just burns requests while
  /// the console has already fallen back to sign-in. Anything else is retried,
  /// because a device that is rebooting will answer again shortly.
  it('does not retry a refused session, but does retry a transport failure', () => {
    const retry = queryClient.getDefaultOptions().queries?.retry as (failures: number, error: Error) => boolean

    expect(retry(0, new ApiError('no session', 401))).toBe(false)
    expect(retry(0, new ApiError('the device is busy', 409))).toBe(true)
    expect(retry(0, new Error('network down'))).toBe(true)
    expect(retry(3, new Error('network down'))).toBe(false)
  })
})
