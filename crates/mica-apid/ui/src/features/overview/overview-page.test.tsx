import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderRoute, stubFetch } from '@/shared/testing/panel'
import { OverviewPage } from './overview-page'

const FIVE_MINUTES_AGO = new Date(Date.now() - 5 * 60_000).toISOString()

const routes = {
  '/api/v1/health': { apid: 'ok', micad: 'ok', checkedAt: 183_900 },
  '/api/v1/system/info': {
    machineId: { available: true, id: '4f2e9c1a7b3d4e5f' },
    uptime: { available: true, seconds: 1_231_932 },
  },
  '/api/v1/network': { configured: {}, configuredCount: 0, observed: { available: true, interfaceCount: 1, interfaces: [{ index: 2, name: 'eth0', operationalState: 'routable', addresses: ['192.168.1.24/24'] }] } },
  '/api/v1/settings/hostname': 'mica-cm4',
  '/api/v1/tasks': [{ id: 'task-1', operation: 'set', dotPath: 'hostname', source: 'api', status: 'finished', outcome: 'succeeded', enqueuedAt: FIVE_MINUTES_AGO, foldedCount: 0 }],
  '/api/v1/update': { lifecycle: { state: 'ready', available: { name: 'mica', version: '2026.09.0' } } },
  '/api/v1/time/status': { status: 'synchronized', synchronized: true },
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the overview', () => {
  it('reads the device into four headline metrics and the recent tasks', async () => {
    stubFetch(routes)
    renderRoute(<OverviewPage />)

    expect(await screen.findByText('mica-cm4')).toBeTruthy()
    expect(await screen.findByText(/eth0/)).toBeTruthy()
    expect(await screen.findByText('14d 6h')).toBeTruthy()
    expect(await screen.findByText('hostname')).toBeTruthy()
  })

  it('raises a reported update into the attention list with a route to it', async () => {
    stubFetch(routes)
    renderRoute(<OverviewPage />)

    expect(await screen.findByText('System update 2026.09.0 is ready to install')).toBeTruthy()
    expect((await screen.findByRole('link', { name: 'Open Update' })).getAttribute('href')).toContain('/system')
  })

  it('names an unsynchronized clock as needing attention', async () => {
    stubFetch({ ...routes, '/api/v1/time/status': { status: 'unsynchronized', synchronized: false } })
    renderRoute(<OverviewPage />)

    expect(await screen.findByText('System clock is not synchronized')).toBeTruthy()
  })

  it('says the read failed rather than showing an empty console', async () => {
    stubFetch({ ...routes, '/api/v1/health': () => jsonResponse({ error: { message: 'micad is not answering' } }, 503) })
    renderRoute(<OverviewPage />)

    expect(await screen.findByText('The request could not be completed.')).toBeTruthy()
  })
})
