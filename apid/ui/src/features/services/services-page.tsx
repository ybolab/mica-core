import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Boxes, RadioTower, TerminalSquare } from 'lucide-react'
import { useState } from 'react'
import { api, errorMessage, json } from '@/shared/lib/http'
import type { TaskAccepted } from '@/lib/types'
import { Page, PageHeader, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { TaskProgress } from '@/shared/components/task-progress'
import { Button } from '@/shared/components/ui/button'
import { Switch } from '@/shared/components/ui/switch'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { useSimulation } from '@/shared/simulation/simulation-provider'

const services = [
  { id: 'containers', settingsPath: 'container.enabled', statePath: 'container', icon: Boxes },
  { id: 'mqtt', settingsPath: 'mqtt.enabled', statePath: 'mqtt', icon: RadioTower },
] as const

export function ServicesPage() {
  const { t } = useTranslation()
  const simulation = useSimulation()
  const [terminalOpen, setTerminalOpen] = useState(false)
  const setTerminalEnabled = (enabled: boolean) => {
    simulation.setTerminalEnabled(enabled)
    if (!enabled) setTerminalOpen(false)
  }

  return (
    <Page>
      <PageHeader title={t('services.title')} description={t('services.description')} />
      <div className="service-grid">
        {services.map((service) => <ServiceCard key={service.id} {...service} />)}
        <Surface className="service-card">
          <header><span className="service-icon"><TerminalSquare /></span><StatusBadge tone={simulation.terminalEnabled ? 'success' : 'neutral'}>{t(simulation.terminalEnabled ? 'common.states.enabled' : 'common.states.disabled')}</StatusBadge></header>
          <h3>{t('services.terminal.title')}</h3>
          <p>{t('services.terminal.description')}</p>
          <div className="service-state"><span>{t('services.terminal.browserSession')}</span><Switch checked={simulation.terminalEnabled} onCheckedChange={setTerminalEnabled} aria-label={t('services.terminal.title')} /></div>
          <footer><span>{t('services.terminal.scope')}</span><Button size="sm" variant="outline" disabled={!simulation.terminalEnabled} onClick={() => setTerminalOpen(true)}>{t('services.terminal.open')}</Button></footer>
        </Surface>
      </div>
      <div className="section-layout">
        <div className="section-copy"><h2>{t('services.runtime.title')}</h2><p>{t('services.runtime.description')}</p></div>
        <Surface>
          {simulation.terminalEnabled ? <TerminalScreen label={t('services.terminal.title')} /> : <div className="empty"><TerminalSquare />{t('services.terminal.disabledCopy')}</div>}
        </Surface>
      </div>
      <SimulationNotice scope={t('services.terminal.title')} />
      <Dialog open={terminalOpen && simulation.terminalEnabled} onOpenChange={setTerminalOpen}><DialogContent showCloseButton={false}><DialogHeader><DialogTitle>{t('services.terminal.title')}</DialogTitle><DialogDescription>{t('services.runtime.description')}</DialogDescription></DialogHeader><TerminalScreen label={t('services.terminal.title')} /><DialogFooter><Button variant="outline" onClick={() => setTerminalOpen(false)}>{t('common.actions.close')}</Button></DialogFooter></DialogContent></Dialog>
    </Page>
  )
}

function TerminalScreen({ label }: { label: string }) {
  return <div className="terminal" aria-label={label}><p>mos@mos-cm4:~$ systemctl --no-pager status mosd</p><p>● mosd.service - mos settings daemon</p><p>&nbsp;&nbsp;&nbsp;Active: active (running)</p><p>&nbsp;&nbsp;&nbsp;Tasks: 8 (limit: 3834)</p><p className="terminal-cursor">mos@mos-cm4:~$ </p></div>
}

function ServiceCard({ id, settingsPath, statePath, icon: Icon }: typeof services[number]) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const enabled = useQuery({ queryKey: ['settings', settingsPath], queryFn: () => api<boolean>(`/api/v1/settings/${settingsPath}`) })
  const state = useQuery({ queryKey: ['state', statePath], queryFn: () => api<Record<string, unknown>>(`/api/v1/state/${statePath}`), retry: false })
  const update = useMutation({
    mutationFn: (value: boolean) => api<TaskAccepted>(`/api/v1/settings/${settingsPath}`, json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', settingsPath] }),
  })
  const title = t(`services.${id}.title`)
  const enabledState = enabled.isPending ? 'common.states.pending' : enabled.isError ? 'common.states.unknown' : enabled.data ? 'common.states.enabled' : 'common.states.disabled'
  const liveState = state.isPending ? t('common.states.pending') : state.data ? t('services.liveAvailable') : t('services.noLiveState')

  return (
    <Surface className="service-card">
      <header><span className="service-icon"><Icon /></span><StatusBadge tone={enabled.isPending || enabled.isError ? 'warning' : enabled.data ? 'success' : 'neutral'}>{t(enabledState)}</StatusBadge></header>
      <h3>{title}</h3>
      <p>{t(`services.${id}.warning`)}</p>
      <dl className="catalog-meta">
        <div><dt>{t('services.liveState')}</dt><dd>{liveState}</dd></div>
        <div><dt>{t('services.managedBy')}</dt><dd>mosd</dd></div>
      </dl>
      {enabled.error || update.error ? <p className="callout error" role="alert">{errorMessage(enabled.error ?? update.error, t('common.requestFailed'))}</p> : null}
      <footer><StatusBadge tone={state.data ? 'success' : 'warning'}>{state.isPending ? t('common.states.pending') : state.data ? t('common.states.available') : t('common.states.unknown')}</StatusBadge><Button size="sm" variant="outline" onClick={() => update.mutate(!enabled.data)} disabled={enabled.isPending || enabled.isError || update.isPending}>{update.isPending ? t('services.saving') : enabled.data ? t('common.actions.disable') : t('common.actions.enable')}</Button></footer>
      <TaskProgress taskId={update.data?.taskId} />
    </Surface>
  )
}
