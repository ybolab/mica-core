import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { FileCog } from 'lucide-react'
import { api, errorMessage } from '@/lib/api'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
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
    <Card>
      <CardHeader title={t('access.provisioning.title')} description={t('access.provisioning.description')} action={<FileCog className="size-5 text-muted-foreground" />} />
      <div className="service-state">
        <Status ok={!status.isPending && !status.isError && lastImport?.outcome !== 'rejected'}>
          {status.isPending ? t('access.provisioning.checking') : t(applied ? 'access.provisioning.applied' : 'access.provisioning.none')}
        </Status>
      </div>
      {value ? (
        <dl className="details">
          {applied ? <div><dt>{t('access.provisioning.version')}</dt><dd>{value.documentVersion}</dd></div> : null}
          {value.documentDigest ? <div><dt>{t('access.provisioning.digest')}</dt><dd><code>{value.documentDigest}</code></dd></div> : null}
          {lastImport?.source ? <div><dt>{t('access.provisioning.source')}</dt><dd>{sourceLabel(lastImport.source, t)}</dd></div> : null}
          {lastImport?.outcome ? <div><dt>{t('access.provisioning.outcome')}</dt><dd>{outcomeLabel(lastImport.outcome, t)}</dd></div> : null}
          {lastImport?.at ? <div><dt>{t('access.provisioning.importedAt')}</dt><dd>{t('common.deviceClock', { time: formatDeviceClock(lastImport.at, activeI18n.resolvedLanguage ?? 'en') })}</dd></div> : null}
        </dl>
      ) : null}
      {value && !lastImport ? <p className="empty">{t('access.provisioning.neverOffered')}</p> : null}
      {lastImport?.outcome === 'rejected' ? <p className="callout warning" role="status">{t('access.provisioning.rejected', { reason: lastImport.reason ?? t('common.states.unknown') })}</p> : null}
      {value ? <p className="callout" role="status">{t(value.unclaimed ? 'access.provisioning.wouldApply' : 'access.provisioning.refusesDocuments')}</p> : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
    </Card>
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
