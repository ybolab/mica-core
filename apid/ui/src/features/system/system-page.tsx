import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useRouterState } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Download, ExternalLink, PackageSearch, Power, RefreshCcw, Settings2, Upload } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import type { TaskAccepted, UiStatus } from '@/lib/types'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Section, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { PlannedNotice } from '@/shared/simulation/planned'
import { Field } from '@/shared/components/field'
import { Input } from '@/shared/components/ui/input'
import { Status } from '@/components/ui/status'
import { Switch } from '@/shared/components/ui/switch'
import { TaskProgress } from '@/shared/components/task-progress'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/shared/components/ui/tabs'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { useSimulation } from '@/shared/simulation/simulation-provider'
import { InformationPanel } from '@/features/system/information-panel'
import { TimePanel } from '@/features/system/time-panel'
import { StoragePanel } from '@/features/system/storage-panel'
import { DiagnosticsPanel } from '@/features/system/diagnostics-panel'
import { RollbackPanel } from '@/features/system/rollback-panel'
import { AutomaticUpdatesPanel } from '@/features/system/automatic-updates-panel'
import { CredentialRecoveryPanel } from '@/features/recovery/credential-recovery-panel'
import { ResetPanel } from '@/features/recovery/reset-panel'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/shared/components/ui/alert-dialog'

export function SystemPage() {
  const { t } = useTranslation()
  const hash = useRouterState({ select: (state) => state.location.hash })
  // The prototype has six tabs: recovery is folded into update, general and
  // diagnostics. The old anchor still resolves rather than dropping the reader
  // on the first tab.
  const tabs = ['general', 'information', 'time', 'update', 'storage', 'diagnostics']
  const initialTab = tabs.includes(hash) ? hash : hash === 'recovery' ? 'update' : 'general'
  return (
    <div className="page">
      <header className="page-head"><div><h1>{t('system.title')}</h1></div></header>
      <Tabs key={initialTab} defaultValue={initialTab}>
        <TabsList aria-label={t('system.title')}>
          <TabsTrigger value="general">{t('system.tabs.general')}</TabsTrigger>
          <TabsTrigger value="information">{t('system.tabs.information')}</TabsTrigger>
          <TabsTrigger value="time">{t('system.tabs.time')}</TabsTrigger>
          <TabsTrigger value="update">{t('system.tabs.update')}</TabsTrigger>
          <TabsTrigger value="storage">{t('system.tabs.storage')}</TabsTrigger>
          <TabsTrigger value="diagnostics">{t('system.tabs.diagnostics')}</TabsTrigger>
        </TabsList>
        <TabsContent value="general" className="tab-panel">
          <Section title={t('system.identity.title')} description={t('system.identity.description')}><HostnamePanel /></Section>
          <Section title={t('system.ui.title')} description={t('system.ui.description')}><UiPanel /></Section>
          <Section title={t('system.power.title')} description={t('system.power.description')}><PowerPanel /></Section>
          <Section title={t('system.recovery.reset.title')} description={t('system.recovery.reset.description')} className="danger-section"><ResetPanel /></Section>
        </TabsContent>
        <TabsContent value="information" className="tab-panel"><InformationPanel /></TabsContent>
        <TabsContent value="time" className="tab-panel"><TimePanel /></TabsContent>
        <TabsContent value="update" className="tab-panel">
          <UpdatePanel />
          <UpdateChecks />
          <UpdateActions />
          <Section title={t('system.update.automaticTitle')} description={t('system.update.automaticDescription')}><AutomaticUpdatesPanel /></Section>
          <Section title={t('system.update.manualTitle')} description={t('system.update.manualDescription')}><ManualUpdate /></Section>
          <Section title={t('system.backup.title')} description={t('system.backup.description')}><ConfigBackup /></Section>
          <Section title={t('system.recovery.additionTitle')} description={t('system.recovery.addition')} className="marked-addition">
            <div className="stack"><RollbackPanel /><CredentialRecoveryPanel /></div>
          </Section>
        </TabsContent>
        <TabsContent value="storage" className="tab-panel"><StoragePanel /></TabsContent>
        <TabsContent value="diagnostics" className="tab-panel">
          <DiagnosticsPanel />
          <Section title={t('system.support.title')} description={t('system.support.description')}><SupportAccess /></Section>
        </TabsContent>
      </Tabs>
      <SimulationNotice scope={t('system.simulationScope')} />
    </div>
  )
}

