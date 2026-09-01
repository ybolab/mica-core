import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import type { TimeStatus } from '@/lib/types'
import { NtpServersPanel, SyncStatusPanel, TimezonePanel } from './time'

function response(value: unknown) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

function renderPanel(panel: ReactNode) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  return render(<QueryClientProvider client={queryClient}>{panel}</QueryClientProvider>)
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('synchronization status', () => {
  it('shows the degraded state and that retries continue', async () => {
    const status: TimeStatus = { status: 'offline-degraded', synchronized: false }
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(status)))
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText(/degraded — no reachable time server/)).toBeTruthy()
    expect(screen.getByText(/keeps retrying every 30 seconds/)).toBeTruthy()
  })

  it('shows the synchronized state with its server and correction evidence', async () => {
    const status: TimeStatus = {
      status: 'synchronized',
      synchronized: true,
      server: { name: '0.pool.ntp.org', address: '192.0.2.7' },
      sample: { leap: 0, stratum: 2, spike: false, offsetSeconds: 0.012, packetCount: 5, correction: 'slew' },
    }
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(status)))
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText('synchronized with network time')).toBeTruthy()
    expect(screen.getByText('0.pool.ntp.org (192.0.2.7)')).toBeTruthy()
    expect(screen.getByText(/stratum 2/)).toBeTruthy()
    expect(screen.getByText('slewing (ordinary drift)')).toBeTruthy()
  })

  it('tells a clock step from ordinary drift', async () => {
    const status: TimeStatus = {
      status: 'synchronizing',
      server: { name: 's.example' },
      sample: { leap: 0, stratum: 3, spike: false, offsetSeconds: -3.2, packetCount: 1, correction: 'step' },
    }
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(status)))
    renderPanel(<SyncStatusPanel />)

    expect(await screen.findByText('clock stepped (large correction)')).toBeTruthy()
  })
})

describe('NTP server settings', () => {
  it('saves the whole list, one server per line, to its dot-path', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(response(['0.pool.ntp.org']))
      .mockResolvedValueOnce(response({ taskId: 't1' }))
      .mockResolvedValue(response(['0.pool.ntp.org', 'time.example.com']))
    vi.stubGlobal('fetch', fetch)
    renderPanel(<NtpServersPanel />)

    const editor = await screen.findByRole('textbox', { name: 'NTP servers' })
    await waitFor(() => expect((editor as HTMLTextAreaElement).value).toBe('0.pool.ntp.org'))
    await userEvent.click(editor)
    await userEvent.keyboard('{End}\ntime.example.com')
    await userEvent.click(screen.getByRole('button', { name: 'Save servers' }))

    await waitFor(() => expect(fetch.mock.calls.length).toBeGreaterThanOrEqual(2))
    const [url, init] = fetch.mock.calls[1] as [string, RequestInit]
    expect(url).toBe('/api/v1/settings/time.ntp.servers')
    expect(init.method).toBe('PUT')
    expect(JSON.parse(init.body as string)).toEqual(['0.pool.ntp.org', 'time.example.com'])
  })
})

describe('timezone settings', () => {
  it('saves the zone name to its dot-path', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(response('UTC'))
      .mockResolvedValueOnce(response({ taskId: 't2' }))
      .mockResolvedValue(response('Europe/Berlin'))
    vi.stubGlobal('fetch', fetch)
    renderPanel(<TimezonePanel />)

    // The accessible name is the whole wrapping label, hint included.
    const input = await screen.findByRole('textbox', { name: /^Timezone/ })
    await waitFor(() => expect((input as HTMLInputElement).value).toBe('UTC'))
    await userEvent.clear(input)
    await userEvent.type(input, 'Europe/Berlin')
    await userEvent.click(screen.getByRole('button', { name: 'Save timezone' }))

    await waitFor(() => expect(fetch.mock.calls.length).toBeGreaterThanOrEqual(2))
    const [url, init] = fetch.mock.calls[1] as [string, RequestInit]
    expect(url).toBe('/api/v1/settings/time.timezone')
    expect(init.method).toBe('PUT')
    expect(JSON.parse(init.body as string)).toBe('Europe/Berlin')
  })
})
