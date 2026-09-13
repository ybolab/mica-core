import { useMemo, useState } from 'react'
import { Link } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Box, Check, ChevronRight, Download, Search, Trash2 } from 'lucide-react'
import { api } from '@/shared/lib/http'
import { filterApps, retainedCount, type AppFilter } from './filter'
import { PlannedNotice } from '@/shared/simulation/planned'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/shared/components/ui/select'
import { useSimulation, type SimulatedApp, type SimulatedRuntime } from '@/shared/simulation/simulation-provider'
import { SimulationNotice } from '@/shared/simulation/simulation-notice'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { DataTable } from '@/shared/components/data-table'
import { FactList } from '@/shared/components/fact-list'
import { Page, PageHeader } from '@/shared/components/page'
import { CollectionPanel, Panel } from '@/shared/components/panel'
import { StatusBadge, type StatusTone } from '@/shared/components/status-badge'
import { Button, buttonVariants } from '@/shared/components/ui/button'
import { Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/shared/components/ui/input-group'
import { Progress, ProgressLabel, ProgressTrack, ProgressIndicator, ProgressValue } from '@/shared/components/ui/progress'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/shared/components/ui/tabs'
import { notifySuccess } from '@/shared/feedback/toast'

const catalogIds = ['mqtt-bridge', 'serial-bridge', 'device-agent'] as const

export function ApplicationsPage() {
  const { t } = useTranslation()
  const simulation = useSimulation()
  const [query, setQuery] = useState('')
  const [source, setSource] = useState<AppFilter['source']>('all')
  const [kind, setKind] = useState<AppFilter['kind']>('all')
  const [selected, setSelected] = useState<string>()
  const [details, setDetails] = useState<string>()
  const [step, setStep] = useState(0)
  const installed = useMemo(() => filterApps(simulation.apps, { query, source, kind }), [query, source, kind, simulation.apps])
  const retained = retainedCount(installed)
  const containerRuntime = useQuery({ queryKey: ['settings', 'container.enabled'], queryFn: () => api<boolean>('/api/v1/settings/container.enabled') })
  const selectedApp = simulation.apps.find((app) => app.id === selected)
  const detailApp = simulation.apps.find((app) => app.id === details)

  const openInstall = (id: string) => {
    setSelected(id)
    setStep(0)
  }
  const confirmInstall = () => {
    if (!selected) return
    simulation.installApp(selected)
    setStep(3)
  }

  return (
    <Page>
      <PageHeader title={t('applications.title')} />
      <PlannedNotice>{t('applications.planned')}</PlannedNotice>
      {containerRuntime.data === false ? (
        <Callout tone="danger" title={t('applications.blocked.title')}>
          <span className="flex flex-wrap items-center justify-between gap-3">
            <span>{t('applications.blocked.copy')}</span>
            <Link to="/services/$service" params={{ service: 'containers' }} className={buttonVariants({ variant: 'outline', size: 'sm' })}>{t('applications.blocked.action')}</Link>
          </span>
        </Callout>
      ) : null}
      <Tabs defaultValue="installed">
        <TabsList aria-label={t('applications.title')}>
          <TabsTrigger value="installed">{t('applications.tabs.installed')}</TabsTrigger>
          <TabsTrigger value="catalog">{t('applications.tabs.catalog')}</TabsTrigger>
          <TabsTrigger value="activity">{t('applications.tabs.activity')}</TabsTrigger>
        </TabsList>
        <TabsContent value="installed" className="grid gap-4 pt-4">
          <div className="flex flex-wrap items-center gap-3">
            <InputGroup className="w-full sm:w-80">
              <InputGroupAddon><Search aria-hidden="true" /></InputGroupAddon>
              <InputGroupInput value={query} onChange={(event) => setQuery(event.target.value)} placeholder={t('applications.search')} aria-label={t('applications.search')} />
            </InputGroup>
            <Select value={source} onValueChange={(value) => setSource(value as AppFilter['source'])}>
              <SelectTrigger className="w-auto" aria-label={t('applications.columns.source')}><SelectValue /></SelectTrigger>
              <SelectContent>{(['all', 'catalog', 'local', 'system'] as const).map((option) => <SelectItem value={option} key={option}>{option === 'all' ? t('applications.columns.source') : t(`applications.sources.${option}`)}</SelectItem>)}</SelectContent>
            </Select>
            <Select value={kind} onValueChange={(value) => setKind(value as AppFilter['kind'])}>
              <SelectTrigger className="w-auto" aria-label={t('applications.columns.kind')}><SelectValue /></SelectTrigger>
              <SelectContent>{(['all', 'container', 'native'] as const).map((option) => <SelectItem value={option} key={option}>{option === 'all' ? t('applications.columns.kind') : t(`applications.kinds.${option}`)}</SelectItem>)}</SelectContent>
            </Select>
            <span className="text-sm text-muted-foreground">{t('applications.count', { count: installed.length })} · {t('applications.retained', { count: retained })}</span>
          </div>
          <CollectionPanel>
            <DataTable<SimulatedApp>
              rows={installed}
              rowKey={(app) => app.id}
              empty={t('applications.empty')}
              columns={[
                { id: 'application', header: t('applications.columns.application'), cell: (app) => (
                  <span className="flex flex-col">
                    <button type="button" className="text-left font-medium text-primary hover:underline" onClick={() => setDetails(app.id)}>{app.name}</button>
                    <small className="font-mono text-muted-foreground">{app.version}</small>
                  </span>
                ) },
                { id: 'source', header: t('applications.columns.source'), cell: (app) => t(`applications.sources.${app.source}`) },
                { id: 'kind', header: t('applications.columns.kind'), cell: (app) => <StatusBadge>{t(`applications.kinds.${app.kind}`)}</StatusBadge> },
                { id: 'desired', header: t('applications.columns.desired'), cell: (app) => t(`common.states.${app.desired}`) },
                { id: 'runtime', header: t('applications.columns.runtime'), cell: (app) => <StatusBadge tone={runtimeTone(app.runtime)}>{runtimeLabel(app.runtime, t)}</StatusBadge> },
                { id: 'health', header: t('applications.columns.health'), cell: (app) => t(`applications.states.${app.health}`) },
                { id: 'actions', header: '', align: 'end', cell: (app) => <AppAction app={app} onOpen={() => setDetails(app.id)} /> },
              ]}
            />
          </CollectionPanel>
        </TabsContent>
        <TabsContent value="catalog" className="grid gap-4 pt-4">
          <p className="text-sm text-muted-foreground">{t('applications.catalogCached')}</p>
          <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {catalogIds.map((id) => {
              const app = simulation.apps.find((item) => item.id === id)!
              const detailKey = id === 'mqtt-bridge' ? 'mqtt' : id === 'serial-bridge' ? 'serial' : 'agent'
              return (
                <Panel className="h-full" key={id} contentClassName="gap-3">
                  <div className="flex items-start justify-between gap-2">
                    <div className="grid gap-0.5">
                      <strong className="font-semibold">{id === 'mqtt-bridge' ? app.name : t(`applications.catalogItems.${detailKey}.name`)}</strong>
                      <span className="text-sm text-muted-foreground">{t(`applications.catalogItems.${detailKey}.publisher`)}</span>
                    </div>
                    <StatusBadge>{t(`applications.kinds.${app.kind}`)}</StatusBadge>
                  </div>
                  <StatusBadge tone="success">{t('applications.install.compatible')}</StatusBadge>
                  <FactList facts={[
                    { id: 'version', label: t('applications.install.version'), value: app.version, mono: true },
                    { id: 'download', label: t('applications.download'), value: t(`applications.catalogItems.${detailKey}.size`) },
                    { id: 'storage', label: t('applications.persistent'), value: t(`applications.catalogItems.${detailKey}.storage`) },
                    { id: 'permissions', label: t('applications.permissions'), value: t(`applications.catalogItems.${detailKey}.permissions`) },
                  ]} />
                  <Button className="mt-auto justify-self-start" variant="outline" size="sm" onClick={() => openInstall(id)} disabled={app.runtime !== 'not-installed'}>
                    <Download />{app.runtime === 'not-installed' ? t('applications.actions.install') : t('applications.states.running')}
                  </Button>
                </Panel>
              )
            })}
          </div>
        </TabsContent>
        <TabsContent value="activity" className="grid gap-4 pt-4">
          <CollectionPanel>
            <DataTable
              rows={simulation.activity}
              rowKey={(item) => item.id}
              empty={t('applications.empty')}
              columns={[
                { id: 'action', header: t('applications.columns.action'), cell: (item) => t(`applications.activityActions.${item.action}`) },
                { id: 'application', header: t('applications.columns.application'), cell: (item) => item.app },
                { id: 'result', header: t('applications.columns.result'), cell: () => <StatusBadge tone="success">{t('common.states.succeeded')}</StatusBadge> },
                { id: 'time', header: t('applications.columns.time'), align: 'end', cell: (item) => t(`applications.activityTimes.${item.time}`) },
              ]}
            />
          </CollectionPanel>
        </TabsContent>
      </Tabs>
      <SimulationNotice scope={t('applications.title')} />

      <Dialog open={Boolean(selected)} onOpenChange={(open) => { if (!open) setSelected(undefined) }}>
        <DialogContent size="lg" showCloseButton={false}>
          <DialogHeader>
            <DialogTitle>{t('applications.install.title', { name: selectedApp?.name })}</DialogTitle>
            <DialogDescription>{t('applications.description')}</DialogDescription>
          </DialogHeader>
          <ol className="flex flex-wrap gap-4 border-b pb-3 text-sm">
            {(['compatibility', 'access', 'confirm', 'progress'] as const).map((key, index) => (
              <li key={key} className={index <= step ? 'flex items-center gap-2 font-semibold text-foreground' : 'flex items-center gap-2 text-muted-foreground'}>
                <span className={index <= step
                  ? 'grid size-5 place-items-center rounded-full border border-primary bg-primary text-[0.6875rem] font-semibold text-primary-foreground'
                  : 'grid size-5 place-items-center rounded-full border bg-card text-[0.6875rem] font-semibold'}>
                  {index < step ? <Check className="size-3" /> : index + 1}
                </span>
                {t(`applications.install.steps.${key}`)}
              </li>
            ))}
          </ol>
          <DialogBody>
            {step === 0 ? <CompatibilityStep app={selectedApp} /> : null}
            {step === 1 ? <AccessStep /> : null}
            {step === 2 ? <p className="text-sm">{t('applications.install.changes')}</p> : null}
            {step === 3 ? (
              <div className="grid justify-items-center gap-3 py-6 text-center">
                <span className="grid size-11 place-items-center rounded-full bg-success-background text-success"><Check /></span>
                <strong>{t('applications.install.done', { name: selectedApp?.name })}</strong>
                <Progress className="w-full max-w-75" value={100}>
                  <ProgressLabel>{t('applications.install.steps.progress')}</ProgressLabel>
                  <ProgressValue />
                  <ProgressTrack><ProgressIndicator /></ProgressTrack>
                </Progress>
              </div>
            ) : null}
          </DialogBody>
          <DialogFooter>
            {/* Every step has a way out. Steps 0 to 2 used to offer none, and
                the dialog suppressed its own close button. */}
            {step < 3 ? <Button variant="outline" onClick={() => setSelected(undefined)}>{t('common.actions.cancel')}</Button> : null}
            {step > 0 && step < 3 ? <Button variant="outline" onClick={() => setStep((value) => value - 1)}>{t('applications.install.back')}</Button> : null}
            {step < 2 ? <Button onClick={() => setStep((value) => value + 1)}>{t('applications.install.next')}<ChevronRight /></Button> : null}
            {step === 2 ? <Button onClick={confirmInstall}>{t('applications.install.confirm')}</Button> : null}
            {step === 3 ? <Button onClick={() => setSelected(undefined)}>{t('applications.install.open')}</Button> : null}
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <Dialog open={Boolean(details)} onOpenChange={(open) => { if (!open) setDetails(undefined) }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>{detailApp?.name}</DialogTitle>
            <DialogDescription>{t('applications.details.description')}</DialogDescription>
          </DialogHeader>
          <DialogBody>
            <FactList facts={detailApp ? [
              { id: 'version', label: t('applications.details.version'), value: detailApp.version, mono: true },
              { id: 'source', label: t('applications.details.source'), value: t(`applications.sources.${detailApp.source}`) },
              { id: 'kind', label: t('applications.details.kind'), value: t(`applications.kinds.${detailApp.kind}`) },
              { id: 'runtime', label: t('applications.details.runtime'), value: runtimeLabel(detailApp.runtime, t) },
              { id: 'health', label: t('applications.details.health'), value: t(`applications.states.${detailApp.health}`) },
              { id: 'permissions', label: t('applications.details.permissions'), value: t('applications.details.permissionsValue') },
            ] : []} />
          </DialogBody>
          <DialogFooter><Button variant="outline" onClick={() => setDetails(undefined)}>{t('common.actions.close')}</Button></DialogFooter>
        </DialogContent>
      </Dialog>
    </Page>
  )
}

function AppAction({ app, onOpen }: { app: SimulatedApp; onOpen: () => void }) {
  const { t } = useTranslation()
  const simulation = useSimulation()
  return (
    <div className="flex justify-end gap-2">
      <Button size="sm" variant="ghost" onClick={onOpen}>{t('applications.actions.open')}</Button>
      {app.id === 'modbus' && app.version === '1.8.2' ? (
        <Button size="sm" variant="outline" onClick={() => { simulation.updateApp(app.id); notifySuccess(t('applications.actions.updated', { name: app.name })) }}>
          {t('applications.actions.update')}
        </Button>
      ) : null}
      <Button
        size="sm"
        variant="outline"
        onClick={() => {
          simulation.toggleApp(app.id)
          notifySuccess(t(app.runtime === 'running' ? 'applications.actions.stopped' : 'applications.actions.started', { name: app.name }))
        }}
      >
        {app.runtime === 'running' ? t('applications.actions.stop') : t('applications.actions.start')}
      </Button>
      <ConfirmDialog
        trigger={<Button size="icon-sm" variant="ghost" aria-label={t('applications.remove.title', { name: app.name })}><Trash2 /></Button>}
        title={t('applications.remove.title', { name: app.name })}
        description={t('applications.remove.description')}
        confirmLabel={t('applications.actions.remove')}
        success={t('applications.actions.removed', { name: app.name })}
        failure={t('applications.actions.remove')}
        onConfirm={() => simulation.removeApp(app.id)}
      />
    </div>
  )
}

function CompatibilityStep({ app }: { app?: SimulatedApp }) {
  const { t } = useTranslation()
  return (
    <div className="grid gap-3">
      <StatusBadge tone="success">{t('applications.install.compatible')}</StatusBadge>
      <FactList facts={[
        { id: 'version', label: t('applications.install.version'), value: app?.version, mono: true },
        { id: 'digest', label: t('applications.install.digest'), value: 'sha256:9e12…f7a3', mono: true },
      ]} />
    </div>
  )
}

function AccessStep() {
  const { t } = useTranslation()
  return (
    <div className="grid gap-2">
      {[t('applications.install.network'), t('applications.install.messaging'), t('applications.install.resources'), t('applications.install.storage')].map((item) => (
        <div className="flex items-start gap-2 border-b py-2 text-sm text-muted-foreground" key={item}>
          <Box className="size-4 shrink-0 text-primary" /><span>{item}</span>
        </div>
      ))}
    </div>
  )
}

function runtimeTone(runtime: SimulatedRuntime): StatusTone {
  if (runtime === 'running') return 'success'
  if (runtime === 'blocked') return 'danger'
  return 'neutral'
}

function runtimeLabel(runtime: SimulatedRuntime, t: ReturnType<typeof useTranslation>['t']) {
  return t(`applications.states.${runtime === 'not-installed' ? 'notInstalled' : runtime}`)
}