// The update state document `GET /api/v1/update` answers; only the members
// this read-only panel renders are typed.
interface UpdateStateDoc {
  lifecycle?: {
    state?: string
    reason?: string
    available?: { deploymentId: string; version: string; channel: string }
    deploymentId?: string
    last_check?: string
    client?: { available?: boolean; reason?: string }
    policy_error?: string
    reboot_gate?: { safe?: boolean; reasons?: string[] }
  }
  boot?: { deploymentId: string; kernelId: string; rootfsId: string; contentVerified: boolean; secureBoot: boolean; backend: 'uefi' | 'uboot-fit'; bootVerified: boolean }
  state?: { current: string | null; candidate: string | null; fallback: string | null; highestGeneration: number; failed: string[] }
  deployments?: { id: string; generation: number; version: string; kernelId: string; kernelRelease: string; rootfsId: string; triesLeft: number | null }[]
}

const activeUpdateStates = new Set(['checking', 'downloading', 'installing', 'discarding'])

function useUpdateState() {
  return useQuery({
    queryKey: ['update-state'],
    queryFn: () => api<UpdateStateDoc>('/api/v1/update'),
    refetchInterval: (query) => activeUpdateStates.has(query.state.data?.lifecycle?.state ?? '') ? 1000 : false,
  })
}

