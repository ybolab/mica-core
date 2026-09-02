import { useMemo, useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Cable, KeyRound, Plus, RefreshCw, Trash2, Wifi } from 'lucide-react'
import { api, errorMessage, json } from '@/shared/lib/http'
import { configuredSummary, networkRows, type NetworkRow } from '@/lib/network'
import type { NetworkOverview, TaskAccepted } from '@/lib/types'
import { Page, PageHeader, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { TaskProgress } from '@/shared/components/task-progress'
import { Button } from '@/shared/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
import { Input } from '@/shared/components/ui/input'
import { Label } from '@/shared/components/ui/label'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/shared/components/ui/select'
import { Switch } from '@/shared/components/ui/switch'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/shared/components/ui/tabs'
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from '@/shared/components/ui/alert-dialog'
import { ObservedNetworkPanel } from '@/features/network/observed-network-panel'

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
  const [editing, setEditing] = useState<{ name?: string; value?: InterfaceConfig }>()
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const rows = networkRows(network.data?.configured, network.data?.observed.interfaces)

  return (
    <Page>
      <PageHeader title={t('network.title')} action={<Button variant="outline" onClick={() => network.refetch()} disabled={network.isFetching}><RefreshCw className={network.isFetching ? 'animate-spin' : ''} />{t('common.actions.refresh')}</Button>} />
      {network.isPending ? <p className="callout warning" role="status">{t('common.states.pending')}</p> : null}
      {network.error ? <p className="callout error" role="alert">{errorMessage(network.error, t('common.requestFailed'))}</p> : null}
      {network.data?.observed.error ? <p className="callout warning" role="status">{network.data.observed.error}</p> : null}
      <Tabs defaultValue="interfaces">
        <TabsList aria-label={t('network.title')}><TabsTrigger value="interfaces">{t('network.tabs.interfaces')}</TabsTrigger><TabsTrigger value="wifi">{t('network.tabs.wifi')}</TabsTrigger><TabsTrigger value="wireguard">{t('network.tabs.wireguard')}</TabsTrigger><TabsTrigger value="observed">{t('network.tabs.observed')}</TabsTrigger></TabsList>
        <TabsContent value="interfaces" className="tab-panel">
          <div className="toolbar"><span>{t('network.interfaceCount', { count: rows.length })}</span><Button size="sm" onClick={() => setEditing({})}><Plus />{t('network.actions.addInterface')}</Button></div>
          <Surface className="surface-compact"><div className="data-table-wrap"><table className="data-table"><thead><tr><th>{t('network.table.interface')}</th><th>{t('network.table.configured')}</th><th>{t('network.table.observedType')}</th><th>{t('network.table.operational')}</th><th>{t('network.table.addresses')}</th><th /></tr></thead><tbody>{rows.map((row) => <InterfaceRow row={row} key={row.name} onEdit={() => setEditing({ name: row.name, value: row.configured as InterfaceConfig | undefined })} />)}</tbody></table>{!network.isPending && !network.isError && rows.length === 0 ? <p className="empty"><Cable />{t('network.noInterfaces')}</p> : null}</div></Surface>
        </TabsContent>
        <TabsContent value="wifi" className="tab-panel"><WifiPanel /></TabsContent>
        <TabsContent value="wireguard" className="tab-panel"><WireguardPanel configured={(network.data?.configured ?? {}) as Record<string, InterfaceConfig>} /></TabsContent>
        <TabsContent value="observed" className="tab-panel"><ObservedNetworkPanel /></TabsContent>
      </Tabs>
      <InterfaceDialog key={editing ? (editing.name ?? 'new') : 'closed'} edit={editing} onClose={() => setEditing(undefined)} />
    </Page>
  )
}

