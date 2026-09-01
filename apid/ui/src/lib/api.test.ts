import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, ApiError, rememberSession } from './api'

afterEach(() => {
  vi.unstubAllGlobals()
  rememberSession({ state: 'unauthenticated' })
})

describe('api client', () => {
  it('adds the session CSRF token only to mutations', async () => {
    rememberSession({ state: 'authenticated', csrfToken: 'csrf-value' })
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response('{"ok":true}', { status: 200 }))
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
    vi.stubGlobal('fetch', fetch)

    await api('/api/v1/meta')
    await api('/api/v1/session', { method: 'DELETE' })

    expect(((fetch.mock.calls[0][1] as RequestInit).headers as Headers).has('x-csrf-token')).toBe(false)
    expect(((fetch.mock.calls[1][1] as RequestInit).headers as Headers).get('x-csrf-token')).toBe('csrf-value')
  })

  it('turns the API error envelope into a typed error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(
      JSON.stringify({ error: { code: 'csrf_invalid', message: 'CSRF rejected', path: 'network' } }),
      { status: 403, headers: { 'content-type': 'application/json' } },
    )))
    await expect(api('/api/v1/network', { method: 'PUT' })).rejects.toEqual(
      new ApiError('CSRF rejected', 403, 'csrf_invalid', 'network'),
    )
  })
})
