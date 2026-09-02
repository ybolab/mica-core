import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Cable, Radio, Route as RouteIcon } from 'lucide-react'
import { api, errorMessage } from '@/shared/lib/http'
import type { ObservedNetworkInterface, ObservedNetworkState } from '@/lib/types'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import { Unavailable, join } from '@/shared/components/fact'

/// What the device SEES. This is a different thing from the desired map the
/// other tabs edit, so the separation is stated on the panel rather than left
/// to the reader.
export function ObservedNetworkPanel() {
  const { t } = useTranslation()
  const network = useQuery({
    queryKey: ['observed-network'],
    queryFn: () => api<ObservedNetworkState>('/api/v1/network/status'),
    refetchInterval: 15_000,
    retry: false,
  })
  const value = network.data
  return (
    <div className="stack">
      <p className="callout warning" role="note">{t('network.observed.separation')}</p>
      {network.isPending ? <p className="callout" role="status">{t('network.observed.loading')}</p> : null}
      {network.error ? <p className="callout error" role="alert">{errorMessage(network.error, t('common.requestFailed'))}</p> : null}
      {value ? (
        <>
          <Card>
            <CardHeader title={t('network.observed.interfaces.title')} description={t('network.observed.interfaces.description')} action={<Cable className="size-5 text-muted-foreground" />} />
            {!value.interfaces.available ? <p className="callout warning" role="status"><Unavailable fact={value.interfaces} /></p> : null}
            <div className="grid gap-4">
              {value.interfaces.entries?.map((entry) => <InterfaceDetails key={entry.name} value={entry} />)}
            </div>
            {value.interfaces.available && value.interfaces.count === 0 ? <p className="empty">{t('network.observed.interfaces.empty')}</p> : null}
          </Card>
          <div className="split-grid">
            <Card>
              <CardHeader title={t('network.observed.routes.title')} description={t('network.observed.routes.description')} action={<RouteIcon className="size-5 text-muted-foreground" />} />
              {!value.defaultRoutes.available ? <p className="callout warning" role="status"><Unavailable fact={value.defaultRoutes} /></p> : null}
              {value.defaultRoutes.available && value.defaultRoutes.count === 0 ? <p className="empty">{t('network.observed.routes.empty')}</p> : null}
              <dl className="details">{value.defaultRoutes.entries?.map((route, index) => <div key={`${route.family}-${route.gateway}-${index}`}><dt>{route.interface ?? t('common.notAvailable')}</dt><dd>{join([route.gateway, route.family, route.metric === undefined ? undefined : t('network.observed.routes.metric', { metric: route.metric }), route.configSource]) ?? t('common.notAvailable')}</dd></div>)}</dl>
            </Card>
            <Card>
              <CardHeader title={t('network.observed.dns.title')} description={t('network.observed.dns.description')} action={<Radio className="size-5 text-muted-foreground" />} />
              {!value.dns.available ? <p className="callout warning" role="status"><Unavailable fact={value.dns} /></p> : (
                <dl className="details">
                  <div><dt>{t('network.observed.dns.link')}</dt><dd>{value.dns.linkServers?.join(', ') || t('common.notAvailable')}</dd></div>
                  <div><dt>{t('network.observed.dns.resolver')}</dt><dd>{value.dns.resolverServers?.join(', ') || t('common.notAvailable')}</dd></div>
                  {value.dns.probe ? <div><dt>{value.dns.probe.name}</dt><dd><Status ok={value.dns.probe.reachable}>{join([value.dns.probe.result, value.dns.probe.detail])}</Status></dd></div> : null}
                </dl>
              )}
            </Card>
          </div>
          <div className="split-grid">
            <WifiAssociations value={value.wifi} />
            <Card>
              <CardHeader title={t('network.observed.capabilities.title')} description={t('network.observed.capabilities.description')} action={<Radio className="size-5 text-muted-foreground" />} />
              <dl className="details">
                <Capability label={t('network.observed.capabilities.wifi')} supported={value.capabilities.wifi.supported} entries={value.capabilities.wifi.interfaces} />
                <Capability label={t('network.observed.capabilities.bluetooth')} supported={value.capabilities.bluetooth.supported} entries={value.capabilities.bluetooth.adapters} />
                <Capability label={t('network.observed.capabilities.cellular')} supported={value.capabilities.cellular.supported} entries={value.capabilities.cellular.interfaces} />
              </dl>
            </Card>
          </div>
        </>
      ) : null}
    </div>
  )
}

