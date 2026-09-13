import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderRoute, stubFetch } from '@/shared/testing/panel'
import { NetworkPage } from './network-page'

const peer = { publicKey: 'abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG=', allowedIps: ['10.10.0.0/24'], endpoint: 'vpn.example.com:51820', persistentKeepalive: 25 }

const routes = {
  '/api/v1/network': {
    configured: { wg0: { kind: 'wireguard', dhcp: false, static: { address: '10.10.0.2/24', dns: [] }, wireguard: { listenPort: 51820, peers: [] } } },
    configuredCount: 1,
    observed: { available: true, interfaceCount: 0, interfaces: [] },
  },
  '/api/v1/network/status': {
    interfaces: { available: true, count: 0, entries: [] },
    defaultRoutes: { available: true, count: 0, entries: [] },
    dns: { available: true, linkServers: [], resolverServers: [] },
    wifi: { available: true, associations: [] },
    capabilities: { wifi: { supported: false, interfaces: [] }, bluetooth: { supported: false, adapters: [] }, cellular: { supported: false, interfaces: [] } },
  },
  '/api/v1/settings/wifi.client': { enabled: false, interface: 'wlan0', networks: [] },
  '/api/v1/wifi/client/networks': [],
  '/api/v1/network/wg0/peers': [peer],
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

async function openTunnel() {
  renderRoute(<NetworkPage />)
  await userEvent.click(await screen.findByRole('tab', { name: 'WireGuard' }))
}

describe('the WireGuard tunnel', () => {
  it('reads the tunnel, its listen port and its peers', async () => {
    stubFetch(routes)
    await openTunnel()

    expect(await screen.findByText('wg0 · 10.10.0.2/24')).toBeTruthy()
    expect(await screen.findByText('51820/udp')).toBeTruthy()
    expect(await screen.findByText(peer.publicKey)).toBeTruthy()
  })

  it('closes the peer removal confirmation and reports it', async () => {
    stubFetch({ ...routes, [`DELETE /api/v1/network/wg0/peers/${encodeURIComponent(peer.publicKey)}`]: () => new Response(null, { status: 204 }) })
    await openTunnel()

    await userEvent.click(await screen.findByRole('button', { name: 'Remove peer' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Remove peer' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect((await screen.findAllByText('Peer removed.')).length).toBeGreaterThan(0)
  })

  /// The rotated key is shown once, in the panel, because it is the only copy
  /// the operator will ever see; the toast only says the rotation happened.
  it('shows the new public key after a rotation and reports the rotation', async () => {
    stubFetch({ ...routes, 'POST /api/v1/actions/wireguard/wg0/rotate-key': { publicKey: 'rotated-public-key=' } })
    await openTunnel()

    await userEvent.click(await screen.findByRole('button', { name: 'Rotate key' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Rotate key' }))

    expect(await screen.findByText('rotated-public-key=')).toBeTruthy()
    expect((await screen.findAllByText('The tunnel key was rotated.')).length).toBeGreaterThan(0)
  })

  it('adds a peer through the shared form dialog and reports it', async () => {
    stubFetch({ ...routes, 'POST /api/v1/network/wg0/peers': peer })
    await openTunnel()

    await userEvent.click(await screen.findByRole('button', { name: 'Add peer' }))
    const dialog = await screen.findByRole('dialog')
    await userEvent.type(within(dialog).getByLabelText('Public key'), 'newkey=')
    await userEvent.type(within(dialog).getByLabelText('Allowed IPs'), '10.20.0.0/24')
    await userEvent.click(within(dialog).getByRole('button', { name: 'Add' }))

    expect((await screen.findAllByText('Peer added.')).length).toBeGreaterThan(0)
  })
})
