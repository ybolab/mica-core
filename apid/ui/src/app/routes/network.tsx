import { createFileRoute } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Cable, RefreshCw } from 'lucide-react'
import { api } from '@/lib/api'
import { configuredSummary, networkRows, type NetworkRow } from '@/lib/network'
import type { NetworkOverview } from '@/lib/types'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import { formatKnownState } from '@/i18n/format'

function NetworkPage() {
  const { t } = useTranslation()
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const observed = network.data?.observed
  const rows = networkRows(network.data?.configured, observed?.interfaces)
  return (
    <div className="page">
      <header className="page-head">
        <div><p className="eyebrow">{t('network.eyebrow')}</p><h1>{t('network.title')}</h1><p>{t('network.description')}</p></div>
        <Button variant="secondary" onClick={() => network.refetch()} disabled={network.isFetching}><RefreshCw className={network.isFetching ? 'size-4 animate-spin' : 'size-4'} /> {t('common.actions.refresh')}</Button>
      </header>
      <div className="network-summary">
        <Card><span>{t('network.observed')}</span><strong>{observed?.interfaceCount ?? t('common.notAvailable')}</strong><small>{t('network.interfaces')}</small></Card>
        <Card><span>{t('network.configured')}</span><strong>{network.data?.configuredCount ?? t('common.notAvailable')}</strong><small>{t('network.entries')}</small></Card>
        <Card><span>{t('network.observer')}</span><Status ok={observed?.available === true}>{t(observed?.available ? 'common.states.available' : 'common.states.unavailable')}</Status><small>systemd-networkd</small></Card>
      </div>
      {observed?.error ? <p className="callout warning" role="status">{observed.error}</p> : null}
      <Card>
        <CardHeader title={t('network.details')} description={t('network.detailsDescription')} />
        <div className="network-table-wrap">
          <table className="network-table">
            <thead><tr><th>{t('network.table.interface')}</th><th>{t('network.table.configured')}</th><th>{t('network.table.observedType')}</th><th>{t('network.table.operational')}</th><th>{t('network.table.carrier')}</th><th>{t('network.table.addressState')}</th><th>{t('network.table.addresses')}</th></tr></thead>
            <tbody>
              {rows.map((row) => <InterfaceRow row={row} key={row.name} />)}
            </tbody>
          </table>
          {rows.length === 0 ? <div className="empty"><Cable className="size-5" /> {t('network.noInterfaces')}</div> : null}
        </div>
      </Card>
      <Card>
        <CardHeader title={t('network.configuredMap')} description={t('network.configuredMapDescription')} />
        <pre className="json-view">{JSON.stringify(network.data?.configured ?? {}, null, 2)}</pre>
      </Card>
    </div>
  )
}

function InterfaceRow({ row }: { row: NetworkRow }) {
  const { t } = useTranslation()
  const iface = row.observed
  const status = iface?.operationalState ?? iface?.administrativeState ?? t('network.notObserved')
  const summaryLabels = {
    notConfigured: t('network.summary.notConfigured'),
    physical: t('network.summary.physical'),
    dhcp: t('network.summary.dhcp'),
    static: t('network.summary.static'),
    noAddressing: t('network.summary.noAddressing'),
    format: (kind: string, method: string) => t('network.summary.value', { kind, method }),
  }
  return (
    <tr>
      <td><strong>{row.name}</strong><small>{t('network.index', { index: iface?.index ?? t('common.notAvailable') })}{iface?.mtu ? ` · ${t('network.mtu', { mtu: iface.mtu })}` : ''}</small></td>
      <td>{configuredSummary(row.configured, summaryLabels)}</td>
      <td>{iface?.kind ?? iface?.type ?? iface?.driver ?? t('common.notAvailable')}</td>
      <td><Status ok={['routable', 'carrier', 'degraded'].includes(iface?.operationalState ?? '')}>{formatKnownState(status, t)}</Status></td>
      <td>{iface?.carrierState ? formatKnownState(iface.carrierState, t) : t('common.notAvailable')}</td>
      <td>{iface?.addressState ?? iface?.ipv4AddressState ?? t('common.notAvailable')}</td>
      <td className="mono-cell">{summarize(iface?.addresses, t('network.addressFallback'))}</td>
    </tr>
  )
}

function summarize(items: unknown[] | undefined, addressFallback: string) {
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
    return addressFallback
  }).join(', ')
}

export const Route = createFileRoute('/network')({ component: NetworkPage })
