import { useMutation, useQuery } from '@tanstack/react-query'
import { cleanup, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, expect, it, vi } from 'vitest'
import { api, type SessionStatus } from '@/shared/lib/http'
import { sessionKey } from '@/features/auth/auth'
import { stubFetch, jsonResponse } from '@/shared/testing/panel'
import { AppProviders } from './providers'
import { queryClient } from './query-client'

afterEach(() => { cleanup(); queryClient.clear(); vi.unstubAllGlobals() })

function SessionProbe() {
  const session = useQuery({ queryKey: sessionKey, queryFn: () => api<SessionStatus>('/api/v1/session'), retry: false })
  useQuery({ queryKey: ['protected-probe'], queryFn: () => api('/api/v1/health'), enabled: session.data?.state === 'authenticated', retry: false })
  return <p>{session.data?.state ?? 'loading'}</p>
}

it('returns the application to an unauthenticated session when a protected read returns 401', async () => {
  stubFetch({
    '/api/v1/session': { state: 'authenticated', csrfToken: 'expired-token' },
    '/api/v1/health': () => jsonResponse({ error: { code: 'not_authenticated' } }, 401),
  })
  render(<AppProviders><SessionProbe /></AppProviders>)
  expect(await screen.findByText('unauthenticated')).toBeTruthy()
})

it('drops what an expired session had already read, so nothing stale reads as current', async () => {
  stubFetch({
    '/api/v1/session': { state: 'authenticated', csrfToken: 'expired-token' },
    '/api/v1/health': () => jsonResponse({ error: { code: 'not_authenticated' } }, 401),
  })
  queryClient.setQueryData(['tokens'], [{ id: 'tok', name: 'fleet agent', created: 0 }])
  render(<AppProviders><SessionProbe /></AppProviders>)

  expect(await screen.findByText('unauthenticated')).toBeTruthy()
  expect(queryClient.getQueryData(['tokens'])).toBeUndefined()
})

function LogoutProbe() {
  const session = useQuery({ queryKey: sessionKey, queryFn: () => api<SessionStatus>('/api/v1/session'), retry: false })
  const logout = useMutation({ mutationFn: () => api('/api/v1/session', { method: 'DELETE' }) })
  return <>
    <p>{session.data?.state ?? 'loading'}</p>
    <button onClick={() => logout.mutate()}>Log out</button>
    {logout.isError && <p>Logout rejected</p>}
  </>
}

it.each([401, 503])('handles a rejected logout with HTTP %i without confusing expiry with an outage', async (status) => {
  stubFetch({
    '/api/v1/session': () => jsonResponse({ state: 'authenticated', csrfToken: 'token' }),
    'DELETE /api/v1/session': () => jsonResponse({ error: { code: 'rejected' } }, status),
  })
  render(<AppProviders><LogoutProbe /></AppProviders>)
  expect(await screen.findByText('authenticated')).toBeTruthy()
  fireEvent.click(screen.getByRole('button', { name: 'Log out' }))
  expect(await screen.findByText('Logout rejected')).toBeTruthy()
  expect(await screen.findByText(status === 401 ? 'unauthenticated' : 'authenticated')).toBeTruthy()
})
