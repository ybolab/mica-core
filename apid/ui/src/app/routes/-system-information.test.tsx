import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import type { SystemInformation } from '@/lib/types'
import { i18n } from '@/i18n/i18n'
import { SystemInformationPage } from './system-information'

function response(value: SystemInformation) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

function renderPage(value: SystemInformation) {
  const fetch = vi.fn().mockResolvedValue(response(value))
  vi.stubGlobal('fetch', fetch)
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}><SystemInformationPage /></QueryClientProvider>
    </I18nextProvider>,
  )
  return fetch
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('system information', () => {
  it('answers device identity with one API read', async () => {
    const fetch = renderPage({
      machineId: { available: true, id: '0123456789abcdef0123456789abcdef' },
      board: { available: true, model: 'cx3576', source: 'devicetree' },
      kernel: { available: true, release: '6.12.1-mos', version: '#1 SMP' },
      release: { available: true, prettyName: 'mos 1.4', imageVersion: '1.4.0' },
      system: {
        available: true,
        version: '1.4.0',
        package: 'mos-system',
        buildDate: '2026-09-01T10:00:00Z',
        gitStamp: { commit: 'abcdef0', dirty: false, revision: 3, consistent: true, stamps: ['abcdef0-3'] },
      },
      daemon: { available: true, name: 'mosd', version: '1.4.0', commit: 'abcdef0' },
      packages: {
        available: true,
        count: 2,
        mosCount: 1,
        malformedRows: 0,
        truncated: false,
        entries: [
          { name: 'mosd', version: '1.4.0', architecture: 'aarch64', mos: true },
          { name: 'systemd', version: '258', architecture: 'aarch64', mos: false },
        ],
      },
      slot: { available: true, booted: 'rootfs.0', bootname: 'A', bundleVersion: '1.4.0', bootStatus: 'good', primary: true },
      uptime: { available: true, seconds: 90061 },
    })

    expect(await screen.findByText('cx3576 · devicetree')).toBeTruthy()
    expect(screen.getByText('0123456789abcdef0123456789abcdef')).toBeTruthy()
    expect(screen.getByText('1d 1h 1m')).toBeTruthy()
    expect(screen.getByText('rootfs.0 · A · good · primary')).toBeTruthy()
    expect(screen.getByText('mosd', { selector: 'td' })).toBeTruthy()
    expect(screen.getByText('1.4.0', { selector: 'td' })).toBeTruthy()
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(1))
    expect(fetch).toHaveBeenCalledWith('/api/v1/system/info', expect.anything())
  })

  it('names unavailable identity evidence instead of showing a healthy blank', async () => {
    renderPage({
      machineId: { available: false, detail: 'machine-id is unreadable' },
      board: { available: false, detail: 'no board identity source' },
      kernel: { available: false, detail: 'kernel information unavailable' },
      release: { available: false, detail: 'os-release is unreadable' },
      system: { available: false, detail: 'manifest missing' },
      daemon: { available: false, detail: 'daemon version unavailable' },
      packages: { available: false, detail: 'manifest missing' },
      slot: { available: false, detail: 'RAUC did not answer' },
      uptime: { available: false, detail: 'procfs unavailable' },
    })

    expect(await screen.findAllByText('Unavailable')).toHaveLength(9)
    expect(screen.getAllByText('manifest missing')).toHaveLength(2)
    expect(screen.getByText('RAUC did not answer')).toBeTruthy()
  })
})
