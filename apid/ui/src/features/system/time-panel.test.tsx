import { cleanup, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { TimeStatus } from '@/lib/types'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { NtpServersPanel, SyncStatusPanel, TimezonePanel } from './time-panel'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('time synchronization status', () => {
  it('reports a synchronized clock with the server and the last sample', async () => {
    stubFetch({
      '/api/v1/time/status': {
        status: 'synchronized',
        synchronized: true,
        server: { name: 'time.cloudflare.com', address: '162.159.200.1' },
        sample: { leap: 0, stratum: 3, spike: false, offsetSeconds: 0.0024, packetCount: 8, correction: 'slew' },
      } satisfies TimeStatus,
    })
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText('synchronized with network time')).toBeTruthy()
    expect(screen.getByText('time.cloudflare.com (162.159.200.1)')).toBeTruthy()
    expect(screen.getByText('stratum 3, offset 2.4 ms')).toBeTruthy()
    expect(screen.getByText('slewing (ordinary drift)')).toBeTruthy()
  })

  it('names the degraded state and says retries continue', async () => {
    stubFetch({ '/api/v1/time/status': { status: 'offline-degraded', synchronized: false } satisfies TimeStatus })
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText(/degraded — no reachable time server/)).toBeTruthy()
    expect(screen.getByText(/keeps retrying every 30 seconds/)).toBeTruthy()
  })

  it('renders an unobservable time service as absence with its reason', async () => {
    stubFetch({
      '/api/v1/time/status': {
        status: 'unknown',
        detail: 'systemd-timesyncd is not reachable on the bus',
      } satisfies TimeStatus,
    })
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText('status unavailable — the time service is not observable')).toBeTruthy()
    expect(screen.getByText('systemd-timesyncd is not reachable on the bus')).toBeTruthy()
  })

  it('offers no pause or enable control for synchronization', async () => {
    stubFetch({ '/api/v1/time/status': { status: 'synchronized', synchronized: true } satisfies TimeStatus })
    renderPanel(<SyncStatusPanel />)

    await screen.findByText('synchronized with network time')
    expect(screen.queryAllByRole('switch')).toHaveLength(0)
    expect(screen.queryAllByRole('button')).toHaveLength(0)
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/time/status': () => jsonResponse({ error: { message: 'mosd timed out' } }, 504) })
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText('mosd timed out')).toBeTruthy()
  })
})

describe('time settings', () => {
  it('writes the server list one per line and follows the accepted task', async () => {
    let taskStatus = { id: 'task-9', dotPath: 'time.ntp.servers', status: 'running', foldedCount: 0 }
    const fetch = stubFetch({
      '/api/v1/settings/time.ntp.servers': ['0.pool.ntp.org'],
      'PUT /api/v1/settings/time.ntp.servers': () => jsonResponse({ taskId: 'task-9' }),
      '/api/v1/tasks/task-9': () => jsonResponse(taskStatus),
    })
    renderPanel(<NtpServersPanel />)

    const textarea = await screen.findByRole('textbox', { name: 'Servers' })
    await waitFor(() => expect((textarea as HTMLTextAreaElement).value).toBe('0.pool.ntp.org'))
    await userEvent.clear(textarea)
    await userEvent.type(textarea, 'time.example.com{enter}1.pool.ntp.org')
    await userEvent.click(screen.getByRole('button', { name: 'Save servers' }))

    await waitFor(() => expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')).toBe(true))
    const put = fetch.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')!
    expect(put[0]).toBe('/api/v1/settings/time.ntp.servers')
    expect((put[1] as RequestInit).body).toBe('["time.example.com","1.pool.ntp.org"]')

    expect(await screen.findByText('Applying time.ntp.servers…')).toBeTruthy()
    taskStatus = { ...taskStatus, status: 'finished', ...{ outcome: 'succeeded' } }
    expect(await screen.findByText('Change applied.', {}, { timeout: 4_000 })).toBeTruthy()
  })

  it('writes the timezone as presentation only and keeps the device clock UTC', async () => {
    const fetch = stubFetch({
      '/api/v1/settings/time.timezone': 'Etc/UTC',
      'PUT /api/v1/settings/time.timezone': () => jsonResponse({ taskId: 'task-3' }),
      '/api/v1/tasks/task-3': () => jsonResponse({ id: 'task-3', dotPath: 'time.timezone', status: 'finished', outcome: 'succeeded', foldedCount: 0 }),
    })
    renderPanel(<TimezonePanel />)

    expect(screen.getByText(/The device clock, logs and the API stay UTC/)).toBeTruthy()
    const input = await screen.findByRole('textbox', { name: /^Timezone/ })
    await waitFor(() => expect((input as HTMLInputElement).value).toBe('Etc/UTC'))
    await userEvent.clear(input)
    await userEvent.type(input, 'Europe/Berlin')
    await userEvent.click(screen.getByRole('button', { name: 'Save timezone' }))

    await waitFor(() => expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')).toBe(true))
    const put = fetch.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')!
    expect((put[1] as RequestInit).body).toBe('"Europe/Berlin"')
    expect(await screen.findByText('Change applied.')).toBeTruthy()
  })

  it('surfaces a rejected write without clearing the draft', async () => {
    stubFetch({
      '/api/v1/settings/time.timezone': 'Etc/UTC',
      'PUT /api/v1/settings/time.timezone': () => jsonResponse({ error: { message: 'Nowhere/Nothing is not an IANA zone' } }, 400),
    })
    renderPanel(<TimezonePanel />)

    const input = await screen.findByRole('textbox', { name: /^Timezone/ })
    await waitFor(() => expect((input as HTMLInputElement).value).toBe('Etc/UTC'))
    await userEvent.clear(input)
    await userEvent.type(input, 'Nowhere/Nothing')
    await userEvent.click(screen.getByRole('button', { name: 'Save timezone' }))

    expect(await screen.findByText('Nowhere/Nothing is not an IANA zone')).toBeTruthy()
    expect((input as HTMLInputElement).value).toBe('Nowhere/Nothing')
  })
})
