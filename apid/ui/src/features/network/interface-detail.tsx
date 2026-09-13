import { useState, type FormEvent } from 'react'
import { Link, useParams } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Check, ChevronLeft } from 'lucide-react'
import { api, json } from '@/shared/lib/http'
import type { Health, NetworkOverview, ObservedNetworkInterface, ObservedNetworkState, TaskAccepted, TaskRecord } from '@/lib/types'
import { FactList } from '@/shared/components/fact-list'
import { FormDialog } from '@/shared/components/form-dialog'
import { FormField } from '@/shared/components/form-field'
import { Page, PageHeader, PageSection } from '@/shared/components/page'
import { Panel } from '@/shared/components/panel'
import { SegmentedControl } from '@/shared/components/segmented-control'
import { StatusBadge } from '@/shared/components/status-badge'
import { Button, buttonVariants } from '@/shared/components/ui/button'
import { Input } from '@/shared/components/ui/input'
import { Spinner } from '@/shared/components/ui/spinner'
import { connectionState } from '@/features/shell/connection'
import { applyStage, applyStages, stageState } from './apply-stage'
import { bridgeMembership, isSessionInterface } from './interface-facts'

interface InterfaceConfig {
  kind?: 'physical' | 'vlan' | 'bridge' | 'wireguard'
  dhcp: boolean
  static?: { address: string; gateway?: string; dns: string[] }
  vlan?: { parent: string; id: number }
  bridge?: { ports: string[] }
  wireguard?: { listenPort?: number; peers: unknown[] }
}

export function InterfaceDetailPage() {
  const { name } = useParams({ from: '/network_/$name' })
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const network = useQuery({ queryKey: ['network'], queryFn: () => api<NetworkOverview>('/api/v1/network'), refetchInterval: 10_000 })
  const status = useQuery({ queryKey: ['observed-network'], queryFn: () => api<ObservedNetworkState>('/api/v1/network/status'), refetchInterval: 15_000, retry: false })
  const configuredMap = (network.data?.configured ?? {}) as Record<string, InterfaceConfig>
  const configured = configuredMap[name]
  const observed = status.data?.interfaces.entries?.find((entry) => entry.name === name)
  const bridge = bridgeMembership(configuredMap, name)

  const [dhcp, setDhcp] = useState<boolean | undefined>()
  const [address, setAddress] = useState<string | undefined>()
  const [gateway, setGateway] = useState<string | undefined>()
  const [dns, setDns] = useState<string | undefined>()
  const [review, setReview] = useState(false)
  const [taskId, setTaskId] = useState<string>()

  const useDhcp = dhcp ?? configured?.dhcp ?? true
  const addressValue = address ?? configured?.static?.address ?? ''
  const gatewayValue = gateway ?? configured?.static?.gateway ?? ''
  const dnsValue = dns ?? configured?.static?.dns.join(', ') ?? ''

  const save = useMutation({
    mutationFn: () => {
      const value: InterfaceConfig = { ...configured, dhcp: useDhcp }
      if (useDhcp) delete value.static
      else value.static = { address: addressValue, ...(gatewayValue ? { gateway: gatewayValue } : {}), dns: splitList(dnsValue) }
      return api<TaskAccepted>(`/api/v1/network/${encodeURIComponent(name)}`, json('PUT', value))
    },
    onSuccess: (accepted) => {
      setReview(false)
      setTaskId(accepted.taskId)
      void queryClient.invalidateQueries({ queryKey: ['network'] })
    },
  })

  const reset = () => { setDhcp(undefined); setAddress(undefined); setGateway(undefined); setDns(undefined) }
  const submit = (event: FormEvent) => { event.preventDefault(); setReview(true) }
  const link = observed?.link
  const online = link?.carrier === true
  const onSession = isSessionInterface((observed?.addresses ?? []).map(formatAddress), window.location.hostname)

  return (
    <Page>
      <PageHeader
        title={name}
        back={<Link to="/network" className={buttonVariants({ variant: 'outline', size: 'sm' })}><ChevronLeft aria-hidden="true" />{t('network.title')} / {t('network.tabs.interfaces')}</Link>}
        action={(
          <>
            <StatusBadge>{kindLabel(configured, observed, t)}</StatusBadge>
            <StatusBadge tone={online ? 'success' : 'danger'}>{link?.operationalState ?? t('network.notObserved')}</StatusBadge>
          </>
        )}
      />

      {taskId ? <ApplyStrip taskId={taskId} onDismiss={() => setTaskId(undefined)} /> : null}

      <PageSection title={t('network.detail.overview')}>
        <Panel>
          <FactList facts={[
            { id: 'mac', label: t('network.detail.mac'), value: observed?.hardwareAddress ?? t('common.notAvailable'), mono: true },
            { id: 'mtu', label: t('network.detail.mtu'), value: observed?.mtu ?? t('common.notAvailable'), mono: true },
            { id: 'bridge', label: t('network.detail.memberOf'), value: bridge ?? t('network.detail.noBridge'), mono: true },
            { id: 'link', label: t('network.detail.link'), value: <span className={online ? undefined : 'text-destructive'}>{link?.carrierState ?? t('common.notAvailable')}</span> },
          ]} />
        </Panel>
      </PageSection>

      <PageSection title={t('network.detail.addressing')}>
        <Panel>
          <form className="grid gap-4" onSubmit={submit}>
            <FormField label={t('network.detail.mode')}>
              {() => (
                <SegmentedControl<'dhcp' | 'static'>
                  label={t('network.detail.mode')}
                  value={useDhcp ? 'dhcp' : 'static'}
                  onValueChange={(mode) => setDhcp(mode === 'dhcp')}
                  segments={[
                    { value: 'dhcp', label: 'DHCP' },
                    { value: 'static', label: t('network.detail.static') },
                  ]}
                />
              )}
            </FormField>
            {useDhcp ? <p className="text-sm text-muted-foreground">{t('network.detail.dhcpNote')}</p> : (
              <div className="grid gap-4 sm:grid-cols-2">
                <FormField label={t('network.detail.address')}>
                  {(id) => <Input id={id} className="font-mono" value={addressValue} onChange={(event) => setAddress(event.target.value)} placeholder="10.0.0.2/24" required />}
                </FormField>
                <FormField label={t('network.detail.gateway')}>
                  {(id) => <Input id={id} className="font-mono" value={gatewayValue} onChange={(event) => setGateway(event.target.value)} placeholder="10.0.0.1" />}
                </FormField>
                <FormField className="sm:col-span-2" label={t('network.detail.dns')}>
                  {(id) => <Input id={id} className="font-mono" value={dnsValue} onChange={(event) => setDns(event.target.value)} placeholder="10.0.0.1, 1.1.1.1" />}
                </FormField>
              </div>
            )}
            <div className="flex justify-end gap-2">
              <Button type="button" variant="outline" onClick={reset}>{t('common.actions.cancel')}</Button>
              <Button type="submit">{t('network.detail.save')}</Button>
            </div>
          </form>
        </Panel>
      </PageSection>

      <PageSection title={t('network.detail.danger')} tone="danger">
        <Panel><p className="text-sm text-muted-foreground">{t('network.detail.dangerCopy', { name })}</p></Panel>
      </PageSection>

      <FormDialog
        open={review}
        onOpenChange={(next) => { if (!next) setReview(false) }}
        title={t('network.review.title')}
        submitLabel={t('network.review.apply')}
        success={t('network.review.applied', { name })}
        failure={t('network.detail.save')}
        onSubmit={() => save.mutateAsync()}
      >
        <FactList facts={[
          { id: 'change', label: t('network.review.change'), value: t(useDhcp ? 'network.detail.changeDhcp' : 'network.detail.changeStatic', { address: addressValue }), mono: true },
          { id: 'affects', label: t('network.review.affects'), value: bridge ? t('network.review.affectsBridge', { name, bridge }) : name },
          { id: 'session', label: t('network.review.session'), value: (
            <span className={onSession ? 'text-destructive' : 'text-success'}>
              {t(onSession ? 'network.review.sessionOn' : 'network.review.sessionOff', { name })}
            </span>
          ) },
          { id: 'recover', label: t('network.review.recover'), value: t('network.review.recoverCopy') },
        ]} />
      </FormDialog>
    </Page>
  )
}

