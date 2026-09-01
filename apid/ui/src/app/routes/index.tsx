import { createFileRoute, Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ArrowRight, Cable, Clock3, Server } from 'lucide-react'
import { api } from '@/lib/api'
import type { Health, Meta, NetworkOverview, TaskRecord } from '@/lib/types'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import { formatKnownState } from '@/i18n/format'

function Overview() {
  const { t } = useTranslation()
  const health = useQuery({ queryKey: ['health'], queryFn: () => api<Health>('/api/v1/health'), refetchInterval: 15_000 })
  const meta = useQuery({ queryKey: ['meta'], queryFn: () => api<Meta>('/api/v1/meta') })
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const tasks = useQuery({ queryKey: ['tasks'], queryFn: () => api<TaskRecord[]>('/api/v1/tasks'), refetchInterval: 5_000 })
  return (
    <div className="page">
      <header className="page-head">
        <div><p className="eyebrow">{t('overview.eyebrow')}</p><h1>{t('overview.title')}</h1><p>{t('overview.description')}</p></div>
        <Status ok={health.data?.mosd === 'ok'}>{health.data?.mosd === 'ok' ? t('overview.allResponding') : t('overview.checking')}</Status>
      </header>
      <div className="metric-grid">
        <Card className="metric"><Server /><span>{t('overview.managementApi')}</span><strong>{health.data?.apid ?? t('common.notAvailable')}</strong><small>{meta.data ? t('overview.metadata', { api: meta.data.api, schema: meta.data.settingsSchemaVersion }) : t('overview.loadingMetadata')}</small></Card>
        <Card className="metric"><Cable /><span>{t('overview.observedInterfaces')}</span><strong>{network.data?.observed.interfaceCount ?? t('common.notAvailable')}</strong><small>{network.data?.observed.available ? t('overview.configuredCount', { count: network.data.configuredCount }) : t('overview.liveUnavailable')}</small></Card>
        <Card className="metric"><Clock3 /><span>{t('overview.deviceUptime')}</span><strong>{formatUptime(health.data?.checkedAt, t)}</strong><small>{t('overview.readFromMosd')}</small></Card>
      </div>
      <div className="split-grid">
        <Card>
          <CardHeader title={t('overview.networkEdge')} description={t('overview.networkEdgeDescription')} action={<Link to="/network" className="text-link">{t('overview.details')} <ArrowRight className="size-4" /></Link>} />
          <div className="interface-list">
            {network.data?.observed.interfaces.slice(0, 5).map((iface, index) => (
              <div className="interface-row" key={`${iface.index ?? index}-${iface.name}`}>
                <span className="interface-icon">{iface.name?.slice(0, 2) ?? '?'}</span>
                <div><strong>{iface.name ?? t('overview.interfaceFallback', { index: iface.index })}</strong><small>{iface.kind ?? iface.type ?? iface.driver ?? t('overview.physicalLink')}</small></div>
                <Status ok={['routable', 'carrier', 'degraded'].includes(iface.operationalState ?? '')}>{formatKnownState(iface.operationalState ?? 'unknown', t)}</Status>
              </div>
            ))}
            {network.data?.observed.interfaces.length === 0 ? <p className="empty">{t('overview.noInterfaces')}</p> : null}
          </div>
        </Card>
        <Card>
          <CardHeader title={t('overview.separationTitle')} description={t('overview.separationDescription')} />
          <div className="boundary-graphic" aria-label={t('overview.boundaryLabel')}>
            <span>SPA</span><i>→</i><span>/api</span><i>→</i><span>mosd</span>
          </div>
          <p className="mt-5 text-sm leading-6 text-muted-foreground">{t('overview.boundaryCopy')}</p>
        </Card>
        <Card>
          <CardHeader title={t('overview.recentTasks')} description={t('overview.recentTasksDescription')} />
          <div className="collection-list">
            {tasks.data?.slice(-5).reverse().map((task) => (
              <div className="collection-row" key={task.id}>
                <div><strong>{task.dotPath}</strong><small>{task.operation} · {task.source}</small></div>
                <Status ok={task.status === 'finished' && task.outcome === 'succeeded'}>{formatKnownState(task.status === 'finished' ? task.outcome ?? 'finished' : task.status, t)}</Status>
              </div>
            ))}
            {tasks.data?.length === 0 ? <p className="empty">{t('overview.noTasks')}</p> : null}
          </div>
        </Card>
      </div>
    </div>
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

export const Route = createFileRoute('/')({ component: Overview })
