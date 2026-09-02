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
import { EmptyState, Page, PageHeader, Surface } from '@/shared/components/product-layout'
import { StatusBadge, type StatusTone } from '@/shared/components/status-badge'
import { Button } from '@/shared/components/ui/button'
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
import { Input } from '@/shared/components/ui/input'
import { Progress, ProgressLabel, ProgressValue } from '@/shared/components/ui/progress'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/shared/components/ui/tabs'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/shared/components/ui/table'
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from '@/shared/components/ui/alert-dialog'

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
        <div className="callout error blocked-banner" role="alert">
          <div><strong>{t('applications.blocked.title')}</strong><span>{t('applications.blocked.copy')}</span></div>
          <Link to="/services/$service" params={{ service: 'containers' }} className="text-link">{t('applications.blocked.action')}</Link>
        </div>
      ) : null}
      <Tabs defaultValue="installed">
        <TabsList aria-label={t('applications.title')}>
          <TabsTrigger value="installed">{t('applications.tabs.installed')}</TabsTrigger>
          <TabsTrigger value="catalog">{t('applications.tabs.catalog')}</TabsTrigger>
          <TabsTrigger value="activity">{t('applications.tabs.activity')}</TabsTrigger>
        </TabsList>
        <TabsContent value="installed" className="tab-panel">
          <div className="toolbar">
            <label className="search-field"><Search aria-hidden="true" /><Input value={query} onChange={(event) => setQuery(event.target.value)} placeholder={t('applications.search')} /></label>
            <Select value={source} onValueChange={(value) => setSource(value as AppFilter['source'])}>
              <SelectTrigger className="w-auto" aria-label={t('applications.columns.source')}><SelectValue /></SelectTrigger>
              <SelectContent>{(['all', 'catalog', 'local', 'system'] as const).map((option) => <SelectItem value={option} key={option}>{option === 'all' ? t('applications.columns.source') : t(`applications.sources.${option}`)}</SelectItem>)}</SelectContent>
            </Select>
            <Select value={kind} onValueChange={(value) => setKind(value as AppFilter['kind'])}>
              <SelectTrigger className="w-auto" aria-label={t('applications.columns.kind')}><SelectValue /></SelectTrigger>
              <SelectContent>{(['all', 'container', 'native'] as const).map((option) => <SelectItem value={option} key={option}>{option === 'all' ? t('applications.columns.kind') : t(`applications.kinds.${option}`)}</SelectItem>)}</SelectContent>
            </Select>
            <span className="field-hint">{t('applications.count', { count: installed.length })} · {t('applications.retained', { count: retained })}</span>
          </div>
          <Surface className="table-surface">
            <Table>
              <TableHeader><TableRow><TableHead>{t('applications.columns.application')}</TableHead><TableHead>{t('applications.columns.source')}</TableHead><TableHead>{t('applications.columns.kind')}</TableHead><TableHead>{t('applications.columns.desired')}</TableHead><TableHead>{t('applications.columns.runtime')}</TableHead><TableHead>{t('applications.columns.health')}</TableHead><TableHead /></TableRow></TableHeader>
              <TableBody>{installed.map((app) => (
                <TableRow key={app.id}>
                  <TableCell><button type="button" className="table-primary-link" onClick={() => setDetails(app.id)}>{app.name}</button><small className="mono">{app.version}</small></TableCell>
                  <TableCell>{t(`applications.sources.${app.source}`)}</TableCell>
                  <TableCell><StatusBadge>{t(`applications.kinds.${app.kind}`)}</StatusBadge></TableCell>
                  <TableCell>{t(`common.states.${app.desired}`)}</TableCell>
                  <TableCell><StatusBadge tone={runtimeTone(app.runtime)}>{runtimeLabel(app.runtime, t)}</StatusBadge></TableCell>
                  <TableCell>{t(`applications.states.${app.health}`)}</TableCell>
                  <TableCell className="text-right"><AppAction app={app} onOpen={() => setDetails(app.id)} /></TableCell>
                </TableRow>
              ))}</TableBody>
            </Table>
            {installed.length === 0 ? <EmptyState title={t('applications.empty')} /> : null}
          </Surface>
        </TabsContent>
        <TabsContent value="catalog" className="tab-panel">
          <p className="tab-caption">{t('applications.catalogCached')}</p>
          <div className="catalog-grid">
            {catalogIds.map((id) => {
              const app = simulation.apps.find((item) => item.id === id)!
              const detailKey = id === 'mqtt-bridge' ? 'mqtt' : id === 'serial-bridge' ? 'serial' : 'agent'
              return (
                <Surface className="catalog-card" key={id}>
                  <div className="catalog-head"><div><strong>{id === 'mqtt-bridge' ? app.name : t(`applications.catalogItems.${detailKey}.name`)}</strong><span>{t(`applications.catalogItems.${detailKey}.publisher`)}</span></div><StatusBadge>{t(`applications.kinds.${app.kind}`)}</StatusBadge></div>
                  <StatusBadge tone="success">{t('applications.install.compatible')}</StatusBadge>
                  <dl className="compact-details">
                    <div><dt>{t('applications.install.version')}</dt><dd className="mono">{app.version}</dd></div>
                    <div><dt>{t('applications.download')}</dt><dd>{t(`applications.catalogItems.${detailKey}.size`)}</dd></div>
                    <div><dt>{t('applications.persistent')}</dt><dd>{t(`applications.catalogItems.${detailKey}.storage`)}</dd></div>
                    <div><dt>{t('applications.permissions')}</dt><dd>{t(`applications.catalogItems.${detailKey}.permissions`)}</dd></div>
                  </dl>
                  <Button variant="outline" size="sm" onClick={() => openInstall(id)} disabled={app.runtime !== 'not-installed'}>
                    <Download />{app.runtime === 'not-installed' ? t('applications.actions.install') : t('applications.states.running')}
                  </Button>
                </Surface>
              )
            })}
          </div>
        </TabsContent>
        <TabsContent value="activity" className="tab-panel">
          <Surface className="table-surface">
            <Table><TableHeader><TableRow><TableHead>{t('applications.columns.action')}</TableHead><TableHead>{t('applications.columns.application')}</TableHead><TableHead>{t('applications.columns.result')}</TableHead><TableHead className="text-right">{t('applications.columns.time')}</TableHead></TableRow></TableHeader>
              <TableBody>{simulation.activity.map((item) => <TableRow key={item.id}><TableCell>{t(`applications.activityActions.${item.action}`)}</TableCell><TableCell>{item.app}</TableCell><TableCell><StatusBadge tone="success">{t('common.states.succeeded')}</StatusBadge></TableCell><TableCell className="text-right">{t(`applications.activityTimes.${item.time}`)}</TableCell></TableRow>)}</TableBody>
            </Table>
          </Surface>
        </TabsContent>
      </Tabs>
      <SimulationNotice scope={t('applications.title')} />

      <Dialog open={Boolean(selected)} onOpenChange={(open) => { if (!open) setSelected(undefined) }}>
        <DialogContent className="install-dialog" showCloseButton={false}>
          <DialogHeader><DialogTitle>{t('applications.install.title', { name: selectedApp?.name })}</DialogTitle><DialogDescription>{t('applications.description')}</DialogDescription></DialogHeader>
          <ol className="wizard-steps">
            {(['compatibility', 'access', 'confirm', 'progress'] as const).map((key, index) => <li key={key} data-active={index <= step || undefined}><span>{index < step ? <Check /> : index + 1}</span>{t(`applications.install.steps.${key}`)}</li>)}
          </ol>
          <div className="wizard-body">
            {step === 0 ? <CompatibilityStep app={selectedApp} /> : null}
            {step === 1 ? <AccessStep /> : null}
            {step === 2 ? <p>{t('applications.install.changes')}</p> : null}
            {step === 3 ? <div className="install-complete"><span><Check /></span><strong>{t('applications.install.done', { name: selectedApp?.name })}</strong><Progress value={100}><ProgressLabel>{t('applications.install.steps.progress')}</ProgressLabel><ProgressValue /></Progress></div> : null}
          </div>
          <DialogFooter>
            {step > 0 && step < 3 ? <Button variant="outline" onClick={() => setStep((value) => value - 1)}>{t('applications.install.back')}</Button> : null}
            {step < 2 ? <Button onClick={() => setStep((value) => value + 1)}>{t('applications.install.next')}<ChevronRight /></Button> : null}
            {step === 2 ? <Button onClick={confirmInstall}>{t('applications.install.confirm')}</Button> : null}
            {step === 3 ? <Button onClick={() => setSelected(undefined)}>{t('applications.install.open')}</Button> : null}
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <Dialog open={Boolean(details)} onOpenChange={(open) => { if (!open) setDetails(undefined) }}>
        <DialogContent><DialogHeader><DialogTitle>{detailApp?.name}</DialogTitle><DialogDescription>{t('applications.details.description')}</DialogDescription></DialogHeader>{detailApp ? <dl className="details"><div><dt>{t('applications.details.version')}</dt><dd><code>{detailApp.version}</code></dd></div><div><dt>{t('applications.details.source')}</dt><dd>{t(`applications.sources.${detailApp.source}`)}</dd></div><div><dt>{t('applications.details.kind')}</dt><dd>{t(`applications.kinds.${detailApp.kind}`)}</dd></div><div><dt>{t('applications.details.runtime')}</dt><dd>{runtimeLabel(detailApp.runtime, t)}</dd></div><div><dt>{t('applications.details.health')}</dt><dd>{t(`applications.states.${detailApp.health}`)}</dd></div><div><dt>{t('applications.details.permissions')}</dt><dd>{t('applications.details.permissionsValue')}</dd></div></dl> : null}<DialogFooter><Button variant="outline" onClick={() => setDetails(undefined)}>{t('common.actions.close')}</Button></DialogFooter></DialogContent>
      </Dialog>
    </Page>
  )
}

