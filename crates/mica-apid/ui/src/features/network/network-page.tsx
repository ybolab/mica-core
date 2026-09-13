import { useMemo, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Cable, ChevronRight, KeyRound, Plus, Trash2 } from 'lucide-react'
import { api, json } from '@/shared/lib/http'
import { configuredSummary, networkRows, type NetworkRow } from '@/lib/network'
import type { NetworkOverview, TaskAccepted } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { CollectionPanel, Panel } from '@/shared/components/panel'
import { DataTable } from '@/shared/components/data-table'
import { FormDialog } from '@/shared/components/form-dialog'
import { FormField, ToggleField } from '@/shared/components/form-field'
import { MetricCard } from '@/shared/components/metric-card'
import { Page, PageHeader, PageSection } from '@/shared/components/page'
import { StatusBadge } from '@/shared/components/status-badge'
import { TaskProgress } from '@/shared/components/task-progress'
import { Button } from '@/shared/components/ui/button'
import { Input } from '@/shared/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/shared/components/ui/select'
import { Switch } from '@/shared/components/ui/switch'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/shared/components/ui/tabs'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'
import { ObservedNetworkPanel } from '@/features/network/observed-network-panel'
import { formatAge, formatKnownState } from '@/i18n/format'

interface WifiNetwork { ssid: string; psk?: string; hidden: boolean; priority: number }
interface WifiClient { enabled: boolean; interface: string; networks: WifiNetwork[] }
interface WireguardPeer { publicKey: string; allowedIps: string[]; endpoint?: string; persistentKeepalive?: number }
interface WireguardRotation { publicKey: string }
interface InterfaceConfig {
  kind?: 'physical' | 'vlan' | 'bridge' | 'wireguard'
  dhcp: boolean
  static?: { address: string; gateway?: string; dns: string[] }
  vlan?: { parent: string; id: number }
  bridge?: { ports: string[] }
  wireguard?: { listenPort?: number; peers: WireguardPeer[] }
}

export function NetworkPage() {
  const { t } = useTranslation()
  const [adding, setAdding] = useState(false)
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const rows = networkRows(network.data?.configured, network.data?.observed.interfaces)
  const age = formatAge(Date.now() - network.dataUpdatedAt, t)
  const summaryLabels = {
    notConfigured: t('network.summary.notConfigured'),
    physical: t('network.summary.physical'),
    dhcp: t('network.summary.dhcp'),
    static: t('network.summary.static'),
    noAddressing: t('network.summary.noAddressing'),
    format: (kind: string, method: string) => t('network.summary.value', { kind, method }),
  }

  return (
    <Page>
      <PageHeader title={t('network.title')} action={<Button onClick={() => setAdding(true)}><Plus />{t('network.actions.addInterface')}</Button>} />
      {network.error ? <Callout tone="danger" title={failureDetail(network.error, t('common.requestFailed'))} /> : null}
      {network.data?.observed.error ? <Callout tone="warning" title={network.data.observed.error} /> : null}
      <Tabs defaultValue="interfaces">
        <TabsList aria-label={t('network.title')}>
          <TabsTrigger value="interfaces">{t('network.tabs.interfaces')}</TabsTrigger>
          <TabsTrigger value="wifi">{t('network.tabs.wifi')}</TabsTrigger>
          <TabsTrigger value="wireguard">{t('network.tabs.wireguard')}</TabsTrigger>
        </TabsList>
        <TabsContent value="interfaces" className="grid gap-6 pt-4">
          <CollectionPanel>
            <DataTable<NetworkRow>
              rows={rows}
              rowKey={(row) => row.name}
              isPending={network.isPending}
              empty={t('network.noInterfaces')}
              emptyIcon={<Cable />}
              columns={[
                { id: 'interface', header: t('network.table.interface'), cell: (row) => row.name },
                { id: 'type', header: t('network.table.type'), cell: (row) => kindOf(row) ? t(`network.kinds.${kindOf(row)}`, { defaultValue: kindOf(row) }) : t('common.notAvailable') },
                { id: 'configured', header: t('network.table.configured'), cell: (row) => configuredSummary(row.configured, summaryLabels) },
                { id: 'observed', header: t('network.table.observed'), cell: (row) => (
                  <StatusBadge tone={isOnline(row) ? 'success' : 'danger'}>
                    {row.observed?.operationalState ? formatKnownState(row.observed.operationalState, t) : t('network.notObserved')}
                  </StatusBadge>
                ) },
                { id: 'addresses', header: t('network.table.addresses'), cell: (row) => <span className="font-mono text-[0.8125rem] break-all">{summarize(row.observed?.addresses)}</span> },
                { id: 'age', header: t('network.table.lastObserved'), align: 'end', cell: (row) => row.observed ? age : '—' },
              ]}
              // One focus stop and one navigation per row. The row used to
              // carry an onClick beside a nested link, so a click on the name
              // navigated twice and the keyboard reached neither.
              rowHref={(row) => (
                <Link to="/network/$name" params={{ name: row.name }} className="flex items-center gap-1 font-mono font-medium text-primary hover:underline">
                  {row.name}<ChevronRight className="size-3.5" aria-hidden="true" />
                </Link>
              )}
            />
          </CollectionPanel>
          <PageSection title={t('network.observed.title')} description={t('network.observed.addition')}>
            <ObservedNetworkPanel />
          </PageSection>
        </TabsContent>
        <TabsContent value="wifi" className="grid gap-6 pt-4"><WifiPanel /></TabsContent>
        <TabsContent value="wireguard" className="grid gap-6 pt-4"><WireguardPanel configured={(network.data?.configured ?? {}) as Record<string, InterfaceConfig>} /></TabsContent>
      </Tabs>
      <InterfaceDialog key={adding ? 'new' : 'closed'} open={adding} onClose={() => setAdding(false)} />
    </Page>
  )
}

