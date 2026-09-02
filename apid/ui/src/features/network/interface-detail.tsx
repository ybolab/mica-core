import { useState, type FormEvent } from 'react'
import { Link, useParams } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Check, ChevronLeft, Loader2 } from 'lucide-react'
import { api, errorMessage, json } from '@/shared/lib/http'
import type { Health, NetworkOverview, ObservedNetworkInterface, ObservedNetworkState, TaskAccepted, TaskRecord } from '@/lib/types'
import { Page, Section, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { Button } from '@/shared/components/ui/button'
import { Input } from '@/shared/components/ui/input'
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
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

  return (
    <Page>
      <header className="page-head">
        <div>
          <Link to="/network" className="text-link"><ChevronLeft aria-hidden="true" />{t('network.title')} / {t('network.tabs.interfaces')}</Link>
          <div className="iface-title">
            <h1 className="mono">{name}</h1>
            <StatusBadge>{kindLabel(configured, observed, t)}</StatusBadge>
            <StatusBadge tone={online ? 'success' : 'danger'}>{link?.operationalState ?? t('network.notObserved')}</StatusBadge>
          </div>
        </div>
      </header>

      {taskId ? <ApplyStrip taskId={taskId} onDismiss={() => setTaskId(undefined)} /> : null}

      <Section title={t('network.detail.overview')}>
        <Surface className="fact-grid">
          <div><span>{t('network.detail.mac')}</span><span className="mono">{observed?.hardwareAddress ?? t('common.notAvailable')}</span></div>
          <div><span>{t('network.detail.mtu')}</span><span className="mono">{observed?.mtu ?? t('common.notAvailable')}</span></div>
          <div><span>{t('network.detail.memberOf')}</span><span className="mono">{bridge ?? t('network.detail.noBridge')}</span></div>
          <div><span>{t('network.detail.link')}</span><span className={online ? undefined : 'text-danger'}>{link?.carrierState ?? t('common.notAvailable')}</span></div>
        </Surface>
      </Section>

      <Section title={t('network.detail.addressing')}>
        <Surface>
          <form className="stack" onSubmit={submit}>
            <div className="field">
              <span className="field-label">{t('network.detail.mode')}</span>
              <div className="segmented" role="radiogroup" aria-label={t('network.detail.mode')}>
                <button type="button" role="radio" aria-checked={useDhcp} data-active={useDhcp || undefined} onClick={() => setDhcp(true)}>DHCP</button>
                <button type="button" role="radio" aria-checked={!useDhcp} data-active={!useDhcp || undefined} onClick={() => setDhcp(false)}>{t('network.detail.static')}</button>
              </div>
            </div>
            {useDhcp ? <p className="field-hint">{t('network.detail.dhcpNote')}</p> : (
              <div className="content-grid">
                <div className="field"><label htmlFor="iface-address">{t('network.detail.address')}</label><Input id="iface-address" className="mono" value={addressValue} onChange={(event) => setAddress(event.target.value)} placeholder="10.0.0.2/24" required /></div>
                <div className="field"><label htmlFor="iface-gateway">{t('network.detail.gateway')}</label><Input id="iface-gateway" className="mono" value={gatewayValue} onChange={(event) => setGateway(event.target.value)} placeholder="10.0.0.1" /></div>
                <div className="field span-2"><label htmlFor="iface-dns">{t('network.detail.dns')}</label><Input id="iface-dns" className="mono" value={dnsValue} onChange={(event) => setDns(event.target.value)} placeholder="10.0.0.1, 1.1.1.1" /></div>
              </div>
            )}
            {save.error ? <p className="callout error" role="alert">{errorMessage(save.error, t('common.requestFailed'))}</p> : null}
            <div className="form-actions">
              <Button type="button" variant="outline" onClick={reset}>{t('common.actions.cancel')}</Button>
              <Button type="submit">{t('network.detail.save')}</Button>
            </div>
          </form>
        </Surface>
      </Section>

      <Section title={t('network.detail.danger')} className="danger-section">
        <Surface><p className="field-hint">{t('network.detail.dangerCopy', { name })}</p></Surface>
      </Section>

      <ReviewDialog
        open={review}
        onClose={() => setReview(false)}
        onApply={() => save.mutate()}
        pending={save.isPending}
        name={name}
        bridge={bridge}
        change={t(useDhcp ? 'network.detail.changeDhcp' : 'network.detail.changeStatic', { address: addressValue })}
        onSession={isSessionInterface((observed?.addresses ?? []).map(formatAddress), window.location.hostname)}
      />
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
  const connection = connectionState({ isError: health.isError, failureCount: health.failureCount, mosd: health.data?.mosd })
  const stage = applyStage(task.data, connection)
  const settled = stage === 'applied' || stage === 'failed'
  return (
    <Surface role="status" aria-live="polite" className="apply-strip">
      <div className="apply-head">
        <strong>{t('network.apply.title')}</strong>
        {settled ? <StatusBadge tone={stage === 'applied' ? 'success' : 'danger'}>{t(`network.apply.${stage}`)}</StatusBadge> : null}
      </div>
      <div className="apply-steps">
        {applyStages.map((step) => {
          const state = stageState(step, stage)
          return (
            <span key={step} data-state={state}>
              {state === 'done' ? <Check aria-hidden="true" /> : state === 'active' ? <Loader2 className="animate-spin" aria-hidden="true" /> : null}
              {t(`network.apply.steps.${step}`)}
            </span>
          )
        })}
      </div>
      <p className="field-hint">{t(`network.apply.body.${stage}`)}</p>
      {settled ? <Button size="sm" variant="outline" onClick={onDismiss}>{t('network.apply.dismiss')}</Button> : null}
    </Surface>
  )
}

function ReviewDialog({ open, onClose, onApply, pending, name, bridge, change, onSession }: {
  open: boolean
  onClose: () => void
  onApply: () => void
  pending: boolean
  name: string
  bridge?: string
  change: string
  onSession: boolean
}) {
  const { t } = useTranslation()
  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next) onClose() }}>
      <DialogContent showCloseButton={false}>
        <DialogHeader><DialogTitle>{t('network.review.title')}</DialogTitle></DialogHeader>
        <dl className="review-list">
          <div><dt>{t('network.review.change')}</dt><dd className="mono">{change}</dd></div>
          <div><dt>{t('network.review.affects')}</dt><dd>{bridge ? t('network.review.affectsBridge', { name, bridge }) : name}</dd></div>
          <div><dt>{t('network.review.session')}</dt><dd className={onSession ? 'text-danger' : 'text-success'}>{t(onSession ? 'network.review.sessionOn' : 'network.review.sessionOff', { name })}</dd></div>
          <div><dt>{t('network.review.recover')}</dt><dd>{t('network.review.recoverCopy')}</dd></div>
        </dl>
        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>{t('common.actions.cancel')}</Button>
          <Button type="button" onClick={onApply} disabled={pending}>{t('network.review.apply')}</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
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
