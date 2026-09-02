import { cleanup, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { SnapshotList } from '@/lib/types'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { DiagnosticsPanel } from './diagnostics-panel'

const retention: SnapshotList['retention'] = {
  maxSnapshots: 8,
  maxTotalBytes: 16 * 1024 * 1024,
  maxSnapshotBytes: 4 * 1024 * 1024,
  schemaVersion: 1,
  redactionSchemaVersion: 1,
}

const list: SnapshotList = {
  snapshots: [
    { id: 1, bytes: 51_200, collectedAt: '2026-09-01T08:00:00Z', machineId: '7f1c2ad0', schemaVersion: 1 },
    { id: 2, bytes: 62_464, collectedAt: '2026-09-02T08:00:00Z', machineId: '7f1c2ad0', schemaVersion: 1 },
  ],
  retention,
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('diagnostic snapshots', () => {
  it('lists the stored snapshots and the retention bounds the store enforces', async () => {
    stubFetch({ '/api/v1/diagnostics/snapshots': list })
    renderPanel(<DiagnosticsPanel />)

    expect(await screen.findByText('Snapshot 1')).toBeTruthy()
    expect(screen.getByText('Snapshot 2')).toBeTruthy()
    expect(screen.getByText('8 snapshots')).toBeTruthy()
    expect(screen.getByText('16.0 MiB total')).toBeTruthy()
    expect(screen.getByText('snapshot 1 · redaction 1')).toBeTruthy()
  })

  it('offers only the redacted export, with no in-console view of the stored bytes', async () => {
    stubFetch({ '/api/v1/diagnostics/snapshots': list })
    renderPanel(<DiagnosticsPanel />)

    const download = await screen.findByRole('link', { name: 'Download snapshot 1' })
    expect(download.getAttribute('href')).toBe('/api/v1/diagnostics/snapshots/1')
    expect(download.hasAttribute('download')).toBe(true)
    expect(screen.getByText(/Exports are already redacted by a fail-closed allowlist/)).toBeTruthy()
    expect(screen.queryByRole('button', { name: /copy/i })).toBeNull()
  })

  it('states an empty store as empty', async () => {
    stubFetch({ '/api/v1/diagnostics/snapshots': { snapshots: [], retention } satisfies SnapshotList })
    renderPanel(<DiagnosticsPanel />)

    expect(await screen.findByText('No diagnostic snapshots are stored.')).toBeTruthy()
  })

  it('reports a completed collection with its redaction counts', async () => {
    stubFetch({
      '/api/v1/diagnostics/snapshots': { snapshots: [], retention } satisfies SnapshotList,
      'POST /api/v1/diagnostics/snapshots': () => jsonResponse({
        snapshot: { id: 3, bytes: 40_960, collectedAt: '2026-09-02T09:00:00Z', machineId: '7f1c2ad0', schemaVersion: 1 },
        elapsedMillis: 1_842,
        sections: { storage: 'ok', network: 'timeout' },
        droppedFields: 12,
        redactedFields: 5,
      }, 201),
    })
    renderPanel(<DiagnosticsPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Generate snapshot' }))
    expect(await screen.findByText('Snapshot 3 collected in 1842 ms · 12 fields dropped · 5 values redacted.')).toBeTruthy()
  })

  it('says another collection is running instead of appearing to queue on 409', async () => {
    stubFetch({
      '/api/v1/diagnostics/snapshots': { snapshots: [], retention } satisfies SnapshotList,
      'POST /api/v1/diagnostics/snapshots': () => jsonResponse({ error: { code: 'diagnostics_busy', message: 'a collection is already running' } }, 409),
    })
    renderPanel(<DiagnosticsPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Generate snapshot' }))
    expect(await screen.findByText(/Another collection is already running. Nothing was queued/)).toBeTruthy()
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('reports a refused collection that is not a busy answer as an error', async () => {
    stubFetch({
      '/api/v1/diagnostics/snapshots': { snapshots: [], retention } satisfies SnapshotList,
      'POST /api/v1/diagnostics/snapshots': () => jsonResponse({ error: { code: 'diagnostics_io', message: 'the store is unwritable; nothing was written' } }, 500),
    })
    renderPanel(<DiagnosticsPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Generate snapshot' }))
    expect(await screen.findByText('the store is unwritable; nothing was written')).toBeTruthy()
  })

  it('deletes a snapshot only after an explicit confirmation', async () => {
    const fetch = stubFetch({
      '/api/v1/diagnostics/snapshots': list,
      'DELETE /api/v1/diagnostics/snapshots/1': () => jsonResponse(undefined, 204),
    })
    renderPanel(<DiagnosticsPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Delete snapshot 1' }))
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'DELETE')).toBe(false)
    await userEvent.click(await screen.findByRole('button', { name: 'Delete' }))

    await waitFor(() => expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'DELETE')).toBe(true))
    const remove = fetch.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'DELETE')!
    expect(remove[0]).toBe('/api/v1/diagnostics/snapshots/1')
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/diagnostics/snapshots': () => jsonResponse({ error: { message: 'the store could not be read' } }, 500) })
    renderPanel(<DiagnosticsPanel />)

    expect(await screen.findByText('the store could not be read')).toBeTruthy()
  })
})
