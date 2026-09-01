import { createFileRoute } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Boxes, RadioTower } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import { TaskProgress } from '@/components/task-progress'
import type { TaskAccepted } from '@/lib/types'

function ServicesPage() {
  const { t } = useTranslation()
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('services.eyebrow')}</p><h1>{t('services.title')}</h1><p>{t('services.description')}</p></div></header>
      <div className="split-grid">
        <ServiceToggle title={t('services.containers.title')} icon={Boxes} settingsPath="container.enabled" statePath="container" warning={t('services.containers.warning')} />
        <ServiceToggle title={t('services.mqtt.title')} icon={RadioTower} settingsPath="mqtt.enabled" statePath="mqtt" warning={t('services.mqtt.warning')} />
      </div>
    </div>
  )
}

function ServiceToggle({ title, icon: Icon, settingsPath, statePath, warning }: { title: string; icon: typeof Boxes; settingsPath: string; statePath: string; warning: string }) {
  const { t } = useTranslation()
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
      <div className="service-state"><Status ok={enabled.data === true}>{t(enabled.data ? 'common.states.enabled' : 'common.states.disabled')}</Status><span>{state.data ? t('services.liveAvailable') : t('services.noLiveState')}</span></div>
      {update.error ? <p className="callout error" role="alert">{errorMessage(update.error, t('common.requestFailed'))}</p> : null}
      <Button variant={enabled.data ? 'secondary' : 'primary'} onClick={() => update.mutate(!enabled.data)} disabled={enabled.isPending || update.isPending}>
        {update.isPending ? t('services.saving') : enabled.data ? t('services.disable', { name: title }) : t('services.enable', { name: title })}
      </Button>
      <TaskProgress taskId={update.data?.taskId} />
      {state.data ? <pre className="json-view compact">{JSON.stringify(state.data, null, 2)}</pre> : null}
    </Card>
  )
}

export const Route = createFileRoute('/services')({ component: ServicesPage })
