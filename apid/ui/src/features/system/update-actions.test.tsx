import { act, cleanup, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { UpdateActions, UpdatePanel } from './system-page'

afterEach(() => {
  cleanup()
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

describe('update actions', () => {
  it('installs the exact acquired signed deployment identity', async () => {
    const fetch = stubFetch({
      '/api/v1/update': { lifecycle: { state: 'ready', deploymentId: 'a'.repeat(64) } },
      'POST /api/v1/update/install': () => jsonResponse({}, 202),
    })
    renderPanel(<UpdateActions />)
    await userEvent.click(await screen.findByRole('button', { name: 'Install update' }))
    await waitFor(() => expect(fetch.mock.calls.some(([path]) => path === '/api/v1/update/install')).toBe(true))
    const request = fetch.mock.calls.find(([path]) => path === '/api/v1/update/install')?.[1]
    expect(JSON.parse(String(request?.body))).toEqual({ deploymentId: 'a'.repeat(64) })
    expect(new Headers(request?.headers).get('content-type')).toBe('application/json')
  })

  it('follows an active operation to completion and then stops polling', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] })
    const fetch = vi.fn()
      .mockResolvedValueOnce(jsonResponse({ lifecycle: { state: 'checking' } }))
      .mockImplementation(() => Promise.resolve(jsonResponse({ lifecycle: { state: 'ready', deploymentId: 'a'.repeat(64), reason: 'check finished' } })))
    vi.stubGlobal('fetch', fetch)
    renderPanel(<><UpdatePanel /><UpdateActions /></>)
    await screen.findByText('checking')
    expect(screen.getByRole('button', { name: 'Install update' }).hasAttribute('disabled')).toBe(true)
    await act(async () => { await vi.advanceTimersByTimeAsync(2000) })
    await screen.findByText('check finished')
    expect(screen.getByRole('button', { name: 'Install update' }).hasAttribute('disabled')).toBe(false)
    const calls = fetch.mock.calls.length
    await act(async () => { await vi.advanceTimersByTimeAsync(5000) })
    expect(fetch).toHaveBeenCalledTimes(calls)
  })
})

it('displays the running, candidate and fallback identities with the running components', async () => {
  stubFetch({ '/api/v1/update': {
    boot: { deploymentId: 'a'.repeat(64), kernelId: 'c'.repeat(64), rootfsId: 'd'.repeat(64), contentVerified: true, secureBoot: true, backend: 'uefi', bootVerified: true },
    state: { current: 'a'.repeat(64), candidate: 'e'.repeat(64), fallback: 'b'.repeat(64), highestGeneration: 3, failed: [] },
    lifecycle: { state: 'reboot-required' },
  } })
  renderPanel(<UpdatePanel />)
  for (const id of ['a', 'b', 'c', 'd', 'e']) expect(await screen.findByText(id.repeat(64))).toBeTruthy()
})
