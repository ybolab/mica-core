import { Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { api } from '@/shared/lib/http'
import type { Health, NetworkOverview, SystemInformation, TaskRecord, TimeStatus } from '@/lib/types'
import { Page, PageHeader, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { formatAge, formatKnownState } from '@/i18n/format'
import { freshnessLabel } from '@/features/shell/connection'

interface UpdateLifecycle {
  lifecycle?: { state?: string; available?: { name?: string; version?: string } }
}

export function OverviewPage() {
  const { t } = useTranslation()
  const health = useQuery({ queryKey: ['health'], queryFn: () => api<Health>('/api/v1/health'), refetchInterval: 15_000 })
  const information = useQuery({ queryKey: ['system-information'], queryFn: () => api<SystemInformation>('/api/v1/system/info'), retry: false })
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const hostname = useQuery({ queryKey: ['settings', 'hostname'], queryFn: () => api<string>('/api/v1/settings/hostname') })
  const tasks = useQuery({ queryKey: ['tasks'], queryFn: () => api<TaskRecord[]>('/api/v1/tasks'), refetchInterval: 5_000 })
  const update = useQuery({ queryKey: ['update-state'], queryFn: () => api<UpdateLifecycle>('/api/v1/update'), retry: false })
  const time = useQuery({ queryKey: ['time-status'], queryFn: () => api<TimeStatus>('/api/v1/time/status'), retry: false })

  const healthy = health.data?.mosd === 'ok' && health.data?.apid === 'ok'
  const healthLabel = healthy ? t('overview.allResponding') : health.isPending ? t('overview.checking') : t('overview.unavailable')
  const queryError = health.error ?? network.error ?? tasks.error

  const available = update.data?.lifecycle?.available
  const clockAdrift = time.data !== undefined && time.data.synchronized === false
  const attention = [
    available ? {
      id: 'update',
      tone: 'neutral' as const,
      tag: t('overview.attention.updateTag'),
      title: t('overview.attention.updateTitle', { version: available.version ?? available.name ?? '' }),
      copy: t('overview.attention.updateCopy'),
      action: t('overview.attention.review'),
      hash: 'update',
    } : undefined,
    clockAdrift ? {
      id: 'time',
      tone: 'warning' as const,
      tag: t('overview.attention.timeTag'),
      title: t('overview.attention.timeTitle'),
      copy: t('overview.attention.timeCopy'),
      action: t('overview.attention.configure'),
      hash: 'time',
    } : undefined,
  ].filter((item) => item !== undefined)

  const observed = network.data?.observed.interfaces?.find((iface) => (iface.addresses ?? []).length > 0)
  const machineId = information.data?.machineId
  const uptime = information.data?.uptime

  return (
    <Page>
      <PageHeader
        title={t('overview.title')}
        action={
          <>
            <StatusBadge tone={healthy ? 'success' : health.isPending ? 'warning' : 'danger'}>{healthLabel}</StatusBadge>
            <span className="page-freshness">{freshnessLabel(health.dataUpdatedAt, t)}</span>
          </>
        }
      />
      {queryError ? <p className="callout error" role="alert">{t('common.requestFailed')}</p> : null}
      {attention.length > 0 ? (
        <section className="labelled-section">
          <p className="section-label">{t('overview.attention.title')} · {attention.length}</p>
          <Surface className="surface-compact">
            <div className="attention-list">
              {attention.map((item) => (
                <div className="attention-row" key={item.id}>
                  <StatusBadge tone={item.tone}>{item.tag}</StatusBadge>
                  <div><strong>{item.title}</strong><small>{item.copy}</small></div>
                  <Link to="/system" hash={item.hash} className="text-link">{item.action}</Link>
                </div>
              ))}
            </div>
          </Surface>
        </section>
      ) : null}
      <div className="metric-grid">
        <Link to="/services" className="metric surface">
          <span>{t('overview.systemHealth')}</span>
          <strong>{healthy ? t('common.states.healthy') : t('common.states.unknown')}</strong>
          <small>{t('overview.systemHealthCopy')}</small>
        </Link>
        <Link to="/system" hash="information" className="metric surface">
          <span>{t('overview.identity')}</span>
          <strong className="mono">{hostname.data ?? t('common.notAvailable')}</strong>
          <small>{machineId?.available && machineId.id ? t('overview.machineId', { id: machineId.id.slice(0, 8) }) : t('common.notAvailable')}</small>
        </Link>
        <Link to="/network" className="metric surface">
          <span>{t('overview.network')}</span>
          <strong className="mono">{observed ? `${observed.name} · ${observed.addresses?.[0] ?? ''}` : t('common.notAvailable')}</strong>
          <small>{observed?.operationalState ? formatKnownState(observed.operationalState, t) : t('overview.liveUnavailable')}</small>
        </Link>
        <Link to="/system" hash="information" className="metric surface">
          <span>{t('overview.deviceUptime')}</span>
          <strong className="mono">{uptime?.available && uptime.seconds !== undefined ? formatUptime(uptime.seconds, t) : t('common.notAvailable')}</strong>
          <small>{t('overview.readFromMosd')}</small>
        </Link>
      </div>
      <Surface className="surface-compact">
        <div className="surface-title"><div><h2>{t('overview.recentTasks')}</h2></div><span className="page-freshness">{freshnessLabel(tasks.dataUpdatedAt, t)}</span></div>
        <div className="data-table-wrap">
          <table className="data-table">
            <thead><tr><th>{t('overview.tasks.action')}</th><th>{t('overview.tasks.target')}</th><th>{t('overview.tasks.phase')}</th><th>{t('overview.tasks.result')}</th><th className="text-right">{t('overview.tasks.time')}</th></tr></thead>
            <tbody>{tasks.data?.slice(-6).reverse().map((task) => {
              const phase = task.status === 'finished' ? task.outcome ?? 'finished' : task.status
              return (
                <tr key={task.id}>
                  <td>{task.operation}</td>
                  <td className="mono-cell">{task.dotPath}</td>
                  <td><StatusBadge tone={task.outcome === 'succeeded' ? 'success' : task.outcome === 'failed' ? 'danger' : 'warning'}>{formatKnownState(phase, t)}</StatusBadge></td>
                  <td>{task.message ?? t('overview.tasks.noDetail', { source: task.source })}</td>
                  <td className="text-right">{formatAge(Date.now() - Date.parse(task.enqueuedAt), t)}</td>
                </tr>
              )
            })}</tbody>
          </table>
          {!tasks.isPending && !tasks.isError && tasks.data?.length === 0 ? <p className="empty">{t('overview.noTasks')}</p> : null}
        </div>
      </Surface>
    </Page>
  )
}

function formatUptime(seconds: number, t: ReturnType<typeof useTranslation>['t']) {
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  return days
    ? t('overview.uptime.daysHours', { days, hours })
    : t('overview.uptime.hoursMinutes', { hours, minutes: Math.floor((seconds % 3600) / 60) })
}
