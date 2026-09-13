import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderRoute, stubFetch } from '@/shared/testing/panel'
import { NetworkPage } from './network-page'

const routes = {
  '/api/v1/network': {
    configured: { eth0: { dhcp: true }, wg0: { kind: 'wireguard', dhcp: false, static: { address: '10.10.0.2/24', dns: [] }, wireguard: { listenPort: 51820, peers: [] } } },
    configuredCount: 2,
    observed: { available: true, interfaceCount: 2, interfaces: [{ index: 2, name: 'eth0', kind: 'ether', operationalState: 'routable', addresses: ['192.168.1.24/24'] }] },
  },
  '/api/v1/network/status': {
    interfaces: { available: true, count: 0, entries: [] },
    defaultRoutes: { available: true, count: 0, entries: [] },
    dns: { available: true, linkServers: [], resolverServers: [] },
    wifi: { available: true, associations: [] },
    capabilities: { wifi: { supported: false, interfaces: [] }, bluetooth: { supported: false, adapters: [] }, cellular: { supported: false, interfaces: [] } },
  },
  '/api/v1/settings/wifi.client': { enabled: true, interface: 'wlan0', networks: [] },
  '/api/v1/wifi/client/networks': [{ ssid: 'workshop', psk: 'x', hidden: false, priority: 10 }],
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the network page', () => {
  it('gives each interface one link and no second navigation behind it', async () => {
    // The row used to carry an onClick to the same route as the link it
    // contained, so a click on the name navigated twice and the keyboard
    // reached neither.
    stubFetch(routes)
    renderRoute(<NetworkPage />)

    const link = await screen.findByRole('link', { name: /eth0/ })
    expect(link.getAttribute('href')).toContain('/network/eth0')
    const row = link.closest('tr')!
    expect(within(row).getAllByRole('link')).toHaveLength(1)
  })

  it('closes the Wi-Fi removal confirmation and names the network', async () => {
    stubFetch({ ...routes, 'DELETE /api/v1/wifi/client/networks/workshop': () => new Response(null, { status: 204 }) })
    renderRoute(<NetworkPage />)

    await userEvent.click(await screen.findByRole('tab', { name: 'Known Wi-Fi' }))
    await userEvent.click(await screen.findByRole('button', { name: 'Remove workshop' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Remove workshop' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect((await screen.findAllByText('Network workshop removed.')).length).toBeGreaterThan(0)
  })

  it('reports the Wi-Fi client switch, which used to change nothing visible', async () => {
    stubFetch({ ...routes, 'PUT /api/v1/settings/wifi.client.enabled': () => jsonResponse({ taskId: 'task-1' }, 202) })
    renderRoute(<NetworkPage />)

    await userEvent.click(await screen.findByRole('tab', { name: 'Known Wi-Fi' }))
    await userEvent.click(await screen.findByRole('switch', { name: 'Wi-Fi client' }))

    expect((await screen.findAllByText('Wi-Fi client disabled.')).length).toBeGreaterThan(0)
  })

  it('keeps the add-interface dialog reachable when static addressing expands it', async () => {
    stubFetch(routes)
    renderRoute(<NetworkPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Add interface' }))
    await userEvent.click(await screen.findByRole('switch', { name: 'Use DHCP' }))

    const dialog = await screen.findByRole('dialog')
    // Header and footer stay outside the scroll region, so both are present
    // however tall the body grows.
    expect(within(dialog).getByText('Add network interface')).toBeTruthy()
    expect(within(dialog).getByRole('button', { name: 'Add' })).toBeTruthy()
    expect(within(dialog).getByRole('button', { name: 'Cancel' })).toBeTruthy()
    expect(within(dialog).getByLabelText('Address')).toBeTruthy()
  })
})

describe('the add-interface dialog', () => {
  it.each([
    ['VLAN', ['Parent interface', 'VLAN ID']],
    ['Bridge', ['Bridge ports']],
    ['WireGuard', ['Listen port']],
  ])('asks only for what a %s interface needs', async (kind, fields) => {
    stubFetch(routes)
    renderRoute(<NetworkPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Add interface' }))
    const dialog = await screen.findByRole('dialog')
    await userEvent.click(within(dialog).getByRole('combobox', { name: 'Interface kind' }))
    await userEvent.click(await screen.findByRole('option', { name: kind }))

    for (const field of fields) {
      expect(within(await screen.findByRole('dialog')).getByLabelText(field)).toBeTruthy()
    }
  })

  it('sends the typed interface and reports it', async () => {
    const fetch = stubFetch({ ...routes, 'PUT /api/v1/network/eth1': () => jsonResponse({ taskId: 'task-1' }, 202) })
    renderRoute(<NetworkPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Add interface' }))
    const dialog = await screen.findByRole('dialog')
    await userEvent.type(within(dialog).getByLabelText('Interface name'), 'eth1')
    await userEvent.click(within(dialog).getByRole('button', { name: 'Add' }))

    expect((await screen.findAllByText('Interface eth1 saved.')).length).toBeGreaterThan(0)
    const put = fetch.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')!
    expect(JSON.parse(String((put[1] as RequestInit).body))).toEqual({ dhcp: true })
  })
})