function InterfaceRow({ row, onEdit }: { row: NetworkRow; onEdit: () => void }) {
  const { t } = useTranslation()
  const iface = row.observed
  const summaryLabels = { notConfigured: t('network.summary.notConfigured'), physical: t('network.summary.physical'), dhcp: t('network.summary.dhcp'), static: t('network.summary.static'), noAddressing: t('network.summary.noAddressing'), format: (kind: string, method: string) => t('network.summary.value', { kind, method }) }
  const online = ['routable', 'carrier', 'degraded'].includes(iface?.operationalState ?? '')
  return <tr><td><div className="cell-primary"><strong>{row.name}</strong><small>{t('network.index', { index: iface?.index ?? t('common.notAvailable') })}</small></div></td><td>{configuredSummary(row.configured, summaryLabels)}</td><td>{iface?.kind ?? iface?.type ?? iface?.driver ?? t('common.notAvailable')}</td><td><StatusBadge tone={online ? 'success' : 'warning'}>{iface?.operationalState ?? t('network.notObserved')}</StatusBadge></td><td className="mono-cell">{summarize(iface?.addresses)}</td><td className="text-right"><Button size="sm" variant="outline" onClick={onEdit}>{t('common.actions.edit')}</Button></td></tr>
}

function InterfaceDialog({ edit, onClose }: { edit?: { name?: string; value?: InterfaceConfig }; onClose: () => void }) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const original = edit?.value
  const [name, setName] = useState(edit?.name ?? '')
  const [kind, setKind] = useState<NonNullable<InterfaceConfig['kind']>>(original?.kind ?? 'physical')
  const [dhcp, setDhcp] = useState(original?.dhcp ?? true)
  const [address, setAddress] = useState(original?.static?.address ?? '')
  const [gateway, setGateway] = useState(original?.static?.gateway ?? '')
  const [dns, setDns] = useState(original?.static?.dns.join(', ') ?? '')
  const [parent, setParent] = useState(original?.vlan?.parent ?? 'eth0')
  const [vlanId, setVlanId] = useState(String(original?.vlan?.id ?? 100))
  const [ports, setPorts] = useState(original?.bridge?.ports.join(', ') ?? '')
  const [listenPort, setListenPort] = useState(String(original?.wireguard?.listenPort ?? 51820))
  const save = useMutation({
    mutationFn: () => {
      const value: InterfaceConfig = { dhcp }
      if (kind !== 'physical') value.kind = kind
      if (!dhcp) value.static = { address, ...(gateway ? { gateway } : {}), dns: splitList(dns) }
      if (kind === 'vlan') value.vlan = { parent, id: Number(vlanId) }
      if (kind === 'bridge') value.bridge = { ports: splitList(ports) }
      if (kind === 'wireguard') value.wireguard = { listenPort: Number(listenPort), peers: original?.wireguard?.peers ?? [] }
      return api<TaskAccepted>(`/api/v1/network/${encodeURIComponent(name)}`, json('PUT', value))
    },
    onSuccess: () => { void queryClient.invalidateQueries({ queryKey: ['network'] }); onClose() },
  })
  const remove = useMutation({ mutationFn: () => api<void>(`/api/v1/network/${encodeURIComponent(name)}`, { method: 'DELETE' }), onSuccess: () => { void queryClient.invalidateQueries({ queryKey: ['network'] }); onClose() } })
  const submit = (event: FormEvent) => { event.preventDefault(); save.mutate() }

  return <Dialog open={Boolean(edit)} onOpenChange={(open) => { if (!open) onClose() }}><DialogContent><DialogHeader><DialogTitle>{t(edit?.name ? 'network.editor.editTitle' : 'network.editor.addTitle')}</DialogTitle><DialogDescription>{t('network.editor.description')}</DialogDescription></DialogHeader><form className="stack" onSubmit={submit}>
    <div className="field"><Label htmlFor="iface-name">{t('network.editor.name')}</Label><Input id="iface-name" value={name} onChange={(event) => setName(event.target.value)} disabled={Boolean(edit?.name)} required /></div>
    <div className="field"><Label htmlFor="iface-kind">{t('network.editor.kind')}</Label><Select value={kind} onValueChange={(value) => setKind(value as typeof kind)}><SelectTrigger id="iface-kind"><SelectValue /></SelectTrigger><SelectContent>{(['physical', 'vlan', 'bridge', 'wireguard'] as const).map((value) => <SelectItem key={value} value={value}>{t(`network.kinds.${value}`)}</SelectItem>)}</SelectContent></Select></div>
    <div className="ui-selector"><div><strong>{t('network.editor.dhcp')}</strong><small>{t('network.editor.dhcpCopy')}</small></div><Switch checked={dhcp} onCheckedChange={setDhcp} aria-label={t('network.editor.dhcp')} /></div>
    {!dhcp ? <><div className="field"><Label htmlFor="iface-address">{t('network.editor.address')}</Label><Input id="iface-address" value={address} onChange={(event) => setAddress(event.target.value)} placeholder="192.168.1.20/24" required /></div><div className="content-grid"><div className="field"><Label htmlFor="iface-gateway">{t('network.editor.gateway')}</Label><Input id="iface-gateway" value={gateway} onChange={(event) => setGateway(event.target.value)} /></div><div className="field"><Label htmlFor="iface-dns">{t('network.editor.dns')}</Label><Input id="iface-dns" value={dns} onChange={(event) => setDns(event.target.value)} /></div></div></> : null}
    {kind === 'vlan' ? <div className="content-grid"><div className="field"><Label htmlFor="iface-parent">{t('network.editor.parent')}</Label><Input id="iface-parent" value={parent} onChange={(event) => setParent(event.target.value)} required /></div><div className="field"><Label htmlFor="iface-vlan-id">{t('network.editor.vlanId')}</Label><Input id="iface-vlan-id" type="number" min={1} max={4094} value={vlanId} onChange={(event) => setVlanId(event.target.value)} required /></div></div> : null}
    {kind === 'bridge' ? <div className="field"><Label htmlFor="iface-ports">{t('network.editor.ports')}</Label><Input id="iface-ports" value={ports} onChange={(event) => setPorts(event.target.value)} placeholder="eth0, eth1" /></div> : null}
    {kind === 'wireguard' ? <div className="field"><Label htmlFor="iface-listen-port">{t('network.editor.listenPort')}</Label><Input id="iface-listen-port" type="number" min={1} max={65535} value={listenPort} onChange={(event) => setListenPort(event.target.value)} /></div> : null}
    {save.error || remove.error ? <p className="callout error">{errorMessage(save.error ?? remove.error, t('common.requestFailed'))}</p> : null}
    <DialogFooter>{edit?.name ? <ConfirmAction label={t('network.editor.delete')} description={t('network.editor.deleteCopy', { name })} onConfirm={() => remove.mutate()} disabled={remove.isPending} destructive /> : null}<Button type="button" variant="outline" onClick={onClose}>{t('common.actions.cancel')}</Button><Button type="submit" disabled={save.isPending}>{t('common.actions.save')}</Button></DialogFooter>
  </form></DialogContent></Dialog>
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
  const toggle = useMutation({ mutationFn: (enabled: boolean) => api<TaskAccepted>('/api/v1/settings/wifi.client.enabled', json('PUT', enabled)), onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'wifi.client'] }) })
  const add = useMutation({ mutationFn: () => api<WifiNetwork>('/api/v1/wifi/client/networks', json('POST', { ssid, ...(psk ? { psk } : {}), hidden, priority: Number(priority) })), onSuccess: () => { setAdding(false); setSsid(''); setPsk(''); void queryClient.invalidateQueries({ queryKey: ['wifi-networks'] }) } })
  const remove = useMutation({ mutationFn: (name: string) => api<void>(`/api/v1/wifi/client/networks/${encodeURIComponent(name)}`, { method: 'DELETE' }), onSuccess: () => queryClient.invalidateQueries({ queryKey: ['wifi-networks'] }) })
  const clientState = client.isPending ? 'common.states.pending' : client.isError ? 'common.states.unknown' : client.data?.enabled ? 'common.states.enabled' : 'common.states.disabled'
  const panelError = client.error ?? networks.error ?? toggle.error ?? remove.error
  return <><div className="toolbar"><div className="service-state"><StatusBadge tone={client.isPending || client.isError ? 'warning' : client.data?.enabled ? 'success' : 'neutral'}>{t(clientState)}</StatusBadge><span>{client.data?.interface ?? 'wlan0'}</span><Switch checked={client.data?.enabled ?? false} disabled={client.isPending || client.isError || toggle.isPending} onCheckedChange={(value) => toggle.mutate(value)} aria-label={t('network.wifi.enabled')} /></div><Button size="sm" onClick={() => setAdding(true)}><Plus />{t('network.wifi.add')}</Button></div>
    {panelError ? <p className="callout error" role="alert">{errorMessage(panelError, t('common.requestFailed'))}</p> : null}
    <Surface className="surface-compact"><div className="data-table-wrap"><table className="data-table"><thead><tr><th>SSID</th><th>{t('network.wifi.security')}</th><th>{t('network.wifi.hidden')}</th><th>{t('network.wifi.priority')}</th><th /></tr></thead><tbody>{networks.data?.map((item) => <tr key={item.ssid}><td><strong>{item.ssid}</strong></td><td>{item.psk ? 'WPA2/WPA3' : t('network.wifi.open')}</td><td>{t(item.hidden ? 'common.states.enabled' : 'common.states.disabled')}</td><td>{item.priority}</td><td className="text-right"><ConfirmAction iconOnly label={t('network.wifi.remove', { name: item.ssid })} description={t('network.wifi.removeCopy', { name: item.ssid })} onConfirm={() => remove.mutate(item.ssid)} disabled={remove.isPending} destructive /></td></tr>)}</tbody></table>{!networks.isPending && !networks.isError && networks.data?.length === 0 ? <p className="empty"><Wifi />{t('network.wifi.empty')}</p> : null}</div></Surface>
    <TaskProgress taskId={toggle.data?.taskId} />
    <Dialog open={adding} onOpenChange={setAdding}><DialogContent><DialogHeader><DialogTitle>{t('network.wifi.add')}</DialogTitle><DialogDescription>{t('network.wifi.addCopy')}</DialogDescription></DialogHeader><form className="stack" onSubmit={(event) => { event.preventDefault(); add.mutate() }}><div className="field"><Label htmlFor="wifi-ssid">SSID</Label><Input id="wifi-ssid" value={ssid} onChange={(event) => setSsid(event.target.value)} required /></div><div className="field"><Label htmlFor="wifi-password">{t('network.wifi.password')}</Label><Input id="wifi-password" type="password" minLength={8} value={psk} onChange={(event) => setPsk(event.target.value)} /></div><div className="content-grid"><div className="field"><Label htmlFor="wifi-priority">{t('network.wifi.priority')}</Label><Input id="wifi-priority" type="number" value={priority} onChange={(event) => setPriority(event.target.value)} /></div><div className="ui-selector"><strong>{t('network.wifi.hidden')}</strong><Switch checked={hidden} onCheckedChange={setHidden} aria-label={t('network.wifi.hidden')} /></div></div>{add.error ? <p className="callout error">{errorMessage(add.error, t('common.requestFailed'))}</p> : null}<DialogFooter><Button type="button" variant="outline" onClick={() => setAdding(false)}>{t('common.actions.cancel')}</Button><Button type="submit" disabled={add.isPending}>{t('common.actions.add')}</Button></DialogFooter></form></DialogContent></Dialog>
  </>
}

