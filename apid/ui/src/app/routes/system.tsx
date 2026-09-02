import { createFileRoute } from '@tanstack/react-router'
import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ExternalLink, MonitorCog, PackageSearch, Power, RefreshCcw, Settings2 } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import type { TaskAccepted, UiStatus } from '@/lib/types'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field, Input } from '@/components/ui/field'
import { Status } from '@/components/ui/status'
import { Switch } from '@/components/ui/switch'
import { TaskProgress } from '@/components/task-progress'

function SystemPage() {
  const { t } = useTranslation()
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('system.eyebrow')}</p><h1>{t('system.title')}</h1><p>{t('system.description')}</p></div></header>
      <div className="split-grid"><HostnamePanel /><UiPanel /></div>
      <UpdatePanel />
      <PowerPanel />
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
      <div className="service-state"><Status ok={!status.isError && lifecycle?.state !== 'failed'}>{status.isPending ? t('system.update.checking') : (lifecycle?.state ?? t('common.states.unknown'))}</Status></div>
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
      <div className="service-state"><Status ok={!status.isError}>{status.isPending ? t('system.ui.checking') : active ? t('system.ui.customActive') : t('system.ui.builtInActive')}</Status></div>
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
  const run = (name: 'reboot' | 'poweroff') => {
    const message = name === 'reboot' ? t('system.power.confirmReboot') : t('system.power.confirmPowerOff')
    if (window.confirm(message)) action.mutate(name)
  }
  return (
    <Card>
      <CardHeader title={t('system.power.title')} description={t('system.power.description')} action={<Power className="size-5 text-muted-foreground" />} />
      <div className="flex flex-wrap gap-3"><Button variant="secondary" onClick={() => run('reboot')} disabled={action.isPending}><RefreshCcw className="size-4" /> {t('system.power.reboot')}</Button><Button variant="danger" onClick={() => run('poweroff')} disabled={action.isPending}><Power className="size-4" /> {t('system.power.powerOff')}</Button></div>
      {action.isSuccess ? <p className="callout success" role="status">{t('system.power.accepted')}</p> : null}
      {action.error ? <p className="callout error" role="alert">{errorMessage(action.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

export const Route = createFileRoute('/system')({ component: SystemPage })
