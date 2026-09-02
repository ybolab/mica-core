import { useState } from 'react'
import { Link, useParams } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ChevronLeft, TerminalSquare } from 'lucide-react'
import { api, errorMessage, json } from '@/shared/lib/http'
import type { TaskAccepted } from '@/lib/types'
import { Page, Section, Surface } from '@/shared/components/product-layout'
import { Button } from '@/shared/components/ui/button'
import { Input } from '@/shared/components/ui/input'
import { Label } from '@/shared/components/ui/label'
import { Switch } from '@/shared/components/ui/switch'
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
  const update = useMutation({
    mutationFn: (value: boolean) => api<TaskAccepted>(`/api/v1/settings/${service?.settingsPath}`, json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', service?.settingsPath] }),
  })

  if (!service) {
    return <Page><Surface><p className="empty">{t('services.unknown', { id })}</p></Surface></Page>
  }

  const Icon = service.icon
  const simulated = service.settingsPath === undefined
  const on = simulated ? simulation.terminalEnabled : enabled.data === true
  const observed = typeof state.data?.state === 'string' ? state.data.state : undefined
  const endpoint = serviceEndpoint(state.data)
  const title = t(`services.${service.id}.title`)

  return (
    <Page>
      <header className="detail-head">
        <Link to="/services" className="text-link"><ChevronLeft aria-hidden="true" />{t('services.title')}</Link>
        <div className="detail-head-row">
          <div className="service-identity">
            <span className="service-icon"><Icon /></span>
            <div><h1>{title}</h1><p>{t(`services.${service.id}.warning`)}</p></div>
          </div>
          <div className="detail-actions">
          {simulated && on ? <Button onClick={() => setTerminalOpen(true)}><TerminalSquare />{t('services.terminal.open')}</Button> : null}
          <span className="service-desired">{t(on ? 'common.states.enabled' : 'common.states.disabled')}</span>
          <Switch
            checked={on}
            onCheckedChange={(value) => { if (simulated) { simulation.setTerminalEnabled(value); if (!value) setTerminalOpen(false) } else update.mutate(value) }}
            disabled={!simulated && (enabled.isPending || enabled.isError || update.isPending)}
              aria-label={title}
            />
          </div>
        </div>
      </header>

      {enabled.error || update.error ? <p className="callout error" role="alert">{errorMessage(enabled.error ?? update.error, t('common.requestFailed'))}</p> : null}

      <div className="content-grid content-grid-wide">
        <Surface className="metric">
          <span>{t('services.detail.configured')}</span>
          <strong>{t(on ? 'common.states.enabled' : 'common.states.disabled')}</strong>
          <small>{update.isPending ? t('services.saving') : t('services.detail.settled')}</small>
        </Surface>
        <Surface className="metric">
          <span>{t('services.detail.observed')}</span>
          <strong>{simulated ? t('common.notAvailable') : observed ?? t('common.states.unknown')}</strong>
          <small>{simulated ? t('services.detail.noObserver') : state.isError ? t('services.noLiveState') : t('services.liveAvailable')}</small>
        </Surface>
        <Surface className="metric">
          <span>{t('services.detail.endpoint')}</span>
          <strong className="mono key-value">{endpoint ?? (simulated ? t('services.terminal.endpoint') : t('common.notAvailable'))}</strong>
          <small>{endpoint ? t('services.detail.endpointReported') : t('services.detail.endpointUnreported')}</small>
        </Surface>
      </div>

      <Section title={t('services.detail.configuration')}>
        <Surface>
          <PlannedNotice>{t('services.detail.configPlanned')}</PlannedNotice>
          {service.id === 'containers' ? <ContainerForm /> : service.id === 'mqtt' ? <MqttForm /> : <TerminalForm />}
        </Surface>
      </Section>

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
    <div className="content-grid">
      <div className="field"><Label>{t('services.containers.hub')}</Label><Input className="mono" disabled placeholder="registry.example.com" /></div>
      <div className="field"><Label>{t('services.containers.mirror')}</Label><Input className="mono" disabled placeholder={t('common.optional')} /></div>
      <div className="field"><Label>{t('services.containers.user')}</Label><Input disabled placeholder={t('common.optional')} /></div>
      <div className="field"><Label>{t('services.containers.token')}</Label><Input type="password" disabled placeholder={t('common.optional')} /></div>
    </div>
  )
}

function MqttForm() {
  const { t } = useTranslation()
  return (
    <div className="content-grid">
      <div className="field"><Label>{t('services.mqtt.listen')}</Label><Input className="mono" disabled placeholder="0.0.0.0:1883" /></div>
      <div className="field"><Label>{t('services.mqtt.listenTls')}</Label><Input className="mono" disabled placeholder="0.0.0.0:8883" /></div>
      <div className="field span-2"><Label>{t('services.mqtt.anonymous')}</Label><Switch disabled aria-label={t('services.mqtt.anonymous')} /></div>
    </div>
  )
}

function TerminalForm() {
  const { t } = useTranslation()
  return (
    <div className="content-grid">
      <div className="field"><Label>{t('services.terminal.timeout')}</Label><Input disabled placeholder="15 min" /></div>
      <div className="field"><Label>{t('services.terminal.shell')}</Label><Input className="mono" disabled placeholder="/bin/bash" /></div>
    </div>
  )
}
