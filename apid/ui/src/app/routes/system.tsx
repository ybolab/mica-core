import { createFileRoute } from '@tanstack/react-router'
import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ExternalLink, MonitorCog, Power, RefreshCcw } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import type { TaskAccepted, UiStatus } from '@/lib/types'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field, Input } from '@/components/ui/field'
import { Status } from '@/components/ui/status'
import { Switch } from '@/components/ui/switch'
import { TaskProgress } from '@/components/task-progress'

function SystemPage() {
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">Appliance</p><h1>System</h1><p>Identity, UI selection and explicit power actions.</p></div></header>
      <div className="split-grid"><HostnamePanel /><UiPanel /></div>
      <PowerPanel />
    </div>
  )
}

function HostnamePanel() {
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
      <CardHeader title="Device identity" description="Hostname used by local services and discovery." />
      <form className="grid gap-4" onSubmit={(event: FormEvent) => { event.preventDefault(); update.mutate(value) }}>
        <Field label="Hostname"><Input value={value} onChange={(event) => setDraft(event.target.value)} required /></Field>
        <Button type="submit" disabled={update.isPending || !draft}>Save hostname</Button>
        <TaskProgress taskId={update.data?.taskId} />
        {update.error ? <p className="callout error" role="alert">{errorMessage(update.error)}</p> : null}
      </form>
    </Card>
  )
}

export function UiPanel() {
  const queryClient = useQueryClient()
  const status = useQuery({ queryKey: ['ui-status'], queryFn: () => api<UiStatus>('/api/v1/ui') })
  const activate = useMutation({
    mutationFn: () => api<UiStatus>('/api/v1/ui/active', { method: 'PUT' }),
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
    if (checked) activate.mutate()
    else deactivate.mutate()
  }
  return (
    <Card>
      <CardHeader title="User interface" description="The built-in SPA always remains available at /ui." action={<MonitorCog className="size-5 text-muted-foreground" />} />
      <div className="service-state"><Status ok={!status.isError}>{status.isPending ? 'checking UI selection' : active ? 'custom UI active at /' : 'built-in UI active at /'}</Status></div>
      <div className="ui-selector">
        <div><strong>Use custom UI at root</strong><small>The recovery console at <code>/ui</code> does not change.</small></div>
        <Switch
          aria-label="Use custom UI at root"
          checked={active}
          disabled={selectorDisabled}
          onCheckedChange={select}
        />
      </div>
      {!status.isPending && !candidate ? <p className="callout warning" role="status">No retained custom UI is installed.</p> : null}
      {candidate && !candidate.usable ? <p className="callout warning" role="status">The retained custom UI cannot be selected: {unavailableMessage(candidate.unavailableReason)}.</p> : null}
      {custom ? <dl className="details"><div><dt>Bundle</dt><dd>{custom.name ?? `generation ${custom.generation}`} {custom.version}</dd></div><div><dt>Index</dt><dd>{custom.indexReadable ? 'readable' : 'unreadable'}</dd></div><div><dt>Digest</dt><dd>{digestMessage(custom.digestMatches)}</dd></div></dl> : null}
      {active ? <a className="text-link" href="/">Open custom UI at root <ExternalLink className="size-4" /></a> : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error)}</p> : null}
      {mutationError ? <p className="callout error" role="alert">{errorMessage(mutationError)}</p> : null}
    </Card>
  )
}

function digestMessage(matches: boolean | undefined) {
  if (matches === true) return 'verified'
  if (matches === false) return 'changed'
  return 'not verified'
}

function unavailableMessage(
  reason: NonNullable<UiStatus['availableCustom']>['unavailableReason'],
) {
  switch (reason) {
    case 'missingActivationRecord': return 'its activation record is missing'
    case 'unsafeTree': return 'its files no longer form a safe bundle tree'
    case 'indexUnavailable': return 'index.html is missing or unreadable'
    case 'manifestInvalid': return 'its manifest is invalid'
    case 'digestMismatch': return 'its files changed after activation'
    case 'incompatible': return 'it does not support this API version'
    default: return 'it did not pass validation'
  }
}

function PowerPanel() {
  const action = useMutation({ mutationFn: (name: 'reboot' | 'poweroff') => api<void>(`/api/v1/actions/${name}`, { method: 'POST' }) })
  const run = (name: 'reboot' | 'poweroff') => {
    const message = name === 'reboot' ? 'Reboot this appliance now?' : 'Power off this appliance now?'
    if (window.confirm(message)) action.mutate(name)
  }
  return (
    <Card>
      <CardHeader title="Power" description="These actions are dispatched immediately. The connection may close before the device changes state." action={<Power className="size-5 text-muted-foreground" />} />
      <div className="flex flex-wrap gap-3"><Button variant="secondary" onClick={() => run('reboot')} disabled={action.isPending}><RefreshCcw className="size-4" /> Reboot</Button><Button variant="danger" onClick={() => run('poweroff')} disabled={action.isPending}><Power className="size-4" /> Power off</Button></div>
      {action.isSuccess ? <p className="callout success" role="status">Power action accepted.</p> : null}
      {action.error ? <p className="callout error" role="alert">{errorMessage(action.error)}</p> : null}
    </Card>
  )
}

export const Route = createFileRoute('/system')({ component: SystemPage })
