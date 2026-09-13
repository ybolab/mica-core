import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Cable, Radio, Route as RouteIcon } from 'lucide-react'
import { api } from '@/shared/lib/http'
import type { ObservedNetworkInterface, ObservedNetworkState } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { FactList, type Fact } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'
import { StatusDot } from '@/shared/components/status-badge'
import { Spinner } from '@/shared/components/ui/spinner'
import { failureDetail } from '@/shared/feedback/toast'
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
    <div className="grid gap-3">
      <Callout tone="warning" title={t('network.observed.separation')} />
      {network.isPending ? <p className="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />{t('network.observed.loading')}</p> : null}
      {network.error ? <Callout tone="danger" title={failureDetail(network.error, t('common.requestFailed'))} /> : null}
      {value ? (
        <>
          <Panel title={t('network.observed.interfaces.title')} description={t('network.observed.interfaces.description')} action={<Cable className="size-5 text-muted-foreground" />}>
            {!value.interfaces.available ? <Callout tone="warning"><Unavailable fact={value.interfaces} /></Callout> : null}
            {value.interfaces.entries?.map((entry) => <InterfaceDetails key={entry.name} value={entry} />)}
            {value.interfaces.available && value.interfaces.count === 0 ? <p className="py-4 text-center text-sm text-muted-foreground">{t('network.observed.interfaces.empty')}</p> : null}
          </Panel>
          <div className="grid gap-3 lg:grid-cols-2">
            <Panel title={t('network.observed.routes.title')} description={t('network.observed.routes.description')} action={<RouteIcon className="size-5 text-muted-foreground" />}>
              {!value.defaultRoutes.available ? <Callout tone="warning"><Unavailable fact={value.defaultRoutes} /></Callout> : null}
              {value.defaultRoutes.available && value.defaultRoutes.count === 0 ? <p className="py-4 text-center text-sm text-muted-foreground">{t('network.observed.routes.empty')}</p> : null}
              <FactList facts={(value.defaultRoutes.entries ?? []).map((route, index) => ({
                id: `${route.family}-${route.gateway}-${index}`,
                label: route.interface ?? t('common.notAvailable'),
                value: join([route.gateway, route.family, route.metric === undefined ? undefined : t('network.observed.routes.metric', { metric: route.metric }), route.configSource]) ?? t('common.notAvailable'),
              }))} />
            </Panel>
            <Panel title={t('network.observed.dns.title')} description={t('network.observed.dns.description')} action={<Radio className="size-5 text-muted-foreground" />}>
              {!value.dns.available ? <Callout tone="warning"><Unavailable fact={value.dns} /></Callout> : (
                <FactList facts={[
                  { id: 'link', label: t('network.observed.dns.link'), value: value.dns.linkServers?.join(', ') || t('common.notAvailable') },
                  { id: 'resolver', label: t('network.observed.dns.resolver'), value: value.dns.resolverServers?.join(', ') || t('common.notAvailable') },
                  value.dns.probe ? {
                    id: 'probe',
                    label: value.dns.probe.name,
                    value: <StatusDot state={value.dns.probe.reachable ? 'ok' : 'warning'}>{join([value.dns.probe.result, value.dns.probe.detail])}</StatusDot>,
                  } : undefined,
                ]} />
              )}
            </Panel>
          </div>
          <div className="grid gap-3 lg:grid-cols-2">
            <WifiAssociations value={value.wifi} />
            <Panel title={t('network.observed.capabilities.title')} description={t('network.observed.capabilities.description')} action={<Radio className="size-5 text-muted-foreground" />}>
              <FactList facts={[
                capability('wifi', t('network.observed.capabilities.wifi'), value.capabilities.wifi.supported, value.capabilities.wifi.interfaces, t),
                capability('bluetooth', t('network.observed.capabilities.bluetooth'), value.capabilities.bluetooth.supported, value.capabilities.bluetooth.adapters, t),
                capability('cellular', t('network.observed.capabilities.cellular'), value.capabilities.cellular.supported, value.capabilities.cellular.interfaces, t),
              ]} />
            </Panel>
          </div>
        </>
      ) : null}
    </div>
  )
}

