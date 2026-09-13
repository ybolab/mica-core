import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { FileCog } from 'lucide-react'
import { api } from '@/shared/lib/http'
import { Callout } from '@/shared/components/callout'
import { FactList } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'
import { StatusDot } from '@/shared/components/status-badge'
import { failureDetail } from '@/shared/feedback/toast'
import { formatDeviceClock } from '@/shared/components/fact'

/// The last import ATTEMPT. `outcome` is `applied`, `unchanged` or `rejected`,
/// and `reason` is carried only by a rejection.
interface ProvisioningImport {
  source?: string
  outcome?: string
  reason?: string | null
  at?: number
}

/// `GET /api/v1/provisioning/status`. The applied version and digest are
/// independent of `lastImport`: a rejection leaves them exactly as they were.
export interface ProvisioningStatus {
  documentVersion?: number | null
  documentDigest?: string | null
  lastImport?: ProvisioningImport | null
  unclaimed: boolean
}

export function ProvisioningPanel() {
  const { t, i18n: activeI18n } = useTranslation()
  const status = useQuery({ queryKey: ['provisioning-status'], queryFn: () => api<ProvisioningStatus>('/api/v1/provisioning/status') })
  const value = status.data
  const lastImport = value?.lastImport
  const applied = value?.documentVersion !== undefined && value?.documentVersion !== null
  return (
    <Panel
      title={t('access.provisioning.title')}
      description={t('access.provisioning.description')}
      action={<FileCog className="size-5 text-muted-foreground" />}
    >
      <StatusDot state={status.isPending ? 'pending' : !status.isError && lastImport?.outcome !== 'rejected' ? 'ok' : 'warning'}>
        {status.isPending ? t('access.provisioning.checking') : t(applied ? 'access.provisioning.applied' : 'access.provisioning.none')}
      </StatusDot>
      <FactList facts={[
        applied ? { id: 'version', label: t('access.provisioning.version'), value: value?.documentVersion } : undefined,
        value?.documentDigest ? { id: 'digest', label: t('access.provisioning.digest'), value: value.documentDigest, mono: true } : undefined,
        lastImport?.source ? { id: 'source', label: t('access.provisioning.source'), value: sourceLabel(lastImport.source, t) } : undefined,
        lastImport?.outcome ? { id: 'outcome', label: t('access.provisioning.outcome'), value: outcomeLabel(lastImport.outcome, t) } : undefined,
        lastImport?.at ? { id: 'at', label: t('access.provisioning.importedAt'), value: t('common.deviceClock', { time: formatDeviceClock(lastImport.at, activeI18n.resolvedLanguage ?? 'en') }) } : undefined,
      ]} />
      {value && !lastImport ? <p className="text-sm text-muted-foreground">{t('access.provisioning.neverOffered')}</p> : null}
      {lastImport?.outcome === 'rejected' ? <Callout tone="warning" title={t('access.provisioning.rejected', { reason: lastImport.reason ?? t('common.states.unknown') })} /> : null}
      {value ? <Callout title={t(value.unclaimed ? 'access.provisioning.wouldApply' : 'access.provisioning.refusesDocuments')} /> : null}
      {status.error ? <Callout tone="danger" title={failureDetail(status.error, t('common.requestFailed'))} /> : null}
    </Panel>
  )
}

function sourceLabel(source: string, t: ReturnType<typeof useTranslation>['t']) {
  if (source === 'boot') return t('access.provisioning.sources.boot')
  if (source === 'media') return t('access.provisioning.sources.media')
  return source
}

function outcomeLabel(outcome: string, t: ReturnType<typeof useTranslation>['t']) {
  if (outcome === 'applied') return t('access.provisioning.outcomes.applied')
  if (outcome === 'unchanged') return t('access.provisioning.outcomes.unchanged')
  if (outcome === 'rejected') return t('access.provisioning.outcomes.rejected')
  return outcome
}