function ApplyStrip({ taskId, onDismiss }: { taskId: string; onDismiss: () => void }) {
  const { t } = useTranslation()
  const health = useQuery({ queryKey: ['shell-health'], queryFn: () => api<Health>('/api/v1/health'), refetchInterval: 5_000 })
  const task = useQuery({
    queryKey: ['task', taskId],
    queryFn: () => api<TaskRecord>(`/api/v1/tasks/${encodeURIComponent(taskId)}`),
    refetchInterval: (query) => query.state.data?.status === 'finished' ? false : 1_000,
  })
  const connection = connectionState({ isError: health.isError, failureCount: health.failureCount, micad: health.data?.micad })
  const stage = applyStage(task.data, connection)
  const settled = stage === 'applied' || stage === 'failed'
  return (
    <Panel className="gap-3" contentClassName="gap-3">
      <div role="status" aria-live="polite" className="flex flex-col gap-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <strong className="text-sm font-medium">{t('network.apply.title')}</strong>
          {settled ? <StatusBadge tone={stage === 'applied' ? 'success' : 'danger'}>{t(`network.apply.${stage}`)}</StatusBadge> : null}
        </div>
        <div className="flex flex-wrap gap-5 text-sm">
          {applyStages.map((step) => {
            const state = stageState(step, stage)
            return (
              <span key={step} className={state === 'done' ? 'flex items-center gap-2 text-success' : state === 'active' ? 'flex items-center gap-2 font-semibold text-foreground' : 'flex items-center gap-2 text-muted-foreground'}>
                {state === 'done' ? <Check className="size-3.5" aria-hidden="true" /> : state === 'active' ? <Spinner className="size-3.5" /> : null}
                {t(`network.apply.steps.${step}`)}
              </span>
            )
          })}
        </div>
        <p className="text-sm text-muted-foreground">{t(`network.apply.body.${stage}`)}</p>
      </div>
      {settled ? <Button className="justify-self-start" size="sm" variant="outline" onClick={onDismiss}>{t('network.apply.dismiss')}</Button> : null}
    </Panel>
  )
}

function formatAddress(address: { address?: string; prefixLength?: number }) {
  return `${address.address ?? ''}${address.prefixLength === undefined ? '' : `/${address.prefixLength}`}`
}

function kindLabel(configured: InterfaceConfig | undefined, observed: ObservedNetworkInterface | undefined, t: ReturnType<typeof useTranslation>['t']) {
  const kind = configured?.kind ?? observed?.kind ?? observed?.type
  return kind ? t(`network.kinds.${kind}`, { defaultValue: kind }) : t('common.notAvailable')
}

function splitList(value: string) {
  return value.split(',').map((item) => item.trim()).filter(Boolean)
}