function kindOf(row: NetworkRow) {
  return (row.configured?.kind as string | undefined) ?? row.observed?.kind ?? row.observed?.type
}

function isOnline(row: NetworkRow) {
  return ['routable', 'carrier', 'degraded'].includes(row.observed?.operationalState ?? '')
}

function InterfaceDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [name, setName] = useState('')
  const [kind, setKind] = useState<NonNullable<InterfaceConfig['kind']>>('physical')
  const [dhcp, setDhcp] = useState(true)
  const [address, setAddress] = useState('')
  const [gateway, setGateway] = useState('')
  const [dns, setDns] = useState('')
  const [parent, setParent] = useState('eth0')
  const [vlanId, setVlanId] = useState('100')
  const [ports, setPorts] = useState('')
  const [listenPort, setListenPort] = useState('51820')

  const save = () => {
    const value: InterfaceConfig = { dhcp }
    if (kind !== 'physical') value.kind = kind
    if (!dhcp) value.static = { address, ...(gateway ? { gateway } : {}), dns: splitList(dns) }
    if (kind === 'vlan') value.vlan = { parent, id: Number(vlanId) }
    if (kind === 'bridge') value.bridge = { ports: splitList(ports) }
    if (kind === 'wireguard') value.wireguard = { listenPort: Number(listenPort), peers: [] }
    return api<TaskAccepted>(`/api/v1/network/${encodeURIComponent(name)}`, json('PUT', value))
      .then(() => queryClient.invalidateQueries({ queryKey: ['network'] }))
  }

  return (
    <FormDialog
      open={open}
      onOpenChange={(next) => { if (!next) onClose() }}
      title={t('network.editor.addTitle')}
      description={t('network.editor.description')}
      submitLabel={t('common.actions.add')}
      success={t('network.editor.added', { name })}
      failure={t('network.editor.addTitle')}
      onSubmit={save}
    >
      <FormField label={t('network.editor.name')}>
        {(id) => <Input id={id} value={name} onChange={(event) => setName(event.target.value)} required />}
      </FormField>
      <FormField label={t('network.editor.kind')}>
        {(id) => (
          <Select value={kind} onValueChange={(value) => setKind(value as typeof kind)}>
            <SelectTrigger id={id}><SelectValue /></SelectTrigger>
            <SelectContent>{(['physical', 'vlan', 'bridge', 'wireguard'] as const).map((option) => <SelectItem value={option} key={option}>{t(`network.kinds.${option}`)}</SelectItem>)}</SelectContent>
          </Select>
        )}
      </FormField>
      <ToggleField
        title={t('network.editor.dhcp')}
        description={t('network.editor.dhcpCopy')}
        control={<Switch checked={dhcp} onCheckedChange={setDhcp} aria-label={t('network.editor.dhcp')} />}
      />
      {!dhcp ? (
        <>
          <FormField label={t('network.editor.address')}>
            {(id) => <Input id={id} className="font-mono" value={address} onChange={(event) => setAddress(event.target.value)} placeholder="192.168.1.20/24" required />}
          </FormField>
          <FormField label={t('network.editor.gateway')}>
            {(id) => <Input id={id} className="font-mono" value={gateway} onChange={(event) => setGateway(event.target.value)} />}
          </FormField>
          <FormField label={t('network.editor.dns')}>
            {(id) => <Input id={id} className="font-mono" value={dns} onChange={(event) => setDns(event.target.value)} />}
          </FormField>
        </>
      ) : null}
      {kind === 'vlan' ? (
        <div className="grid gap-4 sm:grid-cols-2">
          <FormField label={t('network.editor.parent')}>
            {(id) => <Input id={id} value={parent} onChange={(event) => setParent(event.target.value)} required />}
          </FormField>
          <FormField label={t('network.editor.vlanId')}>
            {(id) => <Input id={id} type="number" min={1} max={4094} value={vlanId} onChange={(event) => setVlanId(event.target.value)} required />}
          </FormField>
        </div>
      ) : null}
      {kind === 'bridge' ? (
        <FormField label={t('network.editor.ports')}>
          {(id) => <Input id={id} value={ports} onChange={(event) => setPorts(event.target.value)} placeholder="eth0, eth1" />}
        </FormField>
      ) : null}
      {kind === 'wireguard' ? (
        <FormField label={t('network.editor.listenPort')}>
          {(id) => <Input id={id} type="number" min={1} max={65535} value={listenPort} onChange={(event) => setListenPort(event.target.value)} />}
        </FormField>
      ) : null}
    </FormDialog>
  )
}

function WifiPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [adding, setAdding] = useState(false)
  const [ssid, setSsid] = useState('')
  const [psk, setPsk] = useState('')
  const [hidden, setHidden] = useState(false)
  const [priority, setPriority] = useState('0')
  const client = useQuery({ queryKey: ['settings', 'wifi.client'], queryFn: () => api<WifiClient>('/api/v1/settings/wifi.client') })
  const networks = useQuery({ queryKey: ['wifi-networks'], queryFn: () => api<WifiNetwork[]>('/api/v1/wifi/client/networks') })
  const toggle = useMutationFeedback<TaskAccepted, boolean>({
    mutationFn: (enabled) => api<TaskAccepted>('/api/v1/settings/wifi.client.enabled', json('PUT', enabled)),
    success: (_data, enabled) => t(enabled ? 'network.wifi.clientEnabled' : 'network.wifi.clientDisabled'),
    failure: t('network.wifi.client'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['settings', 'wifi.client'] }),
  })
  const remove = useMutationFeedback<void, string>({
    mutationFn: (name) => api<void>(`/api/v1/wifi/client/networks/${encodeURIComponent(name)}`, { method: 'DELETE' }),
    success: (_data, name) => t('network.wifi.removed', { name }),
    failure: t('network.wifi.remove', { name: '' }),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['wifi-networks'] }),
  })
  const addNetwork = () => api<WifiNetwork>('/api/v1/wifi/client/networks', json('POST', { ssid, ...(psk ? { psk } : {}), hidden, priority: Number(priority) }))
    .then(() => {
      // Every field resets, not just the two that used to: a reopened dialog
      // showing the previous network's priority is a value nobody chose.
      setSsid(''); setPsk(''); setHidden(false); setPriority('0')
      return queryClient.invalidateQueries({ queryKey: ['wifi-networks'] })
    })
  const clientState = client.isPending ? 'common.states.pending' : client.isError ? 'common.states.unknown' : client.data?.enabled ? 'common.states.enabled' : 'common.states.disabled'
  const panelError = client.error ?? networks.error

  return (
    <>
      <div className="flex flex-wrap items-center gap-3">
        <StatusBadge tone={client.isPending || client.isError ? 'warning' : client.data?.enabled ? 'success' : 'neutral'}>{t(clientState)}</StatusBadge>
        <span className="font-mono text-sm text-muted-foreground">{client.data?.interface ?? 'wlan0'}</span>
        <div className="ml-auto flex items-center gap-3">
          <Switch checked={client.data?.enabled ?? false} onCheckedChange={(value) => toggle.mutate(value)} aria-label={t('network.wifi.client')} />
          <Button size="sm" variant="outline" onClick={() => setAdding(true)}><Plus />{t('network.wifi.add')}</Button>
        </div>
      </div>
      {panelError ? <Callout tone="danger" title={failureDetail(panelError, t('common.requestFailed'))} /> : null}
      <CollectionPanel>
        <DataTable<WifiNetwork>
          rows={networks.data}
          rowKey={(entry) => entry.ssid}
          isPending={networks.isPending}
          empty={t('network.wifi.empty')}
          columns={[
            { id: 'ssid', header: 'SSID', cell: (entry) => <span className="font-mono">{entry.ssid}{entry.hidden ? <span className="text-muted-foreground"> · {t('network.wifi.hidden')}</span> : null}</span> },
            { id: 'security', header: t('network.wifi.security'), cell: (entry) => t(entry.psk ? 'network.wifi.wpa' : 'network.wifi.open') },
            { id: 'credential', header: t('network.wifi.credential'), cell: (entry) => <StatusBadge>{t(entry.psk ? 'network.wifi.saved' : 'network.wifi.noCredential')}</StatusBadge> },
            { id: 'auto', header: t('network.wifi.auto'), cell: (entry) => t('network.wifi.priorityValue', { priority: entry.priority }) },
            { id: 'actions', header: '', align: 'end', cell: (entry) => (
              <ConfirmDialog
                trigger={<Button type="button" size="sm" variant="destructive" aria-label={t('network.wifi.remove', { name: entry.ssid })}><Trash2 />{t('network.wifi.remove', { name: entry.ssid })}</Button>}
                title={t('network.wifi.remove', { name: entry.ssid })}
                description={t('network.wifi.removeCopy', { name: entry.ssid })}
                confirmLabel={t('network.wifi.remove', { name: entry.ssid })}
                success={t('network.wifi.removed', { name: entry.ssid })}
                failure={t('network.wifi.remove', { name: entry.ssid })}
                onConfirm={() => remove.mutateAsync(entry.ssid)}
              />
            ) },
          ]}
        />
      </CollectionPanel>
      <TaskProgress taskId={toggle.data?.taskId} />
      <FormDialog
        open={adding}
        onOpenChange={setAdding}
        title={t('network.wifi.add')}
        description={t('network.wifi.addCopy')}
        submitLabel={t('common.actions.add')}
        success={t('network.wifi.added', { name: ssid })}
        failure={t('network.wifi.add')}
        onSubmit={addNetwork}
      >
        <FormField label="SSID">
          {(id) => <Input id={id} value={ssid} onChange={(event) => setSsid(event.target.value)} required />}
        </FormField>
        <FormField label={t('network.wifi.password')}>
          {(id) => <Input id={id} type="password" minLength={8} value={psk} onChange={(event) => setPsk(event.target.value)} />}
        </FormField>
        <FormField label={t('network.wifi.priority')}>
          {(id) => <Input id={id} type="number" value={priority} onChange={(event) => setPriority(event.target.value)} />}
        </FormField>
        <ToggleField title={t('network.wifi.hidden')} control={<Switch checked={hidden} onCheckedChange={setHidden} aria-label={t('network.wifi.hidden')} />} />
      </FormDialog>
    </>
  )
}

