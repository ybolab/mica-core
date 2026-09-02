import { Link } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { api, errorMessage, json } from '@/shared/lib/http'
import type { TaskAccepted } from '@/lib/types'
import { Page, PageHeader, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { Switch } from '@/shared/components/ui/switch'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { useSimulation } from '@/shared/simulation/simulation-provider'
import { serviceCatalog, serviceEndpoint, type ServiceDefinition } from './service-catalog'

export function ServicesPage() {
  const { t } = useTranslation()
  return (
    <Page>
      <PageHeader title={t('services.title')} />
      <div className="service-grid">
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
  const update = useMutation({
    mutationFn: (value: boolean) => api<TaskAccepted>(`/api/v1/settings/${service.settingsPath}`, json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', service.settingsPath] }),
  })

  const simulated = service.settingsPath === undefined
  const on = simulated ? simulation.terminalEnabled : enabled.data === true
  const observed = typeof state.data?.state === 'string' ? state.data.state : undefined
  const endpoint = serviceEndpoint(state.data)
  const title = t(`services.${service.id}.title`)

  return (
    <Surface className="service-card">
      <header>
        <div className="service-identity">
          <span className="service-icon"><Icon /></span>
          <div>
            <Link to="/services/$service" params={{ service: service.id }} className="table-primary-link">{title}</Link>
            <small className="mono">{endpoint ?? (simulated ? t('services.terminal.endpoint') : t('common.notAvailable'))}</small>
          </div>
        </div>
        <Switch
          checked={on}
          onCheckedChange={(value) => simulated ? simulation.setTerminalEnabled(value) : update.mutate(value)}
          disabled={!simulated && (enabled.isPending || enabled.isError || update.isPending)}
          aria-label={title}
        />
      </header>
      <p>{t(`services.${service.id}.warning`)}</p>
      {enabled.error || update.error ? <p className="callout error" role="alert">{errorMessage(enabled.error ?? update.error, t('common.requestFailed'))}</p> : null}
      <footer>
        <StatusBadge tone={simulated ? 'neutral' : observed === 'running' ? 'success' : state.isError ? 'warning' : 'neutral'}>
          {simulated ? t(on ? 'common.states.enabled' : 'common.states.disabled') : observed ?? t('common.states.unknown')}
        </StatusBadge>
        <span>{update.isPending ? t('services.saving') : t(on ? 'services.desiredEnabled' : 'services.desiredDisabled')}</span>
      </footer>
    </Surface>
  )
}
