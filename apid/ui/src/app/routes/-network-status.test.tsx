import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import type { ObservedNetworkState } from '@/lib/types'
import { i18n } from '@/i18n/i18n'
import { ObservedNetworkPage } from './network-status'

function response(value: ObservedNetworkState) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('observed network state', () => {
  it('shows live evidence separately from desired network settings', async () => {
    const value: ObservedNetworkState = {
      interfaces: {
        available: true,
        count: 1,
        entries: [{
          name: 'eth0',
          index: 2,
          kind: 'ether',
          type: 'ether',
          driver: 'igc',
          mtu: 1500,
          link: { administrativeState: 'configured', operationalState: 'no-carrier', carrierState: 'off', carrier: false, onlineState: 'offline', addressState: 'degraded' },
          addresses: [{ family: 'ipv4', address: '192.0.2.10', prefixLength: 24, scope: 'global', configSource: 'DHCPv4' }],
          dhcp: { available: true, state: 'bound', lease: { address: '192.0.2.10', prefixLength: 24, server: '192.0.2.1', router: '192.0.2.1', lifetimeSeconds: 3600 } },
          dns: ['192.0.2.53'],
          wifi: { available: false, detail: 'not a wireless interface' },
        }],
      },
      defaultRoutes: { available: true, count: 0, entries: [] },
      dns: {
        available: true,
        linkServers: ['192.0.2.53'],
        resolverServers: ['192.0.2.53'],
        probe: { name: '0.debian.pool.ntp.org', reachable: false, result: 'timeout', detail: 'resolver did not answer' },
      },
      wifi: {
        available: true,
        associations: [{ interface: 'wlan0', state: 'completed', associated: true, ssid: '<redacted>', rssiDbm: -42, linkSpeedMbps: 144 }],
      },
      capabilities: {
        wifi: { supported: false, interfaces: [] },
        bluetooth: { supported: false, adapters: [] },
        cellular: { supported: false, interfaces: [] },
      },
    }
    const fetch = vi.fn().mockResolvedValue(response(value))
    vi.stubGlobal('fetch', fetch)
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } })
    render(
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}><ObservedNetworkPage /></QueryClientProvider>
      </I18nextProvider>,
    )

    expect(await screen.findByText('eth0')).toBeTruthy()
    expect(screen.getByText(/Observed state only/)).toBeTruthy()
    expect(screen.getByRole('link', { name: 'Open desired network settings' }).getAttribute('href')).toBe('/_ui/network')
    expect(screen.getByText('no-carrier · carrier off · offline')).toBeTruthy()
    expect(screen.getByText('192.0.2.10/24 · DHCPv4')).toBeTruthy()
    expect(screen.getByText('No default route')).toBeTruthy()
    expect(screen.getByText('timeout · resolver did not answer')).toBeTruthy()
    expect(screen.getByText('completed · associated · <redacted> · -42 dBm · 144 Mbps')).toBeTruthy()
    expect(screen.getAllByText('unsupported')).toHaveLength(3)
    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(1))
    expect(fetch).toHaveBeenCalledWith('/api/v1/network/status', expect.anything())
  })
})
