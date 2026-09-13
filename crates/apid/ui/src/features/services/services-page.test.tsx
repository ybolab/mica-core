import { cleanup, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderRoute, stubFetch } from '@/shared/testing/panel'
import { SimulationProvider } from '@/shared/simulation/simulation-provider'
import { ServicesPage } from './services-page'

const routes = {
  '/api/v1/settings/container.enabled': true,
  '/api/v1/settings/mqtt.enabled': true,
  '/api/v1/state/container': { state: 'running' },
  '/api/v1/state/mqtt': { state: 'running' },
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function renderPage() {
  return renderRoute(<SimulationProvider><ServicesPage /></SimulationProvider>)
}

describe('the services page', () => {
  it('reports a service switch, which used to change nothing visible', async () => {
    stubFetch({ ...routes, 'PUT /api/v1/settings/container.enabled': () => jsonResponse({ taskId: 'task-1' }, 202) })
    renderPage()

    await userEvent.click(await screen.findByRole('switch', { name: 'Container runtime' }))

    expect((await screen.findAllByText('Container runtime disabled.')).length).toBeGreaterThan(0)
  })

  it('names the device refusal rather than a generic failure', async () => {
    stubFetch({
      ...routes,
      'PUT /api/v1/settings/container.enabled': () => jsonResponse({ error: { code: 'busy', message: 'a container is still shutting down' } }, 409),
    })
    renderPage()

    await userEvent.click(await screen.findByRole('switch', { name: 'Container runtime' }))

    expect((await screen.findAllByText('a container is still shutting down')).length).toBeGreaterThan(0)
  })

  it('shows the observed state beside the desired one', async () => {
    stubFetch(routes)
    renderPage()

    expect((await screen.findAllByText('running')).length).toBeGreaterThan(0)
    expect((await screen.findAllByText('enabled in the desired configuration')).length).toBeGreaterThan(0)
  })
})
