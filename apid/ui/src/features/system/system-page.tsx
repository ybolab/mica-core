import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useRouterState } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Database, ExternalLink, MonitorCog, PackageSearch, Power, RefreshCcw, Settings2, Stethoscope } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import type { TaskAccepted, UiStatus } from '@/lib/types'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
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
  const initialTab = ['general', 'information', 'time', 'update', 'storage', 'diagnostics', 'recovery'].includes(hash) ? hash : 'general'
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('system.eyebrow')}</p><h1>{t('system.title')}</h1><p>{t('system.description')}</p></div></header>
      <Tabs key={initialTab} defaultValue={initialTab}>
        <TabsList aria-label={t('system.title')}>
          <TabsTrigger value="general">{t('system.tabs.general')}</TabsTrigger>
          <TabsTrigger value="information">{t('system.tabs.information')}</TabsTrigger>
          <TabsTrigger value="time">{t('system.tabs.time')}</TabsTrigger>
          <TabsTrigger value="update">{t('system.tabs.update')}</TabsTrigger>
          <TabsTrigger value="storage">{t('system.tabs.storage')}</TabsTrigger>
          <TabsTrigger value="diagnostics">{t('system.tabs.diagnostics')}</TabsTrigger>
          <TabsTrigger value="recovery">{t('system.tabs.recovery')}</TabsTrigger>
        </TabsList>
        <TabsContent value="general" className="tab-panel"><div className="split-grid"><HostnamePanel /><UiPanel /></div><PowerPanel /></TabsContent>
        <TabsContent value="information" className="tab-panel"><InformationPanel /></TabsContent>
        <TabsContent value="time" className="tab-panel"><TimePanel /></TabsContent>
        <TabsContent value="update" className="tab-panel"><UpdatePanel /><RollbackPanel /><UpdateActions /></TabsContent>
        <TabsContent value="storage" className="tab-panel"><StoragePanel /></TabsContent>
        <TabsContent value="diagnostics" className="tab-panel"><DiagnosticsPanel /></TabsContent>
        <TabsContent value="recovery" className="tab-panel"><RecoveryPanel /></TabsContent>
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
    available?: { name?: string; version?: string; channel?: string }
    bundle?: string
    last_check?: string
    client?: { available?: boolean; reason?: string }
    policy_error?: string
    reboot_gate?: { safe?: boolean; reasons?: string[] }
  }
  booted_slot?: string | null
  pending_not_confirmed?: boolean
}

