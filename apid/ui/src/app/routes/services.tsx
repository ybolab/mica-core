import { createFileRoute } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Boxes, RadioTower } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import { TaskProgress } from '@/components/task-progress'
import type { TaskAccepted } from '@/lib/types'

function ServicesPage() {
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">Workloads</p><h1>Services</h1><p>Enable appliance services through API-backed controls.</p></div></header>
      <div className="split-grid">
        <ServiceToggle title="Containers" icon={Boxes} settingsPath="container.enabled" statePath="container" warning="Containers run as root on this appliance. A Quadlet file can start code with root capabilities." />
        <ServiceToggle title="MQTT" icon={RadioTower} settingsPath="mqtt.enabled" statePath="mqtt" warning="Listener address, authentication and port are configured separately. Review them before exposing MQTT off loopback." />
      </div>
    </div>
  )
}

function ServiceToggle({ title, icon: Icon, settingsPath, statePath, warning }: { title: string; icon: typeof Boxes; settingsPath: string; statePath: string; warning: string }) {
  const queryClient = useQueryClient()
  const enabled = useQuery({ queryKey: ['settings', settingsPath], queryFn: () => api<boolean>(`/api/v1/settings/${settingsPath}`) })
  const state = useQuery({ queryKey: ['state', statePath], queryFn: () => api<Record<string, unknown>>(`/api/v1/state/${statePath}`), retry: false })
  const update = useMutation({
    mutationFn: (value: boolean) => api<TaskAccepted>(`/api/v1/settings/${settingsPath}`, json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', settingsPath] }),
  })
  return (
    <Card>
      <CardHeader title={title} description={warning} action={<span className="service-icon"><Icon className="size-5" /></span>} />
      <div className="service-state"><Status ok={enabled.data === true}>{enabled.data ? 'enabled' : 'disabled'}</Status><span>{state.data ? 'Live state available' : 'No live state'}</span></div>
      {update.error ? <p className="callout error" role="alert">{errorMessage(update.error)}</p> : null}
      <Button variant={enabled.data ? 'secondary' : 'primary'} onClick={() => update.mutate(!enabled.data)} disabled={enabled.isPending || update.isPending}>
        {update.isPending ? 'Saving…' : enabled.data ? `Disable ${title}` : `Enable ${title}`}
      </Button>
      <TaskProgress taskId={update.data?.taskId} />
      {state.data ? <pre className="json-view compact">{JSON.stringify(state.data, null, 2)}</pre> : null}
    </Card>
  )
}

export const Route = createFileRoute('/services')({ component: ServicesPage })
