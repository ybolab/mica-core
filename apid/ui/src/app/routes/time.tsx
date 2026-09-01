import { createFileRoute } from '@tanstack/react-router'
import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Clock3, Globe2, Satellite } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import type { TaskAccepted, TimeStatus } from '@/lib/types'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field, Input } from '@/components/ui/field'
import { Status } from '@/components/ui/status'
import { TaskProgress } from '@/components/task-progress'

function TimePage() {
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">Appliance</p><h1>Time</h1><p>NTP servers, presentation timezone and synchronization status. Device time itself always stays UTC.</p></div></header>
      <div className="split-grid"><NtpServersPanel /><TimezonePanel /></div>
      <SyncStatusPanel />
    </div>
  )
}

export function NtpServersPanel() {
  const queryClient = useQueryClient()
  const servers = useQuery({ queryKey: ['settings', 'time.ntp.servers'], queryFn: () => api<string[]>('/api/v1/settings/time.ntp.servers') })
  const [draft, setDraft] = useState<string>()
  const update = useMutation({
    mutationFn: (value: string[]) => api<TaskAccepted>('/api/v1/settings/time.ntp.servers', json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'time.ntp.servers'] }),
  })
  const value = draft ?? (servers.data ?? []).join('\n')
  const parsed = value.split('\n').map((line) => line.trim()).filter(Boolean)
  return (
    <Card>
      <CardHeader title="NTP servers" description="One server name or address per line. An empty list keeps the image's fallback pool." action={<Satellite className="size-5 text-muted-foreground" />} />
      <form className="grid gap-4" onSubmit={(event: FormEvent) => { event.preventDefault(); update.mutate(parsed) }}>
        <Field label="Servers" hint="Polling, retry and save intervals are fixed device policy and are not configurable.">
          <textarea
            aria-label="NTP servers"
            className="min-h-28 w-full rounded-lg border border-input bg-background px-3 py-2 font-mono text-sm outline-none transition placeholder:text-muted-foreground/70 focus:border-accent focus:ring-3 focus:ring-accent/15"
            value={value}
            onChange={(event) => setDraft(event.target.value)}
            placeholder={'0.pool.ntp.org\ntime.example.com'}
          />
        </Field>
        <Button type="submit" disabled={update.isPending || draft === undefined}>Save servers</Button>
        <TaskProgress taskId={update.data?.taskId} />
        {update.error ? <p className="callout error" role="alert">{errorMessage(update.error)}</p> : null}
      </form>
    </Card>
  )
}

export function TimezonePanel() {
  const queryClient = useQueryClient()
  const timezone = useQuery({ queryKey: ['settings', 'time.timezone'], queryFn: () => api<string>('/api/v1/settings/time.timezone') })
  const [draft, setDraft] = useState<string>()
  const update = useMutation({
    mutationFn: (value: string) => api<TaskAccepted>('/api/v1/settings/time.timezone', json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'time.timezone'] }),
  })
  const value = draft ?? timezone.data ?? ''
  return (
    <Card>
      <CardHeader title="Timezone" description="IANA zone used for display and local schedules only. Clocks, logs and the API stay UTC." action={<Globe2 className="size-5 text-muted-foreground" />} />
      <form className="grid gap-4" onSubmit={(event: FormEvent) => { event.preventDefault(); update.mutate(value.trim()) }}>
        <Field label="Timezone" hint='An IANA name such as "UTC" or "Europe/Berlin".'><Input value={value} onChange={(event) => setDraft(event.target.value)} required /></Field>
        <Button type="submit" disabled={update.isPending || !draft}>Save timezone</Button>
        <TaskProgress taskId={update.data?.taskId} />
        {update.error ? <p className="callout error" role="alert">{errorMessage(update.error)}</p> : null}
      </form>
    </Card>
  )
}

const STATUS_COPY: Record<TimeStatus['status'], string> = {
  'synchronized': 'synchronized with network time',
  'synchronizing': 'synchronizing — a server is selected, the clock is not disciplined yet',
  'offline-degraded': 'degraded — no reachable time server; the device keeps retrying every 30 seconds',
  'invalid-source': 'invalid source — a server answered but its replies cannot be used',
  'unknown': 'status unavailable — the time service is not observable',
}

export function SyncStatusPanel() {
  const status = useQuery({
    queryKey: ['time-status'],
    queryFn: () => api<TimeStatus>('/api/v1/time/status'),
    refetchInterval: 15_000,
    retry: false,
  })
  const value = status.data
  return (
    <Card>
      <CardHeader title="Synchronization" description="Observed from the always-running time service. Synchronization cannot be paused; failures keep retrying." action={<Clock3 className="size-5 text-muted-foreground" />} />
      <div className="service-state">
        <Status ok={value?.status === 'synchronized'}>
          {status.isPending ? 'checking synchronization' : STATUS_COPY[value?.status ?? 'unknown']}
        </Status>
      </div>
      {value ? (
        <dl className="details">
          {value.server ? <div><dt>Server</dt><dd>{value.server.name ?? value.server.address ?? '—'}{value.server.address && value.server.name ? ` (${value.server.address})` : ''}</dd></div> : null}
          {value.sample ? <div><dt>Last sample</dt><dd>stratum {value.sample.stratum}, offset {formatOffset(value.sample.offsetSeconds)}</dd></div> : null}
          {value.sample ? <div><dt>Correction</dt><dd>{value.sample.correction === 'step' ? 'clock stepped (large correction)' : 'slewing (ordinary drift)'}</dd></div> : null}
        </dl>
      ) : null}
      {value?.detail ? <p className="callout warning" role="status">{value.detail}</p> : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error)}</p> : null}
    </Card>
  )
}

function formatOffset(seconds: number) {
  const abs = Math.abs(seconds)
  if (abs < 1) return `${(seconds * 1000).toFixed(1)} ms`
  return `${seconds.toFixed(3)} s`
}

export const Route = createFileRoute('/time')({ component: TimePage })
