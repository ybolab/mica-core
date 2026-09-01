import { createFileRoute } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { Cable, RefreshCw } from 'lucide-react'
import { api } from '@/lib/api'
import { configuredSummary, networkRows, type NetworkRow } from '@/lib/network'
import type { NetworkOverview } from '@/lib/types'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'

function NetworkPage() {
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const observed = network.data?.observed
  const rows = networkRows(network.data?.configured, observed?.interfaces)
  return (
    <div className="page">
      <header className="page-head">
        <div><p className="eyebrow">Connectivity</p><h1>Network</h1><p>Declared configuration and the links actually visible to systemd-networkd.</p></div>
        <Button variant="secondary" onClick={() => network.refetch()} disabled={network.isFetching}><RefreshCw className={network.isFetching ? 'size-4 animate-spin' : 'size-4'} /> Refresh</Button>
      </header>
      <div className="network-summary">
        <Card><span>Observed</span><strong>{observed?.interfaceCount ?? '—'}</strong><small>interfaces</small></Card>
        <Card><span>Configured</span><strong>{network.data?.configuredCount ?? '—'}</strong><small>entries</small></Card>
        <Card><span>Observer</span><Status ok={observed?.available === true}>{observed?.available ? 'available' : 'unavailable'}</Status><small>systemd-networkd</small></Card>
      </div>
      {observed?.error ? <p className="callout warning" role="status">{observed.error}</p> : null}
      <Card>
        <CardHeader title="Interface details" description="Operational and carrier state are live observations, not inferred from configuration." />
        <div className="network-table-wrap">
          <table className="network-table">
            <thead><tr><th>Interface</th><th>Configured</th><th>Observed type</th><th>Operational</th><th>Carrier</th><th>Address state</th><th>Addresses</th></tr></thead>
            <tbody>
              {rows.map((row) => <InterfaceRow row={row} key={row.name} />)}
            </tbody>
          </table>
          {rows.length === 0 ? <div className="empty"><Cable className="size-5" /> No interfaces were reported or configured.</div> : null}
        </div>
      </Card>
      <Card>
        <CardHeader title="Configured map" description="Desired state stored by mosd. Changes are accepted only through the typed /api/v1/network routes." />
        <pre className="json-view">{JSON.stringify(network.data?.configured ?? {}, null, 2)}</pre>
      </Card>
    </div>
  )
}

function InterfaceRow({ row }: { row: NetworkRow }) {
  const iface = row.observed
  const status = iface?.operationalState ?? iface?.administrativeState ?? 'not observed'
  return (
    <tr>
      <td><strong>{row.name}</strong><small>index {iface?.index ?? '—'}{iface?.mtu ? ` · MTU ${iface.mtu}` : ''}</small></td>
      <td>{configuredSummary(row.configured)}</td>
      <td>{iface?.kind ?? iface?.type ?? iface?.driver ?? '—'}</td>
      <td><Status ok={['routable', 'carrier', 'degraded'].includes(status)}>{status}</Status></td>
      <td>{iface?.carrierState ?? '—'}</td>
      <td>{iface?.addressState ?? iface?.ipv4AddressState ?? '—'}</td>
      <td className="mono-cell">{summarize(iface?.addresses)}</td>
    </tr>
  )
}

function summarize(items?: unknown[]) {
  if (!items?.length) return '—'
  return items.map((item) => {
    if (typeof item === 'string') return item
    if (item && typeof item === 'object') {
      const value = item as Record<string, unknown>
      const address = value.Address ?? value.address
      const prefix = value.PrefixLength ?? value.prefixLength
      if (Array.isArray(address)) return `${address.join('.')}${prefix === undefined ? '' : `/${prefix}`}`
      if (typeof address === 'string') return `${address}${prefix === undefined ? '' : `/${prefix}`}`
    }
    return 'address'
  }).join(', ')
}

export const Route = createFileRoute('/network')({ component: NetworkPage })
