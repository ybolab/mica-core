import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import type { SnapshotList } from '@/lib/types'
import { i18n } from '@/i18n/i18n'
import { DiagnosticsPage } from './diagnostics'

function response(value: unknown, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'content-type': 'application/json' },
  })
}

const snapshots: SnapshotList = {
  retention: {
    maxSnapshots: 8,
    maxTotalBytes: 16 * 1024 * 1024,
    maxSnapshotBytes: 2 * 1024 * 1024,
    schemaVersion: 1,
    redactionSchemaVersion: 1,
  },
  snapshots: [{ id: 7, bytes: 4096, collectedAt: '2026-09-01T10:00:00Z', machineId: '0123456789abcdef0123456789abcdef', schemaVersion: 1 }],
}

function renderPage(fetch: ReturnType<typeof vi.fn>) {
  vi.stubGlobal('fetch', fetch)
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } })
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}><DiagnosticsPage /></QueryClientProvider>
    </I18nextProvider>,
  )
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('diagnostic snapshots', () => {
  it('shows retention bounds and authenticated snapshot actions', async () => {
    const fetch = vi.fn().mockResolvedValue(response(snapshots))
    renderPage(fetch)

    expect(await screen.findByText('Snapshot 7')).toBeTruthy()
    expect(screen.getByText('8 snapshots')).toBeTruthy()
    expect(screen.getByText('16.0 MiB total')).toBeTruthy()
    expect(screen.getByText('2.0 MiB each')).toBeTruthy()
    expect(screen.getByRole('link', { name: 'Download snapshot 7' }).getAttribute('href')).toBe('/api/v1/diagnostics/snapshots/7')
    expect(screen.getByRole('button', { name: 'Delete snapshot 7' })).toBeTruthy()
  })

  it('generates a snapshot and refreshes the list', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(response({ ...snapshots, snapshots: [] }))
      .mockResolvedValueOnce(response({
        snapshot: snapshots.snapshots[0],
        elapsedMillis: 240,
        sections: { system: 'ok', network: 'ok' },
        droppedFields: 2,
        redactedFields: 1,
      }, 201))
      .mockResolvedValue(response(snapshots))
    renderPage(fetch)

    await userEvent.click(await screen.findByRole('button', { name: 'Generate snapshot' }))
    await waitFor(() => expect(fetch.mock.calls.length).toBeGreaterThanOrEqual(3))
    expect(fetch.mock.calls[1]?.[0]).toBe('/api/v1/diagnostics/snapshots')
    expect((fetch.mock.calls[1]?.[1] as RequestInit).method).toBe('POST')
    expect(await screen.findByText(/Snapshot 7 collected in 240 ms/)).toBeTruthy()
  })

  it('deletes a snapshot and refreshes the list', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(response(snapshots))
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
      .mockResolvedValue(response({ ...snapshots, snapshots: [] }))
    renderPage(fetch)

    await userEvent.click(await screen.findByRole('button', { name: 'Delete snapshot 7' }))
    await waitFor(() => expect(fetch.mock.calls.length).toBeGreaterThanOrEqual(3))
    expect(fetch.mock.calls[1]?.[0]).toBe('/api/v1/diagnostics/snapshots/7')
    expect((fetch.mock.calls[1]?.[1] as RequestInit).method).toBe('DELETE')
  })
})
