import { Link } from '@tanstack/react-router'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { api, json } from '@/shared/lib/http'
import type { TaskAccepted } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { Page, PageHeader } from '@/shared/components/page'
import { Panel } from '@/shared/components/panel'
import { StatusBadge } from '@/shared/components/status-badge'
import { Switch } from '@/shared/components/ui/switch'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { useSimulation } from '@/shared/simulation/simulation-provider'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'
import { serviceCatalog, serviceEndpoint, type ServiceDefinition } from './service-catalog'

export function ServicesPage() {
  const { t } = useTranslation()
  return (
    <Page>
      <PageHeader title={t('services.title')} />
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {serviceCatalog.map((service) => <ServiceCard key={service.id} service={service} />)}
      </div>
      <SimulationNotice scope={t('services.terminal.title')} />
    </Page>
  )
}

function ServiceCard({ service }: { service: ServiceDefinition }) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const simulation = useSimulation()
  const Icon = service.icon
  const title = t(`services.${service.id}.title`)
  const enabled = useQuery({
    queryKey: ['settings', service.settingsPath],
    queryFn: () => api<boolean>(`/api/v1/settings/${service.settingsPath}`),
    enabled: Boolean(service.settingsPath),
  })
  const state = useQuery({
    queryKey: ['state', service.statePath],
    queryFn: () => api<Record<string, unknown>>(`/api/v1/state/${service.statePath}`),
    enabled: Boolean(service.statePath),
    retry: false,
  })
  const update = useMutationFeedback<TaskAccepted, boolean>({
    mutationFn: (value) => api<TaskAccepted>(`/api/v1/settings/${service.settingsPath}`, json('PUT', value)),
    success: (_data, value) => t(value ? 'services.enabledToast' : 'services.disabledToast', { name: title }),
    failure: title,
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['settings', service.settingsPath] }),
  })

  const simulated = service.settingsPath === undefined
  const on = simulated ? simulation.terminalEnabled : enabled.data === true
  const observed = typeof state.data?.state === 'string' ? state.data.state : undefined
  const endpoint = serviceEndpoint(state.data)

  return (
    <Panel className="h-full" contentClassName="gap-3">
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-center gap-2.5">
          <span className="grid size-9 shrink-0 place-items-center rounded-lg bg-muted"><Icon className="size-5" /></span>
          <div className="flex min-w-0 flex-col">
            <Link to="/services/$service" params={{ service: service.id }} className="font-medium text-primary hover:underline">{title}</Link>
            <small className="font-mono text-sm break-all text-muted-foreground">{endpoint ?? (simulated ? t('services.terminal.endpoint') : t('common.notAvailable'))}</small>
          </div>
        </div>
        <Switch
          checked={on}
          onCheckedChange={(value) => simulated ? simulation.setTerminalEnabled(value) : update.mutate(value)}
          disabled={!simulated && (enabled.isPending || enabled.isError || update.isPending)}
          aria-label={title}
        />
      </div>
      <p className="text-sm text-muted-foreground">{t(`services.${service.id}.warning`)}</p>
      {enabled.error ? <Callout tone="danger" title={failureDetail(enabled.error, t('common.requestFailed'))} /> : null}
      <div className="mt-auto flex flex-wrap items-center justify-between gap-2">
        <StatusBadge tone={simulated ? 'neutral' : observed === 'running' ? 'success' : state.isError ? 'warning' : 'neutral'}>
          {simulated ? t(on ? 'common.states.enabled' : 'common.states.disabled') : observed ?? t('common.states.unknown')}
        </StatusBadge>
        <span className="text-sm text-muted-foreground">{update.isPending ? t('services.saving') : t(on ? 'services.desiredEnabled' : 'services.desiredDisabled')}</span>
      </div>
    </Panel>
  )
}
