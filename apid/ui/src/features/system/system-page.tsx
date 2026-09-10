import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useNavigate, useRouterState } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Download, ExternalLink, PackageSearch, Power, RefreshCcw, Settings2, Upload } from 'lucide-react'
import { api, json } from '@/shared/lib/http'
import type { TaskAccepted, UiStatus } from '@/lib/types'
import { Button, buttonVariants } from '@/shared/components/ui/button'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { FactList } from '@/shared/components/fact-list'
import { FormField, ToggleField } from '@/shared/components/form-field'
import { Page, PageHeader, PageSection } from '@/shared/components/page'
import { Panel } from '@/shared/components/panel'
import { RowItem, RowList } from '@/shared/components/row-item'
import { StatusBadge, StatusDot } from '@/shared/components/status-badge'
import { PlannedNotice } from '@/shared/simulation/planned'
import { Input } from '@/shared/components/ui/input'
import { Switch } from '@/shared/components/ui/switch'
import { TaskProgress } from '@/shared/components/task-progress'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/shared/components/ui/tabs'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { useSimulation } from '@/shared/simulation/simulation-provider'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'
import { InformationPanel } from '@/features/system/information-panel'
import { TimePanel } from '@/features/system/time-panel'
import { StoragePanel } from '@/features/system/storage-panel'
import { DiagnosticsPanel } from '@/features/system/diagnostics-panel'
import { RollbackPanel } from '@/features/system/rollback-panel'
import { AutomaticUpdatesPanel } from '@/features/system/automatic-updates-panel'
import { CredentialRecoveryPanel } from '@/features/recovery/credential-recovery-panel'
import { ResetPanel } from '@/features/recovery/reset-panel'

// The prototype has six tabs: recovery is folded into update, general and
// diagnostics. The old anchor still resolves rather than dropping the reader
// on the first tab.
const TABS = ['general', 'information', 'time', 'update', 'storage', 'diagnostics'] as const