function WireguardPanel({ configured }: { configured: Record<string, InterfaceConfig> }) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const tunnels = useMemo(() => Object.entries(configured).filter(([, value]) => value.kind === 'wireguard'), [configured])
  const [selected, setSelected] = useState(tunnels[0]?.[0] ?? '')
  const iface = selected || tunnels[0]?.[0] || ''
  const [adding, setAdding] = useState(false)
  const [publicKey, setPublicKey] = useState('')
  const [allowedIps, setAllowedIps] = useState('')
  const [endpoint, setEndpoint] = useState('')
  const peers = useQuery({ queryKey: ['wireguard-peers', iface], queryFn: () => api<WireguardPeer[]>(`/api/v1/network/${encodeURIComponent(iface)}/peers`), enabled: Boolean(iface) })
  const add = useMutation({ mutationFn: () => api<WireguardPeer>(`/api/v1/network/${encodeURIComponent(iface)}/peers`, json('POST', { publicKey, allowedIps: splitList(allowedIps), ...(endpoint ? { endpoint } : {}) })), onSuccess: () => { setAdding(false); setPublicKey(''); setAllowedIps(''); setEndpoint(''); void queryClient.invalidateQueries({ queryKey: ['wireguard-peers', iface] }) } })
  const remove = useMutation({ mutationFn: (key: string) => api<void>(`/api/v1/network/${encodeURIComponent(iface)}/peers/${encodeURIComponent(key)}`, { method: 'DELETE' }), onSuccess: () => queryClient.invalidateQueries({ queryKey: ['wireguard-peers', iface] }) })
  const rotate = useMutation({ mutationFn: () => api<WireguardRotation>(`/api/v1/actions/wireguard/${encodeURIComponent(iface)}/rotate-key`, { method: 'POST' }) })
  if (tunnels.length === 0) return <Surface><div className="empty"><KeyRound />{t('network.wireguard.empty')}</div></Surface>
  return <><div className="toolbar"><Select value={iface} onValueChange={(value) => { if (value) setSelected(value) }}><SelectTrigger className="w-56"><SelectValue /></SelectTrigger><SelectContent>{tunnels.map(([name]) => <SelectItem value={name} key={name}>{name}</SelectItem>)}</SelectContent></Select><div className="table-actions"><ConfirmAction label={t('network.wireguard.rotate')} description={t('network.wireguard.rotateCopy', { iface })} onConfirm={() => rotate.mutate()} disabled={rotate.isPending} /><Button size="sm" onClick={() => setAdding(true)}><Plus />{t('network.wireguard.add')}</Button></div></div>
    {peers.error || remove.error || rotate.error ? <p className="callout error" role="alert">{errorMessage(peers.error ?? remove.error ?? rotate.error, t('common.requestFailed'))}</p> : null}
    {rotate.data ? <p className="callout success" role="status">{t('network.wireguard.rotated')} <code>{rotate.data.publicKey}</code></p> : null}
    <Surface className="surface-compact"><div className="data-table-wrap"><table className="data-table"><thead><tr><th>{t('network.wireguard.publicKey')}</th><th>{t('network.wireguard.allowed')}</th><th>{t('network.wireguard.endpoint')}</th><th /></tr></thead><tbody>{peers.data?.map((peer) => <tr key={peer.publicKey}><td className="mono-cell">{peer.publicKey}</td><td className="mono-cell">{peer.allowedIps.join(', ')}</td><td>{peer.endpoint ?? t('common.notAvailable')}</td><td className="text-right"><ConfirmAction iconOnly label={t('network.wireguard.remove')} description={t('network.wireguard.removeCopy')} onConfirm={() => remove.mutate(peer.publicKey)} disabled={remove.isPending} destructive /></td></tr>)}</tbody></table>{!peers.isPending && !peers.isError && peers.data?.length === 0 ? <p className="empty">{t('network.wireguard.noPeers')}</p> : null}</div></Surface>
    <Dialog open={adding} onOpenChange={setAdding}><DialogContent><DialogHeader><DialogTitle>{t('network.wireguard.add')}</DialogTitle><DialogDescription>{t('network.wireguard.addCopy', { iface })}</DialogDescription></DialogHeader><form className="stack" onSubmit={(event) => { event.preventDefault(); add.mutate() }}><div className="field"><Label htmlFor="peer-key">{t('network.wireguard.publicKey')}</Label><Input id="peer-key" value={publicKey} onChange={(event) => setPublicKey(event.target.value)} required /></div><div className="field"><Label htmlFor="peer-allowed">{t('network.wireguard.allowed')}</Label><Input id="peer-allowed" value={allowedIps} onChange={(event) => setAllowedIps(event.target.value)} placeholder="10.10.0.0/24" required /></div><div className="field"><Label htmlFor="peer-endpoint">{t('network.wireguard.endpoint')}</Label><Input id="peer-endpoint" value={endpoint} onChange={(event) => setEndpoint(event.target.value)} placeholder="vpn.example.com:51820" /></div>{add.error ? <p className="callout error">{errorMessage(add.error, t('common.requestFailed'))}</p> : null}<DialogFooter><Button type="button" variant="outline" onClick={() => setAdding(false)}>{t('common.actions.cancel')}</Button><Button type="submit" disabled={add.isPending}>{t('common.actions.add')}</Button></DialogFooter></form></DialogContent></Dialog>
  </>
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

function ConfirmAction({ label, description, onConfirm, disabled, destructive = false, iconOnly = false }: { label: string; description: string; onConfirm: () => void; disabled?: boolean; destructive?: boolean; iconOnly?: boolean }) {
  const { t } = useTranslation()
  return <AlertDialog><AlertDialogTrigger render={<Button type="button" size={iconOnly ? 'icon-sm' : 'sm'} variant={destructive ? 'destructive' : 'outline'} disabled={disabled} aria-label={label} />}>{iconOnly ? <Trash2 /> : <RefreshCw />}{iconOnly ? null : label}</AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{label}</AlertDialogTitle><AlertDialogDescription>{description}</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant={destructive ? 'destructive' : 'default'} onClick={onConfirm}>{label}</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog>
}