function AppAction({ app, onOpen }: { app: SimulatedApp; onOpen: () => void }) {
  const { t } = useTranslation()
  const simulation = useSimulation()
  return <div className="table-actions">
    <Button size="sm" variant="ghost" onClick={onOpen}>{t('applications.actions.open')}</Button>
    {app.id === 'modbus' && app.version === '1.8.2' ? <Button size="sm" variant="outline" onClick={() => simulation.updateApp(app.id)}>{t('applications.actions.update')}</Button> : null}
    <Button size="sm" variant="outline" onClick={() => simulation.toggleApp(app.id)}>{app.runtime === 'running' ? t('applications.actions.stop') : t('applications.actions.start')}</Button>
    <AlertDialog>
      <AlertDialogTrigger render={<Button size="icon-sm" variant="ghost" aria-label={t('applications.remove.title', { name: app.name })} />}><Trash2 /></AlertDialogTrigger>
      <AlertDialogContent>
        <AlertDialogHeader><AlertDialogTitle>{t('applications.remove.title', { name: app.name })}</AlertDialogTitle><AlertDialogDescription>{t('applications.remove.description')}</AlertDialogDescription></AlertDialogHeader>
        <AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => simulation.removeApp(app.id)}>{t('applications.actions.remove')}</AlertDialogAction></AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  </div>
}

function CompatibilityStep({ app }: { app?: SimulatedApp }) {
  const { t } = useTranslation()
  return <div className="wizard-list"><StatusBadge tone="success">{t('applications.install.compatible')}</StatusBadge><dl className="compact-details"><div><dt>{t('applications.install.version')}</dt><dd className="mono">{app?.version}</dd></div><div><dt>{t('applications.install.digest')}</dt><dd className="mono">sha256:9e12…f7a3</dd></div></dl></div>
}

function AccessStep() {
  const { t } = useTranslation()
  return <div className="permission-list">{[t('applications.install.network'), t('applications.install.messaging'), t('applications.install.resources'), t('applications.install.storage')].map((item) => <div key={item}><Box /><span>{item}</span></div>)}</div>
}

function runtimeTone(runtime: SimulatedRuntime): StatusTone {
  if (runtime === 'running') return 'success'
  if (runtime === 'blocked') return 'danger'
  return 'neutral'
}

function runtimeLabel(runtime: SimulatedRuntime, t: ReturnType<typeof useTranslation>['t']) {
  return t(`applications.states.${runtime === 'not-installed' ? 'notInstalled' : runtime}`)
}