export function SystemPage() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const hash = useRouterState({ select: (state) => state.location.hash })
  const active = (TABS as readonly string[]).includes(hash) ? hash : hash === 'recovery' ? 'update' : 'general'
  return (
    <Page>
      <PageHeader title={t('system.title')} />
      {/* Controlled and written back to the hash, so a tab can be linked, sent
          to someone, and survive a reload. It used to be uncontrolled with a
          `key` remount, which read the hash once and then diverged from it. */}
      <Tabs value={active} onValueChange={(value) => void navigate({ to: '/system', hash: String(value), replace: true })}>
        <TabsList aria-label={t('system.title')}>
          {TABS.map((tab) => <TabsTrigger key={tab} value={tab}>{t(`system.tabs.${tab}`)}</TabsTrigger>)}
        </TabsList>
        <TabsContent value="general" className="grid gap-6 pt-4">
          <PageSection title={t('system.identity.title')} description={t('system.identity.description')}><HostnamePanel /></PageSection>
          <PageSection title={t('system.ui.title')} description={t('system.ui.description')}><UiPanel /></PageSection>
          <PageSection title={t('system.power.title')} description={t('system.power.description')}><PowerPanel /></PageSection>
          <PageSection title={t('system.recovery.reset.title')} description={t('system.recovery.reset.description')} tone="danger"><ResetPanel /></PageSection>
        </TabsContent>
        <TabsContent value="information" className="grid gap-6 pt-4"><InformationPanel /></TabsContent>
        <TabsContent value="time" className="grid gap-6 pt-4"><TimePanel /></TabsContent>
        <TabsContent value="update" className="grid gap-6 pt-4">
          <PageSection title={t('system.update.title')} description={t('system.update.description')}>
            <UpdatePanel />
            <UpdateChecks />
            <UpdateActions />
          </PageSection>
          <PageSection title={t('system.update.automaticTitle')} description={t('system.update.automaticDescription')}><AutomaticUpdatesPanel /></PageSection>
          <PageSection title={t('system.update.manualTitle')} description={t('system.update.manualDescription')}><ManualUpdate /></PageSection>
          <PageSection title={t('system.backup.title')} description={t('system.backup.description')}><ConfigBackup /></PageSection>
          <PageSection title={t('system.recovery.additionTitle')} description={t('system.recovery.addition')}>
            <RollbackPanel />
            <CredentialRecoveryPanel />
          </PageSection>
        </TabsContent>
        <TabsContent value="storage" className="grid gap-6 pt-4"><StoragePanel /></TabsContent>
        <TabsContent value="diagnostics" className="grid gap-6 pt-4">
          <DiagnosticsPanel />
          <PageSection title={t('system.support.title')} description={t('system.support.description')}><SupportAccess /></PageSection>
        </TabsContent>
      </Tabs>
      <SimulationNotice scope={t('system.simulationScope')} />
    </Page>
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

/// One query key, one `queryFn`. Two panes used to declare the same key with
/// different result types, which is a cache entry whose shape depends on which
/// pane mounted first.
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
  const healthy = !status.isPending && !status.isError && lifecycle?.state !== 'failed' && lifecycle?.state !== 'update-unavailable'
  return (
    <Panel title={t('system.update.title')} description={t('system.update.description')} action={<PackageSearch className="size-5 text-muted-foreground" />}>
      <StatusDot state={status.isPending ? 'pending' : healthy ? 'ok' : 'warning'}>
        {status.isPending ? t('system.update.checking') : (lifecycle?.state ?? t('common.states.unknown'))}
      </StatusDot>
      {lifecycle?.reason ? <p className="text-sm text-muted-foreground">{lifecycle.reason}</p> : null}
      <FactList facts={[
        available && { id: 'available', label: t('system.update.available'), value: `${available.version} · ${available.deploymentId} (${available.channel})` },
        lifecycle?.deploymentId && { id: 'staged', label: t('system.update.staged'), value: lifecycle.deploymentId, mono: true },
        boot && { id: 'running', label: t('system.update.running'), value: boot.deploymentId, mono: true },
        boot && { id: 'kernel', label: t('system.update.kernel'), value: boot.kernelId, mono: true },
        boot && { id: 'rootfs', label: t('system.update.rootfs'), value: boot.rootfsId, mono: true },
        running && { id: 'version', label: t('system.update.version'), value: `${running.version} · ${running.kernelRelease}` },
        deployment && { id: 'generation', label: t('system.update.generation'), value: deployment.highestGeneration },
        deployment?.current && deployment.current !== boot?.deploymentId
          ? { id: 'confirmed', label: t('system.update.confirmed'), value: deployment.current, mono: true } : undefined,
        deployment?.candidate ? { id: 'candidate', label: t('system.update.candidate'), value: deployment.candidate, mono: true } : undefined,
        deployment?.fallback ? { id: 'fallback', label: t('system.update.fallback'), value: deployment.fallback, mono: true } : undefined,
        deployment?.failed.length
          ? { id: 'failed', label: t('system.update.failed'), value: deployment.failed.join(', '), mono: true } : undefined,
        lifecycle?.last_check && { id: 'lastCheck', label: t('system.update.lastCheck'), value: lifecycle.last_check },
      ]} />
      {deployment?.candidate && deployment.candidate !== boot?.deploymentId ? <Callout tone="warning" title={t('system.update.pendingReboot')} /> : null}
      {lifecycle?.client && lifecycle.client.available === false ? <Callout tone="warning" title={t('system.update.clientUnavailable', { reason: lifecycle.client.reason ?? '' })} /> : null}
      {lifecycle?.policy_error ? <Callout tone="danger" title={t('system.update.policyError', { reason: lifecycle.policy_error })} /> : null}
      {status.error ? <Callout tone="danger" title={failureDetail(status.error, t('common.requestFailed'))} /> : null}
    </Panel>
  )
}

export function UpdateActions() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const status = useUpdateState()
  const action = useMutationFeedback<unknown, 'check' | 'fetch' | 'install'>({
    mutationFn: (name) => api<unknown>(`/api/v1/update/${name}`, json('POST', name === 'install' ? { deploymentId: status.data?.lifecycle?.deploymentId } : undefined)),
    success: (_data, name) => t(`system.update.accepted.${name}`),
    failure: t('system.update.actionsTitle'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['update-state'] }),
  })
  const busy = action.isPending || status.isPending || status.isError || activeUpdateStates.has(status.data?.lifecycle?.state ?? '')
  return (
    <Panel title={t('system.update.actionsTitle')} description={t('system.update.actionsDescription')}>
      <div className="flex flex-wrap gap-3">
        <Button variant="secondary" onClick={() => action.mutate('check')} disabled={busy}>{t('system.update.checkNow')}</Button>
        <Button variant="secondary" onClick={() => action.mutate('fetch')} disabled={busy}>{t('system.update.download')}</Button>
        <Button onClick={() => action.mutate('install')} disabled={busy || !status.data?.lifecycle?.deploymentId}>{t('system.update.install')}</Button>
      </div>
    </Panel>
  )
}

