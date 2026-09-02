import { createFileRoute } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Cable, Radio, Route as RouteIcon } from 'lucide-react'
import type { AvailableFact, ObservedNetworkInterface, ObservedNetworkState } from '@/lib/types'
import { useObservedNetwork } from '@/lib/diagnostics'
import { errorMessage } from '@/lib/api'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'

export function ObservedNetworkPage() {
  const { t } = useTranslation()
  const network = useObservedNetwork()
  const value = network.data
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('observedNetwork.eyebrow')}</p><h1>{t('observedNetwork.title')}</h1><p>{t('observedNetwork.description')}</p></div></header>
      <p className="callout warning" role="note">{t('observedNetwork.separation')} <a className="font-medium underline" href="/_ui/network">{t('observedNetwork.openDesired')}</a></p>
      {network.isPending ? <p className="callout" role="status">{t('observedNetwork.loading')}</p> : null}
      {network.error ? <p className="callout error" role="alert">{errorMessage(network.error, t('common.requestFailed'))}</p> : null}
      {value ? (
        <>
          <Card>
            <CardHeader title={t('observedNetwork.interfaces.title')} description={t('observedNetwork.interfaces.description')} action={<Cable className="size-5 text-muted-foreground" />} />
            {!value.interfaces.available ? <Unavailable fact={value.interfaces} /> : null}
            <div className="grid gap-4">
              {value.interfaces.entries?.map((entry) => <InterfaceDetails key={entry.name} value={entry} />)}
            </div>
            {value.interfaces.available && value.interfaces.count === 0 ? <p className="callout warning" role="status">{t('observedNetwork.interfaces.empty')}</p> : null}
          </Card>
          <div className="split-grid">
            <Card>
              <CardHeader title={t('observedNetwork.routes.title')} description={t('observedNetwork.routes.description')} action={<RouteIcon className="size-5 text-muted-foreground" />} />
              {!value.defaultRoutes.available ? <Unavailable fact={value.defaultRoutes} /> : null}
              {value.defaultRoutes.available && value.defaultRoutes.count === 0 ? <p className="callout warning" role="status">{t('observedNetwork.routes.empty')}</p> : null}
              <dl className="details">{value.defaultRoutes.entries?.map((route, index) => <div key={`${route.family}-${route.gateway}-${index}`}><dt>{route.interface ?? t('common.notAvailable')}</dt><dd>{join([route.gateway, route.family, route.metric === undefined ? undefined : `metric ${route.metric}`, route.configSource])}</dd></div>)}</dl>
            </Card>
            <Card>
              <CardHeader title={t('observedNetwork.dns.title')} description={t('observedNetwork.dns.description')} action={<Radio className="size-5 text-muted-foreground" />} />
              {!value.dns.available ? <Unavailable fact={value.dns} /> : (
                <dl className="details">
                  <div><dt>{t('observedNetwork.dns.link')}</dt><dd>{value.dns.linkServers?.join(', ') || t('common.notAvailable')}</dd></div>
                  <div><dt>{t('observedNetwork.dns.resolver')}</dt><dd>{value.dns.resolverServers?.join(', ') || t('common.notAvailable')}</dd></div>
                  {value.dns.probe ? <div><dt>{value.dns.probe.name}</dt><dd><Status ok={value.dns.probe.reachable}>{join([value.dns.probe.result, value.dns.probe.detail])}</Status></dd></div> : null}
                </dl>
              )}
            </Card>
          </div>
          <div className="split-grid">
            <WifiAssociations value={value.wifi} />
            <Card>
              <CardHeader title={t('observedNetwork.capabilities.title')} description={t('observedNetwork.capabilities.description')} action={<Radio className="size-5 text-muted-foreground" />} />
              <dl className="details">
                <Capability label={t('observedNetwork.capabilities.wifi')} supported={value.capabilities.wifi.supported} entries={value.capabilities.wifi.interfaces} />
                <Capability label={t('observedNetwork.capabilities.bluetooth')} supported={value.capabilities.bluetooth.supported} entries={value.capabilities.bluetooth.adapters} />
                <Capability label={t('observedNetwork.capabilities.cellular')} supported={value.capabilities.cellular.supported} entries={value.capabilities.cellular.interfaces} />
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
      <CardHeader title={t('observedNetwork.wifi.title')} description={t('observedNetwork.wifi.description')} action={<Radio className="size-5 text-muted-foreground" />} />
      {!value.available ? <Unavailable fact={value} /> : (
        <dl className="details">
          {value.associations?.map((association, index) => (
            <div key={`${association.interface ?? 'wifi'}-${index}`}>
              <dt>{association.interface ?? t('observedNetwork.wifi.interfaceFallback')}</dt>
              <dd>{join([
                association.state,
                association.associated ? t('observedNetwork.wifi.associated') : t('observedNetwork.wifi.notAssociated'),
                association.ssid,
                association.rssiDbm === undefined ? undefined : `${association.rssiDbm} dBm`,
                association.linkSpeedMbps === undefined ? undefined : `${association.linkSpeedMbps} Mbps`,
              ])}</dd>
            </div>
          ))}
        </dl>
      )}
      {value.available && value.associations?.length === 0 ? <p className="callout" role="status">{t('observedNetwork.wifi.empty')}</p> : null}
    </Card>
  )
}