function WireguardPanel({ configured }: { configured: Record<string, InterfaceConfig> }) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const tunnels = useMemo(() => Object.entries(configured).filter(([, value]) => value.kind === 'wireguard'), [configured])
  const [selected, setSelected] = useState(tunnels[0]?.[0] ?? '')
  const iface = selected || tunnels[0]?.[0] || ''
  const tunnel = configured[iface]
  const [adding, setAdding] = useState(false)
  const [publicKey, setPublicKey] = useState('')
  const [allowedIps, setAllowedIps] = useState('')
  const [endpoint, setEndpoint] = useState('')
  const peers = useQuery({ queryKey: ['wireguard-peers', iface], queryFn: () => api<WireguardPeer[]>(`/api/v1/network/${encodeURIComponent(iface)}/peers`), enabled: Boolean(iface) })
  const remove = useMutationFeedback<void, string>({
    mutationFn: (key) => api<void>(`/api/v1/network/${encodeURIComponent(iface)}/peers/${encodeURIComponent(key)}`, { method: 'DELETE' }),
    success: t('network.wireguard.removed'),
    failure: t('network.wireguard.remove'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['wireguard-peers', iface] }),
  })
  const rotate = useMutationFeedback<WireguardRotation>({
    mutationFn: () => api<WireguardRotation>(`/api/v1/actions/wireguard/${encodeURIComponent(iface)}/rotate-key`, { method: 'POST' }),
    success: t('network.wireguard.rotatedKey'),
    failure: t('network.wireguard.rotate'),
  })
  const addPeer = () => api<WireguardPeer>(`/api/v1/network/${encodeURIComponent(iface)}/peers`, json('POST', { publicKey, allowedIps: splitList(allowedIps), ...(endpoint ? { endpoint } : {}) }))
    .then(() => {
      setPublicKey(''); setAllowedIps(''); setEndpoint('')
      return queryClient.invalidateQueries({ queryKey: ['wireguard-peers', iface] })
    })

  if (tunnels.length === 0) {
    return <Panel><p className="flex items-center justify-center gap-2 py-6 text-sm text-muted-foreground"><KeyRound className="size-4" />{t('network.wireguard.empty')}</p></Panel>
  }
  return (
    <>
      {tunnels.length > 1 ? (
        <Select value={iface} onValueChange={(value) => { if (value) setSelected(value) }}>
          <SelectTrigger className="w-56" aria-label={t('network.wireguard.tunnel')}><SelectValue /></SelectTrigger>
          <SelectContent>{tunnels.map(([name]) => <SelectItem value={name} key={name}>{name}</SelectItem>)}</SelectContent>
        </Select>
      ) : null}
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <MetricCard label={t('network.wireguard.tunnel')} mono value={`${iface} · ${tunnel?.static?.address ?? t('common.notAvailable')}`} caption={peers.data?.length ? t('network.wireguard.peerCount', { count: peers.data.length }) : t('network.wireguard.noPeers')} />
        <MetricCard label={t('network.wireguard.listen')} mono value={tunnel?.wireguard?.listenPort ? `${tunnel.wireguard.listenPort}/udp` : t('common.notAvailable')} caption={t('network.wireguard.listenCopy')} />
        <MetricCard label={t('network.wireguard.publicKey')} mono value={<span className="text-sm break-all">{rotate.data?.publicKey ?? t('network.wireguard.keyHidden')}</span>} caption={t('network.wireguard.keyCopy')} />
      </div>
      {peers.error ? <Callout tone="danger" title={failureDetail(peers.error, t('common.requestFailed'))} /> : null}
      <CollectionPanel
        title={t('network.wireguard.peers')}
        action={<Button size="sm" variant="outline" onClick={() => setAdding(true)}><Plus />{t('network.wireguard.add')}</Button>}
      >
        <DataTable<WireguardPeer>
          rows={peers.data}
          rowKey={(peer) => peer.publicKey}
          isPending={peers.isPending}
          empty={t('network.wireguard.noPeers')}
          columns={[
            { id: 'key', header: t('network.wireguard.publicKey'), cell: (peer) => <span className="font-mono text-[0.8125rem] break-all">{peer.publicKey}</span> },
            { id: 'allowed', header: t('network.wireguard.allowed'), cell: (peer) => <span className="font-mono text-[0.8125rem]">{peer.allowedIps.join(', ')}</span> },
            { id: 'endpoint', header: t('network.wireguard.endpoint'), cell: (peer) => <span className="font-mono text-[0.8125rem]">{peer.endpoint ?? '—'}</span> },
            { id: 'keepalive', header: t('network.wireguard.keepalive'), cell: (peer) => peer.persistentKeepalive ? t('network.wireguard.keepaliveValue', { seconds: peer.persistentKeepalive }) : '—' },
            { id: 'actions', header: '', align: 'end', cell: (peer) => (
              <ConfirmDialog
                trigger={<Button type="button" size="sm" variant="destructive" aria-label={t('network.wireguard.remove')}><Trash2 />{t('network.wireguard.remove')}</Button>}
                title={t('network.wireguard.remove')}
                description={t('network.wireguard.removeCopy')}
                confirmLabel={t('network.wireguard.remove')}
                success={t('network.wireguard.removed')}
                failure={t('network.wireguard.remove')}
                onConfirm={() => remove.mutateAsync(peer.publicKey)}
              />
            ) },
          ]}
        />
      </CollectionPanel>
      <Panel className="border-destructive">
        <div className="flex flex-wrap items-center justify-between gap-4">
          <div className="grid gap-0.5">
            <strong className="text-sm font-medium">{t('network.wireguard.rotate')}</strong>
            <span className="text-sm text-muted-foreground">{t('network.wireguard.rotateCopy')}</span>
          </div>
          <ConfirmDialog
            trigger={<Button type="button" size="sm" variant="destructive">{t('network.wireguard.rotate')}</Button>}
            title={t('network.wireguard.rotate')}
            description={t('network.wireguard.rotateConfirm')}
            confirmLabel={t('network.wireguard.rotate')}
            success={t('network.wireguard.rotatedKey')}
            failure={t('network.wireguard.rotate')}
            onConfirm={() => rotate.mutateAsync()}
          />
        </div>
      </Panel>
      <FormDialog
        open={adding}
        onOpenChange={setAdding}
        title={t('network.wireguard.add')}
        description={t('network.wireguard.addCopy', { iface })}
        submitLabel={t('common.actions.add')}
        success={t('network.wireguard.added')}
        failure={t('network.wireguard.add')}
        onSubmit={addPeer}
      >
        <FormField label={t('network.wireguard.publicKey')}>
          {(id) => <Input id={id} className="font-mono" value={publicKey} onChange={(event) => setPublicKey(event.target.value)} required />}
        </FormField>
        <FormField label={t('network.wireguard.allowed')}>
          {(id) => <Input id={id} className="font-mono" value={allowedIps} onChange={(event) => setAllowedIps(event.target.value)} placeholder="10.10.0.0/24" required />}
        </FormField>
        <FormField label={t('network.wireguard.endpoint')}>
          {(id) => <Input id={id} className="font-mono" value={endpoint} onChange={(event) => setEndpoint(event.target.value)} placeholder="vpn.example.com:51820" />}
        </FormField>
      </FormDialog>
    </>
  )
}

function summarize(items: unknown[] | undefined) {
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

function splitList(value: string) { return value.split(',').map((item) => item.trim()).filter(Boolean) }