function WifiAssociations({ value }: { value: ObservedNetworkState['wifi'] }) {
  const { t } = useTranslation()
  return (
    <Card>
      <CardHeader title={t('network.observed.wifi.title')} description={t('network.observed.wifi.description')} action={<Radio className="size-5 text-muted-foreground" />} />
      {!value.available ? <p className="callout warning" role="status"><Unavailable fact={value} /></p> : (
        <dl className="details">
          {value.associations?.map((association, index) => (
            <div key={`${association.interface ?? 'wifi'}-${index}`}>
              <dt>{association.interface ?? t('network.observed.wifi.interfaceFallback')}</dt>
              <dd>{join([
                association.state,
                t(association.associated ? 'network.observed.wifi.associated' : 'network.observed.wifi.notAssociated'),
                association.ssid,
                association.rssiDbm === undefined ? undefined : `${association.rssiDbm} dBm`,
                association.linkSpeedMbps === undefined ? undefined : `${association.linkSpeedMbps} Mbps`,
              ])}</dd>
            </div>
          ))}
        </dl>
      )}
      {value.available && value.associations?.length === 0 ? <p className="empty">{t('network.observed.wifi.empty')}</p> : null}
    </Card>
  )
}

function InterfaceDetails({ value }: { value: ObservedNetworkInterface }) {
  const { t } = useTranslation()
  const link = join([value.link.operationalState, value.link.carrierState ? t('network.observed.interfaces.carrier', { state: value.link.carrierState }) : undefined, value.link.onlineState])
  const lease = value.dhcp.lease
  return (
    <section className="rounded-lg border border-border p-4" aria-label={value.name}>
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2"><h3 className="font-semibold">{value.name}</h3><Status ok={value.link.carrier === true}>{link ?? t('common.states.unknown')}</Status></div>
      <dl className="details">
        <div><dt>{t('network.observed.interfaces.device')}</dt><dd>{join([value.kind ?? value.type, value.driver, value.mtu === undefined ? undefined : t('network.mtu', { mtu: value.mtu })]) ?? t('common.notAvailable')}</dd></div>
        <div><dt>{t('network.observed.interfaces.addresses')}</dt><dd>{value.addresses.map((address) => `${address.address ?? t('common.notAvailable')}${address.prefixLength === undefined ? '' : `/${address.prefixLength}`}${address.configSource ? ` · ${address.configSource}` : ''}`).join(', ') || t('common.notAvailable')}</dd></div>
        <div><dt>{t('network.observed.interfaces.dhcp')}</dt><dd>{value.dhcp.available ? (join([value.dhcp.state, lease?.server ? t('network.observed.interfaces.leaseServer', { server: lease.server }) : undefined, lease?.router ? t('network.observed.interfaces.leaseRouter', { router: lease.router }) : undefined, lease?.lifetimeSeconds === undefined ? undefined : t('network.observed.interfaces.leaseLifetime', { seconds: lease.lifetimeSeconds })]) ?? t('common.notAvailable')) : <Unavailable fact={value.dhcp} />}</dd></div>
        <div><dt>{t('network.observed.interfaces.dns')}</dt><dd>{value.dns.map((server) => typeof server === 'string' ? server : server.address).filter(Boolean).join(', ') || t('common.notAvailable')}</dd></div>
        {value.wifi ? <div><dt>{t('network.observed.interfaces.wifi')}</dt><dd>{value.wifi.available ? (join([value.wifi.state, t(value.wifi.associated ? 'network.observed.wifi.associated' : 'network.observed.wifi.notAssociated'), value.wifi.ssid, value.wifi.rssiDbm === undefined ? undefined : `${value.wifi.rssiDbm} dBm`]) ?? t('common.notAvailable')) : <Unavailable fact={value.wifi} />}</dd></div> : null}
      </dl>
    </section>
  )
}

/// Cellular is reported explicitly unsupported by the device; an unsupported
/// radio is rendered as that word, never omitted.
function Capability({ label, supported, entries }: { label: string; supported: boolean; entries: string[] }) {
  const { t } = useTranslation()
  return <div><dt>{label}</dt><dd>{supported ? join([t('network.observed.capabilities.supported'), entries.join(', ')]) : t('network.observed.capabilities.unsupported')}</dd></div>
}