function InterfaceDetails({ value }: { value: ObservedNetworkInterface }) {
  const { t } = useTranslation()
  const link = join([value.link.operationalState, value.link.carrierState ? `carrier ${value.link.carrierState}` : undefined, value.link.onlineState])
  const lease = value.dhcp.lease
  return (
    <section className="rounded-lg border p-4" aria-label={value.name}>
      <div className="mb-3 flex flex-wrap items-center justify-between gap-2"><h3 className="font-semibold">{value.name}</h3><Status ok={value.link.carrier === true}>{link}</Status></div>
      <dl className="details">
        <div><dt>{t('observedNetwork.interfaces.device')}</dt><dd>{join([value.kind ?? value.type, value.driver, value.mtu === undefined ? undefined : `MTU ${value.mtu}`])}</dd></div>
        <div><dt>{t('observedNetwork.interfaces.addresses')}</dt><dd>{value.addresses.map((address) => `${address.address ?? '—'}${address.prefixLength === undefined ? '' : `/${address.prefixLength}`}${address.configSource ? ` · ${address.configSource}` : ''}`).join(', ') || t('common.notAvailable')}</dd></div>
        <div><dt>{t('observedNetwork.interfaces.dhcp')}</dt><dd>{value.dhcp.available ? join([value.dhcp.state, lease?.server ? `server ${lease.server}` : undefined, lease?.router ? `router ${lease.router}` : undefined, lease?.lifetimeSeconds === undefined ? undefined : `${lease.lifetimeSeconds}s`]) : <Unavailable fact={value.dhcp} />}</dd></div>
        <div><dt>{t('observedNetwork.interfaces.dns')}</dt><dd>{value.dns.map((server) => typeof server === 'string' ? server : server.address).filter(Boolean).join(', ') || t('common.notAvailable')}</dd></div>
        {value.wifi ? <div><dt>{t('observedNetwork.interfaces.wifi')}</dt><dd>{value.wifi.available ? join([value.wifi.state, value.wifi.associated ? t('observedNetwork.wifi.associated') : t('observedNetwork.wifi.notAssociated'), value.wifi.ssid, value.wifi.rssiDbm === undefined ? undefined : `${value.wifi.rssiDbm} dBm`]) : <Unavailable fact={value.wifi} />}</dd></div> : null}
      </dl>
    </section>
  )
}

function Capability({ label, supported, entries }: { label: string; supported: boolean; entries: string[] }) {
  const { t } = useTranslation()
  return <div><dt>{label}</dt><dd>{supported ? join([t('observedNetwork.capabilities.supported'), entries.join(', ')]) : t('observedNetwork.capabilities.unsupported')}</dd></div>
}

function Unavailable({ fact }: { fact: AvailableFact }) {
  const { t } = useTranslation()
  return <p className="callout warning" role="status">{t('observedNetwork.unavailable')}{fact.detail ? `: ${fact.detail}` : ''}</p>
}

function join(values: (string | null | undefined)[]) {
  return values.filter((value): value is string => Boolean(value)).join(' · ')
}

export const Route = createFileRoute('/network-status')({ component: ObservedNetworkPage })
