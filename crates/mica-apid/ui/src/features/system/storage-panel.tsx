import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { FolderTree, HardDrive, Layers, ShieldQuestion } from 'lucide-react'
import { api } from '@/shared/lib/http'
import type { StorageBind, StorageMedium, StorageStatus, StorageTier } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { FactList } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'
import { Spinner } from '@/shared/components/ui/spinner'
import { failureDetail } from '@/shared/feedback/toast'
import { formatBytes } from '@/shared/components/fact'

function useStorage() {
  return useQuery({
    queryKey: ['storage-status'],
    queryFn: () => api<StorageStatus>('/api/v1/storage/status'),
    refetchInterval: 30_000,
    retry: false,
  })
}

export function StoragePanel() {
  return (
    <div className="grid gap-3">
      <TiersPanel />
      <NamespacesPanel />
      <div className="grid gap-3 lg:grid-cols-2"><MediaPanel /><LifecyclePanel /></div>
    </div>
  )
}

export function TiersPanel() {
  const { t } = useTranslation()
  const status = useStorage()
  return (
    <Panel title={t('system.storage.tiers.title')} description={t('system.storage.tiers.description')} action={<Layers className="size-5 text-muted-foreground" />}>
      {status.isPending ? <p className="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />{t('system.storage.tiers.loading')}</p> : null}
      <FactList facts={(status.data?.tiers ?? []).map((tier) => ({ id: tier.name, label: tier.name, value: tierSummary(tier, t) }))} />
      {status.error ? <Callout tone="danger" title={failureDetail(status.error, t('common.requestFailed'))} /> : null}
    </Panel>
  )
}

export function NamespacesPanel() {
  const { t } = useTranslation()
  const status = useStorage()
  const namespaces = status.data?.namespaces
  return (
    <Panel title={t('system.storage.namespaces.title')} description={t('system.storage.namespaces.description')} action={<FolderTree className="size-5 text-muted-foreground" />}>
      <FactList facts={(namespaces?.binds ?? []).map((bind) => ({ id: bind.name, label: bind.mount, value: bindSummary(bind, t) }))} />
      {/* One pool, two views. Said in the UI as well as the API, because a
          reader looking at two mounts will otherwise assume two capacities. */}
      {namespaces ? <Callout title={t('system.storage.namespaces.sharedPool', { tier: namespaces.sharedCapacityTier })} /> : null}
    </Panel>
  )
}

export function MediaPanel() {
  const { t } = useTranslation()
  const status = useStorage()
  return (
    <Panel title={t('system.storage.media.title')} description={t('system.storage.media.description')} action={<HardDrive className="size-5 text-muted-foreground" />}>
      <FactList facts={(status.data?.media ?? []).map((medium) => ({
        id: medium.name,
        label: `${medium.name}${medium.sizeBytes ? ` · ${formatBytes(medium.sizeBytes)}` : ''}`,
        value: mediumHealth(medium, t),
      }))} />
      {status.data && status.data.media.length === 0 ? <Callout tone="warning" title={t('system.storage.media.none')} /> : null}
    </Panel>
  )
}

export function LifecyclePanel() {
  const { t } = useTranslation()
  const status = useStorage()
  return (
    <Panel title={t('system.storage.lifecycle.title')} description={t('system.storage.lifecycle.description')} action={<ShieldQuestion className="size-5 text-muted-foreground" />}>
      <FactList facts={Object.entries(status.data?.lifecycle ?? {}).map(([name, decision]) => ({
        id: name,
        label: t(`system.storage.lifecycle.${name}`, { defaultValue: name }),
        value: decision === 'unsupported' ? t('system.storage.lifecycle.unsupported') : t('system.storage.lifecycle.supported'),
      }))} />
    </Panel>
  )
}

/// One tier in one line: what it is, where it is, how full it is, and what the
/// last check recorded. An absent tier says so rather than showing blanks.
function tierSummary(tier: StorageTier, t: ReturnType<typeof useTranslation>['t']) {
  if (!tier.present) return t('system.storage.tiers.absent')
  const parts: string[] = []
  if (tier.mounted && tier.mount) {
    parts.push(tier.readOnly ? `${tier.mount} (${t('system.storage.tiers.readOnly')})` : tier.mount)
  } else {
    parts.push(t('system.storage.tiers.unmounted'))
  }
  if (tier.space) {
    parts.push(t('system.storage.tiers.used', { used: formatBytes(tier.space.usedBytes), total: formatBytes(tier.space.totalBytes) }))
    if (tier.space.reservedBytes > 0) {
      parts.push(t('system.storage.tiers.reserved', { reserved: formatBytes(tier.space.reservedBytes) }))
    }
  } else if (tier.partitionBytes) {
    parts.push(formatBytes(tier.partitionBytes))
  }
  if (tier.pressure) parts.push(t(`system.storage.pressure.${tier.pressure}`))
  parts.push(checkSummary(tier, t))
  return parts.join(' · ')
}

/// fsck exit 1 means errors were CORRECTED, which is the one outcome an
/// operator has to see; it gets its own sentence rather than a number.
function checkSummary(tier: StorageTier, t: ReturnType<typeof useTranslation>['t']) {
  if (tier.check.recorded === false || !tier.check.unit) return t('system.storage.tiers.neverChecked')
  if (tier.check.exitStatus === 1) return t('system.storage.tiers.checkedCorrected')
  return t('system.storage.tiers.checked', {
    result: tier.check.result ?? t('common.states.unknown'),
    status: tier.check.exitStatus ?? t('common.states.unknown'),
  })
}

/// One bind namespace in one line: who owns it, whether it is really backed
/// by DATA, and what the readiness probe did. A bind that is not on DATA is a
/// named error, never "ok" — a writer must not fall back to another
/// filesystem, and the operator has to be able to see that state. No capacity
/// is shown here: the pool is stated once, on the DATA tier.
function bindSummary(bind: StorageBind, t: ReturnType<typeof useTranslation>['t']) {
  const parts = [t(`system.storage.namespaces.owner.${bind.owner}`, { defaultValue: bind.owner })]
  parts.push(t(`system.storage.namespaces.readiness.${bind.readiness}`))
  if (bind.mounted && bind.sourceOnData === false) {
    parts.push(t('system.storage.namespaces.notOnData', { source: bind.source }))
  }
  if (bind.sourceIsDirectory === false) {
    parts.push(t('system.storage.namespaces.sourceNotDirectory', { source: bind.source }))
  }
  if (bind.readOnly) parts.push(t('system.storage.tiers.readOnly'))
  if (bind.probe) {
    if (!bind.probe.attempted) parts.push(t('system.storage.namespaces.probeSkipped', { reason: bind.probe.reason }))
    else if (bind.probe.passed) parts.push(t('system.storage.namespaces.probePassed'))
    else parts.push(t('system.storage.namespaces.probeFailed', { error: bind.probe.error }))
  }
  return parts.join(' · ')
}

/// Wear, or the reason there is none. The JEDEC estimate is a bucket range and
/// stays one: averaging it into a single percentage would invent precision the
/// device never reported. An unsupported metric is stated, never left blank.
function mediumHealth(medium: StorageMedium, t: ReturnType<typeof useTranslation>['t']) {
  if (!medium.health.supported) return t('system.storage.media.unsupported', { reason: medium.health.reason })
  const estimate = medium.health.lifetimeEstimates.find((entry) => entry.usedPercentMax !== undefined)
  const parts: string[] = []
  if (estimate) {
    parts.push(t('system.storage.media.lifetime', { min: estimate.usedPercentMin, max: estimate.usedPercentMax }))
  }
  if (medium.health.preEol) parts.push(t('system.storage.media.preEol', { state: medium.health.preEol }))
  return parts.length > 0 ? parts.join(' · ') : t('system.storage.media.unsupported', { reason: medium.health.source })
}