export function UpdatePanel() {
  const { t } = useTranslation()
  const status = useUpdateState()
  const lifecycle = status.data?.lifecycle
  const available = lifecycle?.available
  const boot = status.data?.boot
  const deployment = status.data?.state
  const running = status.data?.deployments?.find((entry) => entry.id === boot?.deploymentId)
  return (
    <Card>
      <CardHeader title={t('system.update.title')} description={t('system.update.description')} action={<PackageSearch className="size-5 text-muted-foreground" />} />
      <div className="service-state"><Status ok={!status.isPending && !status.isError && lifecycle?.state !== 'failed' && lifecycle?.state !== 'update-unavailable'}>{status.isPending ? t('system.update.checking') : (lifecycle?.state ?? t('common.states.unknown'))}</Status></div>
      {lifecycle?.reason ? <p className="text-sm text-muted-foreground">{lifecycle.reason}</p> : null}
      <dl className="details">
        {available ? <div><dt>{t('system.update.available')}</dt><dd>{available.version} · {available.deploymentId} ({available.channel})</dd></div> : null}
        {lifecycle?.deploymentId ? <div><dt>{t('system.update.staged')}</dt><dd className="break-all">{lifecycle.deploymentId}</dd></div> : null}
        {boot ? <>
          <div><dt>{t('system.update.running')}</dt><dd className="break-all">{boot.deploymentId}</dd></div>
          <div><dt>{t('system.update.kernel')}</dt><dd className="break-all">{boot.kernelId}</dd></div>
          <div><dt>{t('system.update.rootfs')}</dt><dd className="break-all">{boot.rootfsId}</dd></div>
        </> : null}
        {running ? <div><dt>{t('system.update.version')}</dt><dd>{running.version} · {running.kernelRelease}</dd></div> : null}
        {deployment ? <>
          <div><dt>{t('system.update.generation')}</dt><dd>{deployment.highestGeneration}</dd></div>
          {deployment.current && deployment.current !== boot?.deploymentId ? <div><dt>{t('system.update.confirmed')}</dt><dd className="break-all">{deployment.current}</dd></div> : null}
          {deployment.candidate ? <div><dt>{t('system.update.candidate')}</dt><dd className="break-all">{deployment.candidate}</dd></div> : null}
          {deployment.fallback ? <div><dt>{t('system.update.fallback')}</dt><dd className="break-all">{deployment.fallback}</dd></div> : null}
          {deployment.failed.length ? <div><dt>{t('system.update.failed')}</dt><dd>{deployment.failed.map((id) => <div className="break-all" key={id}>{id}</div>)}</dd></div> : null}
        </> : null}
        {lifecycle?.last_check ? <div><dt>{t('system.update.lastCheck')}</dt><dd>{lifecycle.last_check}</dd></div> : null}
      </dl>
      {deployment?.candidate && deployment.candidate !== boot?.deploymentId ? <p className="callout warning" role="status">{t('system.update.pendingReboot')}</p> : null}
      {lifecycle?.client && lifecycle.client.available === false ? <p className="callout warning" role="status">{t('system.update.clientUnavailable', { reason: lifecycle.client.reason ?? '' })}</p> : null}
      {lifecycle?.policy_error ? <p className="callout error" role="alert">{t('system.update.policyError', { reason: lifecycle.policy_error })}</p> : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

export function UpdateActions() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const status = useUpdateState()
  const action = useMutation({ mutationFn: (name: 'check' | 'fetch' | 'install') => api<unknown>(`/api/v1/update/${name}`, json('POST', name === 'install' ? { deploymentId: status.data?.lifecycle?.deploymentId } : undefined)), onSuccess: () => queryClient.invalidateQueries({ queryKey: ['update-state'] }) })
  const busy = action.isPending || status.isPending || status.isError || activeUpdateStates.has(status.data?.lifecycle?.state ?? '')
  return <Card><CardHeader title={t('system.update.actionsTitle')} description={t('system.update.actionsDescription')} /><div className="flex flex-wrap gap-3"><Button variant="secondary" onClick={() => action.mutate('check')} disabled={busy}>{t('system.update.checkNow')}</Button><Button variant="secondary" onClick={() => action.mutate('fetch')} disabled={busy}>{t('system.update.download')}</Button><Button onClick={() => action.mutate('install')} disabled={busy || !status.data?.lifecycle?.deploymentId}>{t('system.update.install')}</Button></div>{action.error ? <p className="callout error">{errorMessage(action.error, t('common.requestFailed'))}</p> : null}</Card>
}

/// The reset tiers and credential recovery bind REAL routes and are their own
/// area; the two cards beside them are still simulated and stay behind the
/// page's simulation notice.
/// The prototype's check table. Only the rows the device actually answers are
/// rendered; the signature, compatibility and space checks it also shows have
/// no endpoint, and are named as missing rather than invented.
export function UpdateChecks() {
  const { t } = useTranslation()
  const status = useQuery({ queryKey: ['update-state'], queryFn: () => api<UpdateStateDoc>('/api/v1/update') })
  const lifecycle = status.data?.lifecycle
  const gate = lifecycle?.reboot_gate
  const rows = [
    gate ? { id: 'reboot', tone: gate.safe ? 'success' as const : 'warning' as const, tag: t(gate.safe ? 'common.states.available' : 'system.update.checks.blocked'), label: t('system.update.checks.safeToReboot'), value: gate.safe ? t('system.update.gateSafe') : (gate.reasons ?? []).join('; ') } : undefined,
    lifecycle?.client ? { id: 'client', tone: lifecycle.client.available ? 'success' as const : 'danger' as const, tag: t(lifecycle.client.available ? 'common.states.available' : 'common.states.unavailable'), label: t('system.update.checks.client'), value: lifecycle.client.reason ?? t('system.update.checks.clientOk') } : undefined,
    lifecycle?.policy_error ? { id: 'policy', tone: 'danger' as const, tag: t('common.states.failed'), label: t('system.update.checks.policy'), value: lifecycle.policy_error } : undefined,
  ].filter((row) => row !== undefined)
  if (rows.length === 0) return null
  return (
    <Surface className="surface-compact">
      {rows.map((row) => (
        <div className="check-row" key={row.id}>
          <span>{row.label}</span>
          <span><StatusBadge tone={row.tone}>{row.tag}</StatusBadge><span>{row.value}</span></span>
        </div>
      ))}
    </Surface>
  )
}

function ManualUpdate() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [deploymentId, setDeploymentId] = useState('')
  const install = useMutation({
    mutationFn: () => api<unknown>('/api/v1/update/install', json('POST', { deploymentId })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['update-state'] }),
  })
  return <Surface><form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); install.mutate() }}>
    <p className="text-sm text-muted-foreground">{t('system.update.manualFormats')}</p>
    <Field label={t('system.update.deploymentId')}>
      <Input value={deploymentId} onChange={(event) => setDeploymentId(event.target.value)} maxLength={64} pattern="[0-9a-f]{64}" required />
    </Field>
    <Button type="submit" disabled={install.isPending || !/^[0-9a-f]{64}$/.test(deploymentId)}>{t('system.update.install')}</Button>
    {install.isSuccess ? <p className="callout success" role="status">{t('system.update.installAccepted')}</p> : null}
    {install.error ? <p className="callout error" role="alert">{errorMessage(install.error, t('common.requestFailed'))}</p> : null}
  </form></Surface>
}