function WifiAssociations({ value }: { value: ObservedNetworkState['wifi'] }) {
  const { t } = useTranslation()
  return (
    <Panel title={t('network.observed.wifi.title')} description={t('network.observed.wifi.description')} action={<Radio className="size-5 text-muted-foreground" />}>
      {!value.available ? <Callout tone="warning"><Unavailable fact={value} /></Callout> : (
        <FactList facts={(value.associations ?? []).map((association, index) => ({
          id: `${association.interface ?? 'wifi'}-${index}`,
          label: association.interface ?? t('network.observed.wifi.interfaceFallback'),
          value: join([
            association.state,
            t(association.associated ? 'network.observed.wifi.associated' : 'network.observed.wifi.notAssociated'),
            association.ssid,
            association.rssiDbm === undefined ? undefined : `${association.rssiDbm} dBm`,
            association.linkSpeedMbps === undefined ? undefined : `${association.linkSpeedMbps} Mbps`,
          ]),
        }))} />
      )}
      {value.available && value.associations?.length === 0 ? <p className="py-4 text-center text-sm text-muted-foreground">{t('network.observed.wifi.empty')}</p> : null}
    </Panel>
  )
}

function InterfaceDetails({ value }: { value: ObservedNetworkInterface }) {
  const { t } = useTranslation()
  const link = join([value.link.operationalState, value.link.carrierState ? t('network.observed.interfaces.carrier', { state: value.link.carrierState }) : undefined, value.link.onlineState])
  const lease = value.dhcp.lease
  return (
    <section className="rounded-lg border p-4" aria-label={value.name}>
      <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
        <h3 className="font-semibold">{value.name}</h3>
        <StatusDot state={value.link.carrier === true ? 'ok' : 'warning'}>{link ?? t('common.states.unknown')}</StatusDot>
      </div>
      <FactList facts={[
        { id: 'device', label: t('network.observed.interfaces.device'), value: join([value.kind ?? value.type, value.driver, value.mtu === undefined ? undefined : t('network.mtu', { mtu: value.mtu })]) ?? t('common.notAvailable') },
        { id: 'addresses', label: t('network.observed.interfaces.addresses'), value: value.addresses.map((address) => `${address.address ?? t('common.notAvailable')}${address.prefixLength === undefined ? '' : `/${address.prefixLength}`}${address.configSource ? ` · ${address.configSource}` : ''}`).join(', ') || t('common.notAvailable') },
        { id: 'dhcp', label: t('network.observed.interfaces.dhcp'), value: value.dhcp.available ? (join([value.dhcp.state, lease?.server ? t('network.observed.interfaces.leaseServer', { server: lease.server }) : undefined, lease?.router ? t('network.observed.interfaces.leaseRouter', { router: lease.router }) : undefined, lease?.lifetimeSeconds === undefined ? undefined : t('network.observed.interfaces.leaseLifetime', { seconds: lease.lifetimeSeconds })]) ?? t('common.notAvailable')) : <Unavailable fact={value.dhcp} /> },
        { id: 'dns', label: t('network.observed.interfaces.dns'), value: value.dns.map((server) => typeof server === 'string' ? server : server.address).filter(Boolean).join(', ') || t('common.notAvailable') },
        value.wifi ? { id: 'wifi', label: t('network.observed.interfaces.wifi'), value: value.wifi.available ? (join([value.wifi.state, t(value.wifi.associated ? 'network.observed.wifi.associated' : 'network.observed.wifi.notAssociated'), value.wifi.ssid, value.wifi.rssiDbm === undefined ? undefined : `${value.wifi.rssiDbm} dBm`]) ?? t('common.notAvailable')) : <Unavailable fact={value.wifi} /> } : undefined,
      ]} />
    </section>
  )
}

/// Cellular is reported explicitly unsupported by the device; an unsupported
/// radio is rendered as that word, never omitted.
function capability(id: string, label: string, supported: boolean, entries: string[], t: ReturnType<typeof useTranslation>['t']): Fact {
  return {
    id,
    label,
    value: supported ? join([t('network.observed.capabilities.supported'), entries.join(', ')]) : t('network.observed.capabilities.unsupported'),
  }
}
