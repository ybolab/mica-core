import { useState } from 'react'
import { Link, useParams } from '@tanstack/react-router'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ChevronLeft, TerminalSquare } from 'lucide-react'
import { api, json } from '@/shared/lib/http'
import type { TaskAccepted } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { FormField } from '@/shared/components/form-field'
import { MetricCard } from '@/shared/components/metric-card'
import { Page, PageHeader, PageSection } from '@/shared/components/page'
import { Panel } from '@/shared/components/panel'
import { Button, buttonVariants } from '@/shared/components/ui/button'
import { Input } from '@/shared/components/ui/input'
import { Switch } from '@/shared/components/ui/switch'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'
import { PlannedNotice } from '@/shared/simulation/planned'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { useSimulation } from '@/shared/simulation/simulation-provider'
import { findService, serviceEndpoint } from './service-catalog'
import { TerminalWindow } from './terminal-window'

export function ServiceDetailPage() {
  const { service: id } = useParams({ from: '/services_/$service' })
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const simulation = useSimulation()
  const service = findService(id)
  const [terminalOpen, setTerminalOpen] = useState(false)

  const enabled = useQuery({
    queryKey: ['settings', service?.settingsPath],
    queryFn: () => api<boolean>(`/api/v1/settings/${service?.settingsPath}`),
    enabled: Boolean(service?.settingsPath),
  })
  const state = useQuery({
    queryKey: ['state', service?.statePath],
    queryFn: () => api<Record<string, unknown>>(`/api/v1/state/${service?.statePath}`),
    enabled: Boolean(service?.statePath),
    retry: false,
  })
  const title = service ? t(`services.${service.id}.title`) : id
  const update = useMutationFeedback<TaskAccepted, boolean>({
    mutationFn: (value) => api<TaskAccepted>(`/api/v1/settings/${service?.settingsPath}`, json('PUT', value)),
    success: (_data, value) => t(value ? 'services.enabledToast' : 'services.disabledToast', { name: title }),
    failure: title,
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['settings', service?.settingsPath] }),
  })

  if (!service) {
    return <Page><Panel><p className="py-6 text-center text-sm text-muted-foreground">{t('services.unknown', { id })}</p></Panel></Page>
  }

  const Icon = service.icon
  const simulated = service.settingsPath === undefined
  const on = simulated ? simulation.terminalEnabled : enabled.data === true
  const observed = typeof state.data?.state === 'string' ? state.data.state : undefined
  const endpoint = serviceEndpoint(state.data)

  return (
    <Page>
      <PageHeader
        title={title}
        description={t(`services.${service.id}.warning`)}
        media={<Icon />}
        back={<Link to="/services" className={buttonVariants({ variant: 'outline', size: 'sm' })}><ChevronLeft aria-hidden="true" />{t('services.title')}</Link>}
        action={(
          <>
            {simulated && on ? <Button onClick={() => setTerminalOpen(true)}><TerminalSquare />{t('services.terminal.open')}</Button> : null}
            <span className="text-sm font-medium">{t(on ? 'common.states.enabled' : 'common.states.disabled')}</span>
            <Switch
              checked={on}
              onCheckedChange={(value) => { if (simulated) { simulation.setTerminalEnabled(value); if (!value) setTerminalOpen(false) } else update.mutate(value) }}
              disabled={!simulated && (enabled.isPending || enabled.isError || update.isPending)}
              aria-label={title}
            />
          </>
        )}
      />

      {enabled.error ? <Callout tone="danger" title={failureDetail(enabled.error, t('common.requestFailed'))} /> : null}

      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
        <MetricCard
          label={t('services.detail.configured')}
          value={t(on ? 'common.states.enabled' : 'common.states.disabled')}
          caption={update.isPending ? t('services.saving') : t('services.detail.settled')}
        />
        <MetricCard
          label={t('services.detail.observed')}
          value={simulated ? t('common.notAvailable') : observed ?? t('common.states.unknown')}
          caption={simulated ? t('services.detail.noObserver') : state.isError ? t('services.noLiveState') : t('services.liveAvailable')}
        />
        <MetricCard
          label={t('services.detail.endpoint')}
          mono
          value={<span className="text-sm break-all">{endpoint ?? (simulated ? t('services.terminal.endpoint') : t('common.notAvailable'))}</span>}
          caption={endpoint ? t('services.detail.endpointReported') : t('services.detail.endpointUnreported')}
        />
      </div>

      <PageSection title={t('services.detail.configuration')}>
        <Panel>
          <PlannedNotice>{t('services.detail.configPlanned')}</PlannedNotice>
          {service.id === 'containers' ? <ContainerForm /> : service.id === 'mqtt' ? <MqttForm /> : <TerminalForm />}
        </Panel>
      </PageSection>

      <SimulationNotice scope={title} />
      {simulated ? <TerminalWindow open={terminalOpen} onClose={() => setTerminalOpen(false)} /> : null}
    </Page>
  )
}

/// The prototype's configuration forms. The device has no endpoint for any of
/// them, so they are disabled rather than offered as controls that look like
/// they save. `PlannedNotice` above says why.
function ContainerForm() {
  const { t } = useTranslation()
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <FormField label={t('services.containers.hub')}>{(id) => <Input id={id} className="font-mono" disabled placeholder="registry.example.com" />}</FormField>
      <FormField label={t('services.containers.mirror')}>{(id) => <Input id={id} className="font-mono" disabled placeholder={t('common.optional')} />}</FormField>
      <FormField label={t('services.containers.user')}>{(id) => <Input id={id} disabled placeholder={t('common.optional')} />}</FormField>
      <FormField label={t('services.containers.token')}>{(id) => <Input id={id} type="password" disabled placeholder={t('common.optional')} />}</FormField>
    </div>
  )
}

function MqttForm() {
  const { t } = useTranslation()
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <FormField label={t('services.mqtt.listen')}>{(id) => <Input id={id} className="font-mono" disabled placeholder="0.0.0.0:1883" />}</FormField>
      <FormField label={t('services.mqtt.listenTls')}>{(id) => <Input id={id} className="font-mono" disabled placeholder="0.0.0.0:8883" />}</FormField>
      <FormField className="sm:col-span-2" label={t('services.mqtt.anonymous')}>{() => <Switch disabled aria-label={t('services.mqtt.anonymous')} />}</FormField>
    </div>
  )
}

function TerminalForm() {
  const { t } = useTranslation()
  return (
    <div className="grid gap-4 sm:grid-cols-2">
      <FormField label={t('services.terminal.timeout')}>{(id) => <Input id={id} disabled placeholder="15 min" />}</FormField>
      <FormField label={t('services.terminal.shell')}>{(id) => <Input id={id} className="font-mono" disabled placeholder="/bin/bash" />}</FormField>
    </div>
  )
}
