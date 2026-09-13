import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ObservedNetworkState } from '@/lib/types'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { ObservedNetworkPanel } from './observed-network-panel'

function observed(overrides: Partial<ObservedNetworkState> = {}): ObservedNetworkState {
  return {
    interfaces: {
      available: true,
      count: 1,
      entries: [{
        name: 'eth0',
        index: 2,
        kind: 'ether',
        driver: 'rk_gmac-dwmac',
        mtu: 1500,
        link: { operationalState: 'routable', carrierState: 'carrier', carrier: true, onlineState: 'online' },
        addresses: [{ family: 'inet', address: '192.168.1.20', prefixLength: 24, configSource: 'DHCPv4' }],
        dhcp: { available: true, state: 'bound', lease: { address: '192.168.1.20', server: '192.168.1.1', router: '192.168.1.1', lifetimeSeconds: 3600 } },
        dns: ['192.168.1.1'],
      }],
    },
    defaultRoutes: {
      available: true,
      count: 1,
      entries: [{ family: 'inet', gateway: '192.168.1.1', interface: 'eth0', metric: 100, protocol: 'dhcp', protocolId: 16, table: 'main', tableId: 254, configSource: 'DHCPv4' }],
    },
    dns: {
      available: true,
      linkServers: ['192.168.1.1'],
      resolverServers: ['127.0.0.53'],
      probe: { name: 'deb.debian.org', reachable: true, result: 'resolved' },
    },
    wifi: { available: true, associations: [] },
    capabilities: {
      wifi: { supported: false, interfaces: [] },
      bluetooth: { supported: false, adapters: [] },
      cellular: { supported: false, interfaces: [] },
    },
    ...overrides,
  }
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('observed network state', () => {
  it('separates what the device sees from what it is configured to do', async () => {
    stubFetch({ '/api/v1/network/status': observed() })
    renderPanel(<ObservedNetworkPanel />)

    expect(screen.getByText(/Observed state only\. This tab shows what the device sees/)).toBeTruthy()
    expect(await screen.findAllByText('eth0')).not.toHaveLength(0)
    expect(screen.getByText(/Configuration is not used to infer these values/)).toBeTruthy()
  })

  it('renders link state, addresses with their source, the lease, routes and the DNS probe', async () => {
    stubFetch({ '/api/v1/network/status': observed() })
    renderPanel(<ObservedNetworkPanel />)

    expect(await screen.findByText('routable · carrier carrier · online')).toBeTruthy()
    expect(screen.getByText('ether · rk_gmac-dwmac · MTU 1500')).toBeTruthy()
    expect(screen.getByText('192.168.1.20/24 · DHCPv4')).toBeTruthy()
    expect(screen.getByText('bound · server 192.168.1.1 · router 192.168.1.1 · lease 3600s')).toBeTruthy()
    expect(screen.getByText('192.168.1.1 · inet · metric 100 · DHCPv4')).toBeTruthy()
    expect(screen.getByText('resolved')).toBeTruthy()
  })

  it('reports cellular as explicitly unsupported rather than hiding it', async () => {
    stubFetch({ '/api/v1/network/status': observed() })
    renderPanel(<ObservedNetworkPanel />)

    expect(await screen.findByText('Cellular')).toBeTruthy()
    expect(screen.getAllByText('unsupported')).toHaveLength(3)
  })

  it('renders an absent observation as absence with its reason', async () => {
    stubFetch({
      '/api/v1/network/status': observed({
        dns: { available: false, detail: 'systemd-resolved is not reachable on the bus' },
        wifi: { available: false, detail: 'wpa_supplicant is not running' },
        interfaces: { available: false, detail: 'systemd-networkd is not reachable on the bus' },
      }),
    })
    renderPanel(<ObservedNetworkPanel />)

    expect(await screen.findByText('Unavailable — systemd-resolved is not reachable on the bus')).toBeTruthy()
    expect(screen.getByText('Unavailable — wpa_supplicant is not running')).toBeTruthy()
    expect(screen.getByText('Unavailable — systemd-networkd is not reachable on the bus')).toBeTruthy()
  })

  it('reports an observed but empty result as empty, not as unavailable', async () => {
    stubFetch({
      '/api/v1/network/status': observed({
        interfaces: { available: true, count: 0, entries: [] },
        defaultRoutes: { available: true, count: 0, entries: [] },
      }),
    })
    renderPanel(<ObservedNetworkPanel />)

    expect(await screen.findByText('No interfaces were observed.')).toBeTruthy()
    expect(screen.getByText('No default route')).toBeTruthy()
    expect(screen.getByText('No Wi-Fi associations were reported.')).toBeTruthy()
    expect(screen.queryByText(/^Unavailable/)).toBeNull()
  })

  it('reports a degraded link and an unavailable per-interface lease side by side', async () => {
    stubFetch({
      '/api/v1/network/status': observed({
        interfaces: {
          available: true,
          count: 1,
          entries: [{
            name: 'eth0',
            link: { operationalState: 'degraded', carrierState: 'no-carrier', carrier: false },
            addresses: [],
            dhcp: { available: false, detail: 'this link runs no DHCP client' },
            dns: [],
            wifi: { available: false, detail: 'this link is not a wireless device' },
          }],
        },
      }),
    })
    renderPanel(<ObservedNetworkPanel />)

    expect(await screen.findByText('degraded · carrier no-carrier')).toBeTruthy()
    expect(screen.getByText('Unavailable — this link runs no DHCP client')).toBeTruthy()
    expect(screen.getByText('Unavailable — this link is not a wireless device')).toBeTruthy()
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/network/status': () => jsonResponse({ error: { message: 'micad is not reachable' } }, 503) })
    renderPanel(<ObservedNetworkPanel />)

    expect(await screen.findByText('micad is not reachable')).toBeTruthy()
  })
})
