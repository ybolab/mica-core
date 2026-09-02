import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Clock3, Globe2, Satellite } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import type { TaskAccepted, TimeStatus } from '@/lib/types'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field } from '@/shared/components/field'
import { Input } from '@/shared/components/ui/input'
import { Status } from '@/components/ui/status'
import { TaskProgress } from '@/shared/components/task-progress'

export function TimePanel() {
  return (
    <div className="stack">
      <div className="split-grid"><NtpServersPanel /><TimezonePanel /></div>
      <SyncStatusPanel />
    </div>
  )
}

export function NtpServersPanel() {
  const { t } = useTranslation()
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
      <CardHeader title={t('system.time.servers.title')} description={t('system.time.servers.description')} action={<Satellite className="size-5 text-muted-foreground" />} />
      <form className="grid gap-4" onSubmit={(event: FormEvent) => { event.preventDefault(); update.mutate(parsed) }}>
        <Field label={t('system.time.servers.label')} hint={t('system.time.servers.hint')}>
          <textarea
            aria-label={t('system.time.servers.label')}
            className="min-h-28 w-full rounded-lg border border-input bg-background px-3 py-2 font-mono text-sm outline-none transition placeholder:text-muted-foreground/70 focus:border-accent focus:ring-3 focus:ring-accent/15"
            value={value}
            onChange={(event) => setDraft(event.target.value)}
            placeholder={'0.pool.ntp.org\ntime.example.com'}
          />
        </Field>
        <Button type="submit" disabled={update.isPending || draft === undefined}>{t('system.time.servers.save')}</Button>
        <TaskProgress taskId={update.data?.taskId} />
        {servers.error ? <p className="callout error" role="alert">{errorMessage(servers.error, t('common.requestFailed'))}</p> : null}
        {update.error ? <p className="callout error" role="alert">{errorMessage(update.error, t('common.requestFailed'))}</p> : null}
      </form>
    </Card>
  )
}

export function TimezonePanel() {
  const { t } = useTranslation()
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
      <CardHeader title={t('system.time.timezone.title')} description={t('system.time.timezone.description')} action={<Globe2 className="size-5 text-muted-foreground" />} />
      <form className="grid gap-4" onSubmit={(event: FormEvent) => { event.preventDefault(); update.mutate(value.trim()) }}>
        <Field label={t('system.time.timezone.label')} hint={t('system.time.timezone.hint')}>
          <Input value={value} onChange={(event) => setDraft(event.target.value)} required />
        </Field>
        <Button type="submit" disabled={update.isPending || !draft}>{t('system.time.timezone.save')}</Button>
        <TaskProgress taskId={update.data?.taskId} />
        {timezone.error ? <p className="callout error" role="alert">{errorMessage(timezone.error, t('common.requestFailed'))}</p> : null}
        {update.error ? <p className="callout error" role="alert">{errorMessage(update.error, t('common.requestFailed'))}</p> : null}
      </form>
    </Card>
  )
}

export function SyncStatusPanel() {
  const { t } = useTranslation()
  const status = useQuery({
    queryKey: ['time-status'],
    queryFn: () => api<TimeStatus>('/api/v1/time/status'),
    refetchInterval: 15_000,
    retry: false,
  })
  const value = status.data
  return (
    <Card>
      <CardHeader title={t('system.time.status.title')} description={t('system.time.status.description')} action={<Clock3 className="size-5 text-muted-foreground" />} />
      <div className="service-state">
        <Status ok={value?.status === 'synchronized'}>
          {status.isPending ? t('system.time.status.checking') : t(`system.time.status.states.${value?.status ?? 'unknown'}`)}
        </Status>
      </div>
      {value ? (
        <dl className="details">
          {value.server ? <div><dt>{t('system.time.status.server')}</dt><dd>{serverSummary(value.server, t)}</dd></div> : null}
          {value.sample ? <div><dt>{t('system.time.status.sample')}</dt><dd>{t('system.time.status.sampleValue', { stratum: value.sample.stratum, offset: formatOffset(value.sample.offsetSeconds) })}</dd></div> : null}
          {value.sample ? <div><dt>{t('system.time.status.correction')}</dt><dd>{t(value.sample.correction === 'step' ? 'system.time.status.stepped' : 'system.time.status.slewing')}</dd></div> : null}
        </dl>
      ) : null}
      {value?.detail ? <p className="callout warning" role="status">{value.detail}</p> : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function serverSummary(server: NonNullable<TimeStatus['server']>, t: ReturnType<typeof useTranslation>['t']) {
  if (server.name && server.address) return `${server.name} (${server.address})`
  return server.name ?? server.address ?? t('common.notAvailable')
}

function formatOffset(seconds: number) {
  const abs = Math.abs(seconds)
  if (abs < 1) return `${(seconds * 1000).toFixed(1)} ms`
  return `${seconds.toFixed(3)} s`
}