/// The prototype's check table. Only the rows the device actually answers are
/// rendered; the signature, compatibility and space checks it also shows have
/// no endpoint, and are named as missing rather than invented.
export function UpdateChecks() {
  const { t } = useTranslation()
  const status = useUpdateState()
  const lifecycle = status.data?.lifecycle
  const gate = lifecycle?.reboot_gate
  const rows = [
    gate ? { id: 'reboot', tone: gate.safe ? 'success' as const : 'warning' as const, tag: t(gate.safe ? 'common.states.available' : 'system.update.checks.blocked'), label: t('system.update.checks.safeToReboot'), value: gate.safe ? t('system.update.gateSafe') : (gate.reasons ?? []).join('; ') } : undefined,
    lifecycle?.client ? { id: 'client', tone: lifecycle.client.available ? 'success' as const : 'danger' as const, tag: t(lifecycle.client.available ? 'common.states.available' : 'common.states.unavailable'), label: t('system.update.checks.client'), value: lifecycle.client.reason ?? t('system.update.checks.clientOk') } : undefined,
    lifecycle?.policy_error ? { id: 'policy', tone: 'danger' as const, tag: t('common.states.failed'), label: t('system.update.checks.policy'), value: lifecycle.policy_error } : undefined,
  ].filter((row) => row !== undefined)
  if (rows.length === 0) return null
  return (
    <Panel contentClassName="gap-0 p-0">
      <RowList>
        {rows.map((row) => (
          <RowItem
            key={row.id}
            title={row.label}
            actions={<><StatusBadge tone={row.tone}>{row.tag}</StatusBadge><span className="text-sm text-muted-foreground">{row.value}</span></>}
          />
        ))}
      </RowList>
    </Panel>
  )
}

function ManualUpdate() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [deploymentId, setDeploymentId] = useState('')
  const install = useMutationFeedback({
    mutationFn: () => api<unknown>('/api/v1/update/install', json('POST', { deploymentId })),
    success: t('system.update.installAccepted'),
    failure: t('system.update.install'),
    onSuccess: () => { setDeploymentId(''); void queryClient.invalidateQueries({ queryKey: ['update-state'] }) },
  })
  return (
    <Panel>
      <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); install.mutate() }}>
        <p className="text-sm text-muted-foreground">{t('system.update.manualFormats')}</p>
        <FormField label={t('system.update.deploymentId')}>
          {(id) => <Input id={id} className="font-mono" value={deploymentId} onChange={(event) => setDeploymentId(event.target.value)} maxLength={64} pattern="[0-9a-f]{64}" required />}
        </FormField>
        <Button className="justify-self-end" type="submit" disabled={install.isPending || !/^[0-9a-f]{64}$/.test(deploymentId)}>{t('system.update.install')}</Button>
      </form>
    </Panel>
  )
}

function ConfigBackup() {
  const { t } = useTranslation()
  return (
    <div className="grid gap-3">
      <PlannedNotice>{t('system.backup.planned')}</PlannedNotice>
      <div className="grid gap-3 sm:grid-cols-2">
        <Panel title={t('system.backup.exportTitle')} description={t('system.backup.exportCopy')}>
          <Button className="justify-self-start" variant="outline" size="sm" disabled><Download />{t('system.backup.download')}</Button>
        </Panel>
        <Panel title={t('system.backup.importTitle')} description={t('system.backup.importCopy')}>
          <Button className="justify-self-start" variant="outline" size="sm" disabled><Upload />{t('system.backup.restore')}</Button>
        </Panel>
      </div>
    </div>
  )
}

function SupportAccess() {
  const { t } = useTranslation()
  const simulation = useSimulation()
  return (
    <Panel>
      <PlannedNotice>{t('system.support.planned')}</PlannedNotice>
      <ToggleField
        title={t('system.recovery.supportAccess')}
        description={t('system.recovery.supportCopy')}
        control={<Switch aria-label={t('system.recovery.supportAccess')} checked={simulation.supportAccess} onCheckedChange={simulation.setSupportAccess} />}
      />
      <p className="text-sm text-muted-foreground">{t(simulation.supportAccess ? 'system.support.on' : 'system.support.off')}</p>
    </Panel>
  )
}

function HostnamePanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const hostname = useQuery({ queryKey: ['settings', 'hostname'], queryFn: () => api<string>('/api/v1/settings/hostname') })
  const [draft, setDraft] = useState<string>()
  const update = useMutationFeedback<TaskAccepted, string>({
    mutationFn: (value) => api<TaskAccepted>('/api/v1/settings/hostname', json('PUT', value)),
    success: t('system.identity.saved'),
    failure: t('system.identity.save'),
    // The draft is dropped so the field shows what the device confirmed rather
    // than what this browser last typed.
    onSuccess: () => { setDraft(undefined); void queryClient.invalidateQueries({ queryKey: ['settings', 'hostname'] }) },
  })
  const value = draft ?? hostname.data ?? ''
  return (
    <Panel>
      <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); update.mutate(value) }}>
        <FormField label={t('system.identity.hostname')}>
          {(id) => <Input id={id} value={value} onChange={(event) => setDraft(event.target.value)} required />}
        </FormField>
        <Button className="justify-self-end" type="submit" disabled={update.isPending || draft === undefined}>{t('system.identity.save')}</Button>
        <TaskProgress taskId={update.data?.taskId} />
      </form>
    </Panel>
  )
}