export function UpdatePanel() {
  const { t } = useTranslation()
  const status = useQuery({ queryKey: ['update-state'], queryFn: () => api<UpdateStateDoc>('/api/v1/update') })
  const lifecycle = status.data?.lifecycle
  const gate = lifecycle?.reboot_gate
  const available = lifecycle?.available
  return (
    <Card>
      <CardHeader title={t('system.update.title')} description={t('system.update.description')} action={<PackageSearch className="size-5 text-muted-foreground" />} />
      <div className="service-state"><Status ok={!status.isPending && !status.isError && lifecycle?.state !== 'failed' && lifecycle?.state !== 'update-unavailable'}>{status.isPending ? t('system.update.checking') : (lifecycle?.state ?? t('common.states.unknown'))}</Status></div>
      {lifecycle?.reason ? <p className="text-sm text-muted-foreground">{lifecycle.reason}</p> : null}
      <dl className="details">
        {available ? <div><dt>{t('system.update.available')}</dt><dd>{available.name} {available.version} ({available.channel})</dd></div> : null}
        {lifecycle?.bundle ? <div><dt>{t('system.update.bundle')}</dt><dd>{lifecycle.bundle}</dd></div> : null}
        {status.data?.booted_slot ? <div><dt>{t('system.update.bootedSlot')}</dt><dd>{status.data.booted_slot}</dd></div> : null}
        {lifecycle?.last_check ? <div><dt>{t('system.update.lastCheck')}</dt><dd>{lifecycle.last_check}</dd></div> : null}
      </dl>
      {status.data?.pending_not_confirmed ? <p className="callout warning" role="status">{t('system.update.pendingReboot')}</p> : null}
      {gate ? (gate.safe
        ? <p className="callout success" role="status">{t('system.update.gateSafe')}</p>
        : <p className="callout warning" role="status">{t('system.update.gateBlocked', { reasons: (gate.reasons ?? []).join('; ') })}</p>) : null}
      {lifecycle?.client && lifecycle.client.available === false ? <p className="callout warning" role="status">{t('system.update.clientUnavailable', { reason: lifecycle.client.reason ?? '' })}</p> : null}
      {lifecycle?.policy_error ? <p className="callout error" role="alert">{t('system.update.policyError', { reason: lifecycle.policy_error })}</p> : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function UpdateActions() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const simulation = useSimulation()
  const action = useMutation({ mutationFn: (name: 'check' | 'fetch' | 'install') => api<unknown>(`/api/v1/update/${name}`, { method: 'POST' }), onSuccess: () => queryClient.invalidateQueries({ queryKey: ['update-state'] }) })
  return <Card><CardHeader title={t('system.update.actionsTitle')} description={t('system.update.actionsDescription')} /><div className="ui-selector"><div><strong>{t('system.update.automatic')}</strong><small>{t('system.update.automaticCopy')}</small></div><Switch aria-label={t('system.update.automatic')} checked={simulation.automaticUpdates} onCheckedChange={simulation.setAutomaticUpdates} /></div><div className="flex flex-wrap gap-3"><Button variant="secondary" onClick={() => action.mutate('check')} disabled={action.isPending}>{t('system.update.checkNow')}</Button><Button variant="secondary" onClick={() => action.mutate('fetch')} disabled={action.isPending}>{t('system.update.download')}</Button><Button onClick={() => action.mutate('install')} disabled={action.isPending}>{t('system.update.install')}</Button></div>{action.error ? <p className="callout error">{errorMessage(action.error, t('common.requestFailed'))}</p> : null}</Card>
}

/// The reset tiers and credential recovery bind REAL routes and are their own
/// area; the two cards beside them are still simulated and stay behind the
/// page's simulation notice.
function RecoveryPanel() {
  const { t } = useTranslation()
  const simulation = useSimulation()
  const [message, setMessage] = useState('')
  return <div className="stack"><ResetPanel /><CredentialRecoveryPanel /><Card><CardHeader title={t('system.recovery.backupTitle')} description={t('system.recovery.backupDescription')} action={<Database className="size-5 text-muted-foreground" />} /><div className="flex flex-wrap gap-3"><Button variant="secondary" onClick={() => setMessage(t('system.recovery.created'))}>{t('system.recovery.create')}</Button><Button variant="secondary" onClick={() => setMessage(t('system.recovery.restored'))}>{t('system.recovery.restore')}</Button></div>{message ? <p className="callout success">{message}</p> : null}</Card><Card><CardHeader title={t('system.recovery.supportTitle')} description={t('system.recovery.supportDescription')} action={<Stethoscope className="size-5 text-muted-foreground" />} /><div className="ui-selector"><div><strong>{t('system.recovery.supportAccess')}</strong><small>{t('system.recovery.supportCopy')}</small></div><Switch aria-label={t('system.recovery.supportAccess')} checked={simulation.supportAccess} onCheckedChange={simulation.setSupportAccess} /></div><Status ok={!simulation.supportAccess}>{t(simulation.supportAccess ? 'system.recovery.supportExpires' : 'system.recovery.supportDisabled')}</Status></Card></div>
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
      <CardHeader title={t('system.identity.title')} description={t('system.identity.description')} />
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
      <CardHeader title={t('system.ui.title')} description={t('system.ui.description')} action={<MonitorCog className="size-5 text-muted-foreground" />} />
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

function PowerPanel() {
  const { t } = useTranslation()
  const action = useMutation({ mutationFn: (name: 'reboot' | 'poweroff') => api<void>(`/api/v1/actions/${name}`, { method: 'POST' }) })
  return (
    <Card>
      <CardHeader title={t('system.power.title')} description={t('system.power.description')} action={<Power className="size-5 text-muted-foreground" />} />
      <div className="flex flex-wrap gap-3"><PowerAction name="reboot" pending={action.isPending} onConfirm={() => action.mutate('reboot')} /><PowerAction name="poweroff" pending={action.isPending} onConfirm={() => action.mutate('poweroff')} /></div>
      {action.isSuccess ? <p className="callout success" role="status">{t('system.power.accepted')}</p> : null}
      {action.error ? <p className="callout error" role="alert">{errorMessage(action.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function PowerAction({ name, pending, onConfirm }: { name: 'reboot' | 'poweroff'; pending: boolean; onConfirm: () => void }) {
  const { t } = useTranslation()
  const label = name === 'reboot' ? t('system.power.reboot') : t('system.power.powerOff')
  const description = name === 'reboot' ? t('system.power.confirmReboot') : t('system.power.confirmPowerOff')
  return <AlertDialog><AlertDialogTrigger render={<Button variant={name === 'reboot' ? 'secondary' : 'destructive'} disabled={pending} />}>{name === 'reboot' ? <RefreshCcw className="size-4" /> : <Power className="size-4" />}{label}</AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{label}</AlertDialogTitle><AlertDialogDescription>{description}</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={onConfirm}>{label}</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog>
}
