import { Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Activity, ArrowRight, Cable, Clock3, Server } from 'lucide-react'
import { api } from '@/shared/lib/http'
import type { Health, Meta, NetworkOverview, TaskRecord } from '@/lib/types'
import { Page, PageHeader, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { formatKnownState } from '@/i18n/format'

export function OverviewPage() {
  const { t } = useTranslation()
  const health = useQuery({ queryKey: ['health'], queryFn: () => api<Health>('/api/v1/health'), refetchInterval: 15_000 })
  const meta = useQuery({ queryKey: ['meta'], queryFn: () => api<Meta>('/api/v1/meta') })
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const tasks = useQuery({ queryKey: ['tasks'], queryFn: () => api<TaskRecord[]>('/api/v1/tasks'), refetchInterval: 5_000 })
  const healthy = health.data?.mosd === 'ok' && health.data?.apid === 'ok'
  const healthLabel = healthy ? t('overview.allResponding') : health.isPending ? t('overview.checking') : t('overview.unavailable')
  const queryError = health.error ?? meta.error ?? network.error ?? tasks.error

  return (
    <Page>
      <PageHeader
        title={t('overview.title')}
        description={t('overview.description')}
        action={<StatusBadge tone={healthy ? 'success' : health.isPending ? 'warning' : 'danger'}>{healthLabel}</StatusBadge>}
      />
      {queryError ? <p className="callout error" role="alert">{t('common.requestFailed')}</p> : null}
      <Surface>
        <div className="surface-title"><div><h2>{t('overview.attention.title')}</h2><p>{t('overview.attention.description')}</p></div></div>
        <div className="attention-list">
          <div className="attention-row"><div><strong>{t('overview.attention.updateTitle')}</strong><small>{t('overview.attention.updateCopy')}</small></div><Link to="/system" hash="update" className="text-link">{t('overview.attention.review')}<ArrowRight /></Link></div>
          <div className="attention-row"><div><strong>{t('overview.attention.timeTitle')}</strong><small>{t('overview.attention.timeCopy')}</small></div><Link to="/system" hash="time" className="text-link">{t('overview.attention.configure')}<ArrowRight /></Link></div>
        </div>
      </Surface>
      <div className="metric-grid">
        <Surface className="metric"><Activity /><span>{t('overview.systemHealth')}</span><strong>{healthy ? t('common.states.healthy') : t('common.states.unknown')}</strong><small>{t('overview.systemHealthCopy')}</small></Surface>
        <Surface className="metric"><Server /><span>{t('overview.managementApi')}</span><strong>{health.data?.apid ?? t('common.notAvailable')}</strong><small>{meta.data ? t('overview.metadata', { api: meta.data.api, schema: meta.data.settingsSchemaVersion }) : t('overview.loadingMetadata')}</small></Surface>
        <Surface className="metric"><Cable /><span>{t('overview.observedInterfaces')}</span><strong>{network.data?.observed.interfaceCount ?? t('common.notAvailable')}</strong><small>{network.data?.observed.available ? t('overview.configuredCount', { count: network.data.configuredCount }) : t('overview.liveUnavailable')}</small></Surface>
        <Surface className="metric"><Clock3 /><span>{t('overview.deviceUptime')}</span><strong>{formatUptime(health.data?.checkedAt, t)}</strong><small>{t('overview.readFromMosd')}</small></Surface>
      </div>
      <Surface className="surface-compact">
        <div className="surface-title table-title"><div><h2>{t('overview.recentTasks')}</h2><p>{t('overview.recentTasksDescription')}</p></div></div>
        <div className="data-table-wrap">
          <table className="data-table">
            <thead><tr><th>{t('overview.tasks.change')}</th><th>{t('overview.tasks.source')}</th><th>{t('overview.tasks.status')}</th></tr></thead>
            <tbody>{tasks.data?.slice(-6).reverse().map((task) => (
              <tr key={task.id}>
                <td><div className="cell-primary"><strong>{task.dotPath}</strong><small>{task.operation}</small></div></td>
                <td>{task.source}</td>
                <td><StatusBadge tone={task.status === 'finished' && task.outcome === 'succeeded' ? 'success' : task.outcome === 'failed' ? 'danger' : 'warning'}>{formatKnownState(task.status === 'finished' ? task.outcome ?? 'finished' : task.status, t)}</StatusBadge></td>
              </tr>
            ))}</tbody>
          </table>
          {!tasks.isPending && !tasks.isError && tasks.data?.length === 0 ? <p className="empty">{t('overview.noTasks')}</p> : null}
        </div>
      </Surface>
      <SimulationNotice scope={t('overview.title')} />
    </Page>
  )
}

function formatUptime(seconds: number | undefined, t: ReturnType<typeof useTranslation>['t']) {
  if (seconds === undefined) return t('common.notAvailable')
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  return days
    ? t('overview.uptime.daysHours', { days, hours })
    : t('overview.uptime.hoursMinutes', { hours, minutes: Math.floor((seconds % 3600) / 60) })
}
