import { cleanup, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, renderRoute, stubFetch } from '@/shared/testing/panel'
import { SimulationProvider } from '@/shared/simulation/simulation-provider'
import { SystemPage, UiPanel, UpdateActions } from './system-page'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

const updateReady = {
  '/api/v1/update': { lifecycle: { state: 'ready', deploymentId: '9e12aa77', available: { deploymentId: '9e12aa77', version: '2026.09.0', channel: 'stable' } } },
}

describe('update actions', () => {
  it('reports each dispatched action, which used to change nothing visible', async () => {
    stubFetch({ ...updateReady, 'POST /api/v1/update/check': () => new Response(null, { status: 202 }) })
    renderPanel(<UpdateActions />)

    await userEvent.click(await screen.findByRole('button', { name: 'Check now' }))

    expect((await screen.findAllByText('Update check started.')).length).toBeGreaterThan(0)
  })

  it('names the device refusal rather than a generic failure line', async () => {
    stubFetch({
      ...updateReady,
      'POST /api/v1/update/fetch': () => jsonResponse({ error: { code: 'no_source', message: 'this device has no update source' } }, 409),
    })
    renderPanel(<UpdateActions />)

    await userEvent.click(await screen.findByRole('button', { name: 'Download' }))

    expect((await screen.findAllByText('this device has no update source')).length).toBeGreaterThan(0)
  })
})

describe('the custom UI switch', () => {
  it('reports the selection it made', async () => {
    stubFetch({
      '/api/v1/ui': { mode: 'builtIn', availableCustom: { generation: 3, name: 'kiosk', version: '1.2.0', usable: true, indexReadable: true, digestMatches: true } },
      'PUT /api/v1/ui/active': { mode: 'custom', custom: { generation: 3, name: 'kiosk', version: '1.2.0', usable: true, indexReadable: true, digestMatches: true } },
    })
    renderPanel(<UiPanel />)

    await userEvent.click(await screen.findByRole('switch', { name: 'Use custom UI at root' }))

    expect((await screen.findAllByText('The custom interface is active.')).length).toBeGreaterThan(0)
  })

  it('refuses a bundle the device could not validate, and says why', async () => {
    stubFetch({
      '/api/v1/ui': { mode: 'builtIn', availableCustom: { generation: 3, usable: false, unavailableReason: 'digestMismatch', indexReadable: true } },
    })
    renderPanel(<UiPanel />)

    const control = await screen.findByRole('switch', { name: 'Use custom UI at root' })
    await waitFor(() => expect(control.getAttribute('disabled') !== null || control.getAttribute('data-disabled') !== null).toBe(true))
    expect(await screen.findByText(/its files changed after activation/)).toBeTruthy()
  })
})

describe('the hostname form', () => {
  it('reports the save and falls back to the value the device confirmed', async () => {
    // The draft used to survive a successful save, so the field kept showing
    // what this browser typed rather than what the device accepted.
    let current = 'mos-cm4'
    stubFetch({
      '/api/v1/settings/hostname': () => jsonResponse(current),
      'PUT /api/v1/settings/hostname': () => { current = 'workshop-01'; return jsonResponse({ taskId: 'task-1' }, 202) },
      '/api/v1/tasks/task-1': { id: 'task-1', operation: 'set', dotPath: 'hostname', source: 'api', status: 'finished', outcome: 'succeeded', enqueuedAt: new Date().toISOString(), foldedCount: 0 },
    })
    renderRoute(<SimulationProvider><SystemPage /></SimulationProvider>)

    const field = await screen.findByLabelText('Hostname')
    await userEvent.clear(field)
    await userEvent.type(field, 'workshop-01')
    await userEvent.click(screen.getByRole('button', { name: 'Save hostname' }))

    expect((await screen.findAllByText('Hostname saved.')).length).toBeGreaterThan(0)
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save hostname' }).hasAttribute('disabled')).toBe(true))
  })
})

describe('the rest of the system tab', () => {
  it('accepts a manual deployment id only in its exact shape', async () => {
    const id = 'a'.repeat(64)
    const fetch = stubFetch({
      ...updateReady,
      '/api/v1/provisioning/status': { unclaimed: false },
      'POST /api/v1/update/install': () => new Response(null, { status: 202 }),
    })
    renderRoute(<SimulationProvider><SystemPage /></SimulationProvider>)

    await userEvent.click(await screen.findByRole('tab', { name: /Update/ }))
    const field = await screen.findByLabelText('Deployment ID')
    await userEvent.type(field, 'not-a-digest')
    const submit = screen.getAllByRole('button', { name: 'Install update' }).at(-1)!
    expect(submit.hasAttribute('disabled')).toBe(true)

    await userEvent.clear(field)
    await userEvent.type(field, id)
    await userEvent.click(screen.getAllByRole('button', { name: 'Install update' }).at(-1)!)

    expect((await screen.findAllByText('Installation accepted. Follow its progress above.')).length).toBeGreaterThan(0)
    expect(fetch.mock.calls.some(([url]) => String(url) === '/api/v1/update/install')).toBe(true)
  })

  it('names the backup and support surfaces as planned rather than offering them', async () => {
    stubFetch({ ...updateReady, '/api/v1/provisioning/status': { unclaimed: false }, '/api/v1/diagnostics/snapshots': { retention: { maxSnapshots: 5, maxTotalBytes: 1, maxSnapshotBytes: 1, schemaVersion: 2, redactionSchemaVersion: 1 }, snapshots: [] } })
    renderRoute(<SimulationProvider><SystemPage /></SimulationProvider>)

    await userEvent.click(await screen.findByRole('tab', { name: /Update/ }))
    expect(screen.getByRole('button', { name: /Download .mosbak/ }).hasAttribute('disabled')).toBe(true)

    await userEvent.click(screen.getByRole('tab', { name: /Diagnostics/ }))
    expect(await screen.findByRole('switch', { name: 'Enable support access' })).toBeTruthy()
  })
})