function ConfigBackup() {
  const { t } = useTranslation()
  return (
    <div className="stack">
      <PlannedNotice>{t('system.backup.planned')}</PlannedNotice>
      <div className="split-grid">
        <Surface>
          <strong>{t('system.backup.exportTitle')}</strong>
          <p className="field-hint">{t('system.backup.exportCopy')}</p>
          <Button variant="outline" size="sm" disabled><Download />{t('system.backup.download')}</Button>
        </Surface>
        <Surface>
          <strong>{t('system.backup.importTitle')}</strong>
          <p className="field-hint">{t('system.backup.importCopy')}</p>
          <Button variant="outline" size="sm" disabled><Upload />{t('system.backup.restore')}</Button>
        </Surface>
      </div>
    </div>
  )
}

function SupportAccess() {
  const { t } = useTranslation()
  const simulation = useSimulation()
  return (
    <Surface>
      <PlannedNotice>{t('system.support.planned')}</PlannedNotice>
      <div className="ui-selector">
        <div><strong>{t('system.recovery.supportAccess')}</strong><small>{t('system.recovery.supportCopy')}</small></div>
        <Switch aria-label={t('system.recovery.supportAccess')} checked={simulation.supportAccess} onCheckedChange={simulation.setSupportAccess} />
      </div>
      <p className="field-hint">{t(simulation.supportAccess ? 'system.support.on' : 'system.support.off')}</p>
    </Surface>
  )
}

function HostnamePanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const hostname = useQuery({ queryKey: ['settings', 'hostname'], queryFn: () => api<string>('/api/v1/settings/hostname') })
  const [draft, setDraft] = useState<string>()
  const update = useMutation({
    mutationFn: (value: string) => api<TaskAccepted>('/api/v1/settings/hostname', json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'hostname'] }),
  })
  const value = draft ?? hostname.data ?? ''
  return (
    <Card>
      <form className="grid gap-4" onSubmit={(event: FormEvent) => { event.preventDefault(); update.mutate(value) }}>
        <Field label={t('system.identity.hostname')}><Input value={value} onChange={(event) => setDraft(event.target.value)} required /></Field>
        <Button type="submit" disabled={update.isPending || !draft}>{t('system.identity.save')}</Button>
        <TaskProgress taskId={update.data?.taskId} />
        {update.error ? <p className="callout error" role="alert">{errorMessage(update.error, t('common.requestFailed'))}</p> : null}
      </form>
    </Card>
  )
}

