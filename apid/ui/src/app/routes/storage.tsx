import { createFileRoute } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { FolderTree, HardDrive, Layers, ShieldQuestion } from 'lucide-react'
import { api, errorMessage } from '@/lib/api'
import type { StorageBind, StorageMedium, StorageStatus, StorageTier } from '@/lib/types'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'

function useStorage() {
  return useQuery({
    queryKey: ['storage-status'],
    queryFn: () => api<StorageStatus>('/api/v1/storage/status'),
    refetchInterval: 30_000,
    retry: false,
  })
}

function StoragePage() {
  const { t } = useTranslation()
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('storage.eyebrow')}</p><h1>{t('storage.title')}</h1><p>{t('storage.description')}</p></div></header>
      <TiersPanel />
      <NamespacesPanel />
      <div className="split-grid"><MediaPanel /><LifecyclePanel /></div>
    </div>
  )
}

export function TiersPanel() {
  const { t } = useTranslation()
  const status = useStorage()
  const workspace = status.data?.tiers.find((tier) => tier.updateWorkspace)?.updateWorkspace
  return (
    <Card>
      <CardHeader title={t('storage.tiers.title')} description={t('storage.tiers.description')} action={<Layers className="size-5 text-muted-foreground" />} />
      {status.isPending ? <p className="callout" role="status">{t('storage.tiers.loading')}</p> : null}
      <dl className="details">
        {status.data?.tiers.map((tier) => (
          <div key={tier.name}>
            <dt>{tier.name}</dt>
            <dd>{tierSummary(tier, t)}</dd>
          </div>
        ))}
      </dl>
      {workspace ? (
        <>
          <h3 className="text-sm font-semibold">{t('storage.workspace.title')}</h3>
          <p className="mt-1 text-sm leading-6 text-muted-foreground">{t('storage.workspace.description')}</p>
          <div className="service-state">
            <Status ok={workspace.available}>
              {workspace.available
                ? t('storage.workspace.available', { size: formatBytes(workspace.reservedBytes) })
                : t('storage.workspace.exhausted', { size: formatBytes(workspace.reservedBytes) })}
            </Status>
          </div>
        </>
      ) : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

export function NamespacesPanel() {
  const { t } = useTranslation()
  const status = useStorage()
  const namespaces = status.data?.namespaces
  return (
    <Card>
      <CardHeader title={t('storage.namespaces.title')} description={t('storage.namespaces.description')} action={<FolderTree className="size-5 text-muted-foreground" />} />
      <dl className="details">
        {namespaces?.binds.map((bind) => (
          <div key={bind.name}>
            <dt>{bind.mount}</dt>
            <dd>{bindSummary(bind, t)}</dd>
          </div>
        ))}
      </dl>
      {/* One pool, two views. Said in the UI as well as the API, because a
          reader looking at two mounts will otherwise assume two capacities. */}
      {namespaces ? <p className="callout" role="note">{t('storage.namespaces.sharedPool', { tier: namespaces.sharedCapacityTier })}</p> : null}
    </Card>
  )
}

export function MediaPanel() {
  const { t } = useTranslation()
  const status = useStorage()
  return (
    <Card>
      <CardHeader title={t('storage.media.title')} description={t('storage.media.description')} action={<HardDrive className="size-5 text-muted-foreground" />} />
      <dl className="details">
        {status.data?.media.map((medium) => (
          <div key={medium.name}>
            <dt>{medium.name}{medium.sizeBytes ? ` · ${formatBytes(medium.sizeBytes)}` : ''}</dt>
            <dd>{mediumHealth(medium, t)}</dd>
          </div>
        ))}
      </dl>
      {status.data && status.data.media.length === 0 ? <p className="callout warning" role="status">{t('storage.media.none')}</p> : null}
    </Card>
  )
}

export function LifecyclePanel() {
  const { t } = useTranslation()
  const status = useStorage()
  return (
    <Card>
      <CardHeader title={t('storage.lifecycle.title')} description={t('storage.lifecycle.description')} action={<ShieldQuestion className="size-5 text-muted-foreground" />} />
      <dl className="details">
        {Object.entries(status.data?.lifecycle ?? {}).map(([name, decision]) => (
          <div key={name}>
            <dt>{t(`storage.lifecycle.${name}`, { defaultValue: name })}</dt>
            <dd>{decision === 'unsupported' ? t('storage.lifecycle.unsupported') : t('storage.lifecycle.supported')}</dd>
          </div>
        ))}
      </dl>
    </Card>
  )
}

/// One tier in one line: what it is, where it is, how full it is, and what the
/// last check recorded. An absent tier says so rather than showing blanks.
function tierSummary(tier: StorageTier, t: ReturnType<typeof useTranslation>['t']) {
  if (!tier.present) return t('storage.tiers.absent')
  const parts: string[] = []
  if (tier.mounted && tier.mount) {
    parts.push(tier.readOnly ? `${tier.mount} (${t('storage.tiers.readOnly')})` : tier.mount)
  } else {
    parts.push(t('storage.tiers.unmounted'))
  }
  if (tier.space) {
    parts.push(t('storage.tiers.used', { used: formatBytes(tier.space.usedBytes), total: formatBytes(tier.space.totalBytes) }))
    if (tier.space.reservedBytes > 0) {
      parts.push(t('storage.tiers.reserved', { reserved: formatBytes(tier.space.reservedBytes) }))
    }
  } else if (tier.partitionBytes) {
    parts.push(formatBytes(tier.partitionBytes))
  }
  if (tier.pressure) parts.push(t(`storage.pressure.${tier.pressure}`))
  parts.push(checkSummary(tier, t))
  return parts.join(' · ')
}

/// fsck exit 1 means errors were CORRECTED, which is the one outcome an
/// operator has to see; it gets its own sentence rather than a number.
function checkSummary(tier: StorageTier, t: ReturnType<typeof useTranslation>['t']) {
  if (tier.check.recorded === false || !tier.check.unit) return t('storage.tiers.neverChecked')
  if (tier.check.exitStatus === 1) return t('storage.tiers.checkedCorrected')
  return t('storage.tiers.checked', {
    result: tier.check.result ?? t('common.states.unknown'),
    status: tier.check.exitStatus ?? t('common.states.unknown'),
  })
}

/// One bind namespace in one line: who owns it, whether it is really backed
/// by DATA, and what the readiness probe did. A bind that is not on DATA is a
/// named error, never "ok" — a writer must not fall back to another
/// filesystem, and the operator has to be able to see that state.
function bindSummary(bind: StorageBind, t: ReturnType<typeof useTranslation>['t']) {
  const parts = [t(`storage.namespaces.owner.${bind.owner}`, { defaultValue: bind.owner })]
  parts.push(t(`storage.namespaces.readiness.${bind.readiness}`))
  if (bind.mounted && bind.sourceOnData === false) {
    parts.push(t('storage.namespaces.notOnData', { source: bind.source }))
  }
  if (bind.sourceIsDirectory === false) {
    parts.push(t('storage.namespaces.sourceNotDirectory', { source: bind.source }))
  }
  if (bind.readOnly) parts.push(t('storage.tiers.readOnly'))
  if (bind.probe) {
    if (!bind.probe.attempted) parts.push(t('storage.namespaces.probeSkipped', { reason: bind.probe.reason }))
    else if (bind.probe.passed) parts.push(t('storage.namespaces.probePassed'))
    else parts.push(t('storage.namespaces.probeFailed', { error: bind.probe.error }))
  }
  return parts.join(' · ')
}

/// Wear, or the reason there is none. An unsupported metric is stated, never
/// left blank: a medium nobody can read must not look healthy.
function mediumHealth(medium: StorageMedium, t: ReturnType<typeof useTranslation>['t']) {
  if (!medium.health.supported) return t('storage.media.unsupported', { reason: medium.health.reason })
  const estimate = medium.health.lifetimeEstimates.find((entry) => entry.usedPercentMax !== undefined)
  const parts: string[] = []
  if (estimate) {
    parts.push(t('storage.media.lifetime', { min: estimate.usedPercentMin, max: estimate.usedPercentMax }))
  }
  if (medium.health.preEol) parts.push(t('storage.media.preEol', { state: medium.health.preEol }))
  return parts.length > 0 ? parts.join(' · ') : t('storage.media.unsupported', { reason: medium.health.source })
}

/// Binary units, because every number on this page comes from a block device.
function formatBytes(bytes: number) {
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`
}

export const Route = createFileRoute('/storage')({ component: StoragePage })