export function UiPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const status = useQuery({ queryKey: ['ui-status'], queryFn: () => api<UiStatus>('/api/v1/ui') })
  const select = useMutationFeedback<UiStatus, number | undefined>({
    mutationFn: (generation) => generation === undefined
      ? api<UiStatus>('/api/v1/ui/active', { method: 'DELETE' })
      : api<UiStatus>('/api/v1/ui/active', json('PUT', { generation })),
    success: (_data, generation) => t(generation === undefined ? 'system.ui.builtInSelected' : 'system.ui.customSelected'),
    failure: t('system.ui.useCustom'),
    onSuccess: (value) => queryClient.setQueryData(['ui-status'], value),
  })
  const active = status.data?.mode === 'custom'
  const candidate = status.data?.availableCustom
  const custom = status.data?.custom ?? candidate
  const selectorDisabled = status.isPending || status.isError || select.isPending || (!active && !candidate?.usable)
  return (
    <Panel>
      <StatusDot state={status.isPending ? 'pending' : status.isError ? 'warning' : 'ok'}>
        {status.isPending ? t('system.ui.checking') : active ? t('system.ui.customActive') : t('system.ui.builtInActive')}
      </StatusDot>
      <ToggleField
        title={t('system.ui.useCustom')}
        description={t('system.ui.recoveryCopy')}
        control={(
          <Switch
            aria-label={t('system.ui.useCustom')}
            checked={active}
            disabled={selectorDisabled}
            onCheckedChange={(checked) => select.mutate(checked && candidate ? candidate.generation : undefined)}
          />
        )}
      />
      {!status.isPending && !candidate ? <Callout tone="warning" title={t('system.ui.noCustom')} /> : null}
      {candidate && !candidate.usable ? <Callout tone="warning" title={t('system.ui.cannotSelect', { reason: unavailableMessage(candidate.unavailableReason, t) })} /> : null}
      <FactList facts={custom ? [
        { id: 'bundle', label: t('system.ui.bundle'), value: `${custom.name ?? t('system.ui.generation', { generation: custom.generation })} ${custom.version ?? ''}`.trim() },
        { id: 'index', label: t('system.ui.index'), value: custom.indexReadable ? t('system.ui.readable') : t('system.ui.unreadable') },
        { id: 'digest', label: t('system.ui.digest'), value: digestMessage(custom.digestMatches, t) },
      ] : []} />
      <div className="flex flex-wrap gap-2">
        {active ? <a className={buttonVariants({ variant: 'outline', size: 'sm' })} href="/">{t('system.ui.openCustom')} <ExternalLink /></a> : null}
        <a className={buttonVariants({ variant: 'outline', size: 'sm' })} href="/_ui/system/ui">{t('system.ui.manage')} <Settings2 /></a>
      </div>
      {status.error ? <Callout tone="danger" title={failureDetail(status.error, t('common.requestFailed'))} /> : null}
    </Panel>
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
  // The device is about to stop answering, so the report is raised by the
  // dialog before the connection drops rather than by a later poll.
  const action = useMutation({ mutationFn: (name: 'reboot' | 'poweroff') => api<void>(`/api/v1/actions/${name}`, { method: 'POST' }) })
  return (
    <Panel>
      <div className="flex flex-wrap gap-3">
        <ConfirmDialog
          trigger={<Button variant="secondary" disabled={action.isPending}><RefreshCcw />{t('system.power.reboot')}</Button>}
          title={t('system.power.reboot')}
          description={t('system.power.confirmReboot')}
          confirmLabel={t('system.power.reboot')}
          success={t('system.power.accepted')}
          failure={t('system.power.reboot')}
          onConfirm={() => action.mutateAsync('reboot')}
        />
        <ConfirmDialog
          trigger={<Button variant="destructive" disabled={action.isPending}><Power />{t('system.power.powerOff')}</Button>}
          title={t('system.power.powerOff')}
          description={t('system.power.confirmPowerOff')}
          confirmLabel={t('system.power.powerOff')}
          success={t('system.power.accepted')}
          failure={t('system.power.powerOff')}
          onConfirm={() => action.mutateAsync('poweroff')}
        />
      </div>
    </Panel>
  )
}