export function UiPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const status = useQuery({ queryKey: ['ui-status'], queryFn: () => api<UiStatus>('/api/v1/ui') })
  const activate = useMutation({
    mutationFn: (generation: number) => api<UiStatus>('/api/v1/ui/active', json('PUT', { generation })),
    onSuccess: (value) => queryClient.setQueryData(['ui-status'], value),
  })
  const deactivate = useMutation({
    mutationFn: () => api<UiStatus>('/api/v1/ui/active', { method: 'DELETE' }),
    onSuccess: (value) => queryClient.setQueryData(['ui-status'], value),
  })
  const active = status.data?.mode === 'custom'
  const candidate = status.data?.availableCustom
  const custom = status.data?.custom ?? candidate
  const mutationPending = activate.isPending || deactivate.isPending
  const mutationError = activate.error ?? deactivate.error
  const selectorDisabled = status.isPending || status.isError || mutationPending || (!active && !candidate?.usable)
  const select = (checked: boolean) => {
    if (checked && candidate) activate.mutate(candidate.generation)
    else deactivate.mutate()
  }
  return (
    <Card>
      <div className="service-state"><Status ok={!status.isPending && !status.isError}>{status.isPending ? t('system.ui.checking') : active ? t('system.ui.customActive') : t('system.ui.builtInActive')}</Status></div>
      <div className="ui-selector">
        <div><strong>{t('system.ui.useCustom')}</strong><small>{t('system.ui.recoveryCopy')}</small></div>
        <Switch
          aria-label={t('system.ui.useCustom')}
          checked={active}
          disabled={selectorDisabled}
          onCheckedChange={select}
        />
      </div>
      {!status.isPending && !candidate ? <p className="callout warning" role="status">{t('system.ui.noCustom')}</p> : null}
      {candidate && !candidate.usable ? <p className="callout warning" role="status">{t('system.ui.cannotSelect', { reason: unavailableMessage(candidate.unavailableReason, t) })}</p> : null}
      {custom ? <dl className="details"><div><dt>{t('system.ui.bundle')}</dt><dd>{custom.name ?? t('system.ui.generation', { generation: custom.generation })} {custom.version}</dd></div><div><dt>{t('system.ui.index')}</dt><dd>{custom.indexReadable ? t('system.ui.readable') : t('system.ui.unreadable')}</dd></div><div><dt>{t('system.ui.digest')}</dt><dd>{digestMessage(custom.digestMatches, t)}</dd></div></dl> : null}
      {active ? <a className="text-link" href="/">{t('system.ui.openCustom')} <ExternalLink className="size-4" /></a> : null}
      <a className="text-link" href="/_ui/system/ui">{t('system.ui.manage')} <Settings2 className="size-4" /></a>
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
      {mutationError ? <p className="callout error" role="alert">{errorMessage(mutationError, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function digestMessage(matches: boolean | undefined, t: ReturnType<typeof useTranslation>['t']) {
  if (matches === true) return t('system.ui.digestVerified')
  if (matches === false) return t('system.ui.digestChanged')
  return t('system.ui.digestUnknown')
}

function unavailableMessage(
  reason: NonNullable<UiStatus['availableCustom']>['unavailableReason'],
  t: ReturnType<typeof useTranslation>['t'],
) {
  switch (reason) {
    case 'missingActivationRecord': return t('system.ui.unavailable.missingActivationRecord')
    case 'unsafeTree': return t('system.ui.unavailable.unsafeTree')
    case 'indexUnavailable': return t('system.ui.unavailable.indexUnavailable')
    case 'manifestInvalid': return t('system.ui.unavailable.manifestInvalid')
    case 'digestMismatch': return t('system.ui.unavailable.digestMismatch')
    case 'incompatible': return t('system.ui.unavailable.incompatible')
    default: return t('system.ui.unavailable.unknown')
  }
}

export function PowerPanel() {
  const { t } = useTranslation()
  const action = useMutation({ mutationFn: (name: 'reboot' | 'poweroff') => api<void>(`/api/v1/actions/${name}`, { method: 'POST' }) })
  return (
    <Card>
      <div className="flex flex-wrap gap-3"><PowerAction name="reboot" pending={action.isPending} onConfirm={() => action.mutate('reboot')} /><PowerAction name="poweroff" pending={action.isPending} onConfirm={() => action.mutate('poweroff')} /></div>
      {action.isSuccess ? <p className="callout success" role="status">{t('system.power.accepted')}</p> : null}
      {action.error ? <p className="callout error" role="alert">{errorMessage(action.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function PowerAction({ name, pending, onConfirm }: { name: 'reboot' | 'poweroff'; pending: boolean; onConfirm: () => void }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const label = name === 'reboot' ? t('system.power.reboot') : t('system.power.powerOff')
  const description = name === 'reboot' ? t('system.power.confirmReboot') : t('system.power.confirmPowerOff')
  return <AlertDialog open={open} onOpenChange={setOpen}><AlertDialogTrigger render={<Button variant={name === 'reboot' ? 'secondary' : 'destructive'} disabled={pending} />}>{name === 'reboot' ? <RefreshCcw className="size-4" /> : <Power className="size-4" />}{label}</AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{label}</AlertDialogTitle><AlertDialogDescription>{description}</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" disabled={pending} onClick={() => { setOpen(false); onConfirm() }}>{label}</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog>
}
