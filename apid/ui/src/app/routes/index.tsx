import { createFileRoute, Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { ArrowRight, Cable, Clock3, Server } from 'lucide-react'
import { api } from '@/lib/api'
import type { Health, Meta, NetworkOverview, TaskRecord } from '@/lib/types'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'

function Overview() {
  const health = useQuery({ queryKey: ['health'], queryFn: () => api<Health>('/api/v1/health'), refetchInterval: 15_000 })
  const meta = useQuery({ queryKey: ['meta'], queryFn: () => api<Meta>('/api/v1/meta') })
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const tasks = useQuery({ queryKey: ['tasks'], queryFn: () => api<TaskRecord[]>('/api/v1/tasks'), refetchInterval: 5_000 })
  return (
    <div className="page">
      <header className="page-head">
        <div><p className="eyebrow">Appliance</p><h1>Overview</h1><p>A live view of the management plane and its network edge.</p></div>
        <Status ok={health.data?.mosd === 'ok'}>{health.data?.mosd === 'ok' ? 'All systems responding' : 'Checking system'}</Status>
      </header>
      <div className="metric-grid">
        <Card className="metric"><Server /><span>Management API</span><strong>{health.data?.apid ?? '—'}</strong><small>{meta.data ? `${meta.data.api} · schema ${meta.data.settingsSchemaVersion}` : 'Loading metadata'}</small></Card>
        <Card className="metric"><Cable /><span>Observed interfaces</span><strong>{network.data?.observed.interfaceCount ?? '—'}</strong><small>{network.data?.observed.available ? `${network.data.configuredCount} configured` : 'Live state unavailable'}</small></Card>
        <Card className="metric"><Clock3 /><span>Device uptime</span><strong>{formatUptime(health.data?.checkedAt)}</strong><small>Read directly from mosd</small></Card>
      </div>
      <div className="split-grid">
        <Card>
          <CardHeader title="Network edge" description="Current status reported by systemd-networkd." action={<Link to="/network" className="text-link">Details <ArrowRight className="size-4" /></Link>} />
          <div className="interface-list">
            {network.data?.observed.interfaces.slice(0, 5).map((iface, index) => (
              <div className="interface-row" key={`${iface.index ?? index}-${iface.name}`}>
                <span className="interface-icon">{iface.name?.slice(0, 2) ?? '?'}</span>
                <div><strong>{iface.name ?? `interface ${iface.index}`}</strong><small>{iface.kind ?? iface.type ?? iface.driver ?? 'physical link'}</small></div>
                <Status ok={['routable', 'carrier', 'degraded'].includes(iface.operationalState ?? '')}>{iface.operationalState ?? 'unknown'}</Status>
              </div>
            ))}
            {network.data?.observed.interfaces.length === 0 ? <p className="empty">No observed interfaces.</p> : null}
          </div>
        </Card>
        <Card>
          <CardHeader title="Separation by design" description="The console reads and writes only through the versioned API." />
          <div className="boundary-graphic" aria-label="UI communicates with the API, which communicates with mosd">
            <span>SPA</span><i>→</i><span>/api</span><i>→</i><span>mosd</span>
          </div>
          <p className="mt-5 text-sm leading-6 text-muted-foreground">No form endpoint or server-rendered management action exists outside <code>/api</code>. The built-in console itself is always recoverable at <code>/ui</code>.</p>
        </Card>
        <Card>
          <CardHeader title="Recent apply tasks" description="Asynchronous settings reconciliation reported by mosd." />
          <div className="collection-list">
            {tasks.data?.slice(-5).reverse().map((task) => (
              <div className="collection-row" key={task.id}>
                <div><strong>{task.dotPath}</strong><small>{task.operation} · {task.source}</small></div>
                <Status ok={task.status === 'finished' && task.outcome === 'succeeded'}>{task.status === 'finished' ? task.outcome ?? 'finished' : task.status}</Status>
              </div>
            ))}
            {tasks.data?.length === 0 ? <p className="empty">No apply tasks have run this boot.</p> : null}
          </div>
        </Card>
      </div>
    </div>
  )
}

function formatUptime(seconds?: number) {
  if (seconds === undefined) return '—'
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  return days ? `${days}d ${hours}h` : `${hours}h ${Math.floor((seconds % 3600) / 60)}m`
}

export const Route = createFileRoute('/')({ component: Overview })
