import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderRoute, stubFetch } from '@/shared/testing/panel'
import { SimulationProvider } from '@/shared/simulation/simulation-provider'
import { ApplicationsPage } from './applications-page'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function renderPage() {
  return renderRoute(<SimulationProvider><ApplicationsPage /></SimulationProvider>)
}

describe('the applications page', () => {
  it('offers a way out of the install wizard at every step', async () => {
    // The wizard suppressed its own close button and offered no cancel on the
    // first three steps, so the only exit was Escape or the backdrop.
    stubFetch({ '/api/v1/settings/container.enabled': true })
    renderPage()

    await userEvent.click(await screen.findByRole('tab', { name: 'Catalog' }))
    const card = (await screen.findAllByText('MQTT Bridge'))[0].closest('[data-slot=card]')!
    await userEvent.click(within(card as HTMLElement).getByRole('button', { name: 'Install' }))

    const dialog = await screen.findByRole('dialog')
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toBeTruthy()
    await userEvent.click(within(dialog).getByRole('button', { name: 'Next' }))
    expect(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Cancel' })).toBeTruthy()
  })

  it('closes the removal confirmation and reports which application went', async () => {
    stubFetch({ '/api/v1/settings/container.enabled': true })
    renderPage()

    await userEvent.click(await screen.findByRole('button', { name: 'Remove Node-RED?' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Remove' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect((await screen.findAllByText('Node-RED removed.')).length).toBeGreaterThan(0)
  })

  it('reports a start and a stop, which used to change only the row', async () => {
    stubFetch({ '/api/v1/settings/container.enabled': true })
    renderPage()

    const row = (await screen.findAllByRole('row')).find((candidate) => candidate.textContent?.includes('Node-RED'))!
    await userEvent.click(within(row).getByRole('button', { name: 'Stop' }))

    expect((await screen.findAllByText('Node-RED stopped.')).length).toBeGreaterThan(0)
  })

  it('routes to the container runtime when it is the thing blocking installs', async () => {
    stubFetch({ '/api/v1/settings/container.enabled': false })
    renderPage()

    expect(await screen.findByText('The container runtime is disabled')).toBeTruthy()
    expect((await screen.findByRole('link', { name: 'Open Services' })).getAttribute('href')).toContain('/services/containers')
  })

  it('filters the installed list by the search box', async () => {
    stubFetch({ '/api/v1/settings/container.enabled': true })
    renderPage()

    await userEvent.type(await screen.findByRole('textbox', { name: 'Search applications' }), 'modbus')

    await waitFor(() => expect(screen.queryByText('Node-RED')).toBeNull())
    expect(screen.getByText('Modbus Gateway')).toBeTruthy()
  })
})
