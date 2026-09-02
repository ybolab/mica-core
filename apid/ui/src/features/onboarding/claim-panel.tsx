import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ShieldCheck } from 'lucide-react'
import { api, errorMessage } from '@/lib/api'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import { formatDeviceClock } from '@/shared/components/fact'

/// `GET /api/v1/claim`. `via` and `at` are absent while the device is
/// unclaimed, and `at` is also absent for a claim by a document that predates
/// any recorded import.
export interface ClaimStatus {
  state: string
  via?: string | null
  at?: number | null
  rotationRequired: boolean
}

/// ONE query for the claim, shared by this pane and by the shell notice that
/// links to it, so the banner and the explanation cannot disagree.
export const claimQuery = {
  queryKey: ['claim'] as const,
  queryFn: () => api<ClaimStatus>('/api/v1/claim'),
}

export function ClaimPanel() {
  const { t, i18n: activeI18n } = useTranslation()
  const claim = useQuery(claimQuery)
  const value = claim.data
  const claimed = value?.state === 'claimed'
  return (
    <Card>
      <CardHeader title={t('access.claim.title')} description={t('access.claim.description')} action={<ShieldCheck className="size-5 text-muted-foreground" />} />
      <div className="service-state">
        <Status ok={claimed && !claim.isError}>
          {claim.isPending ? t('access.claim.checking') : t(claimed ? 'access.claim.claimed' : 'access.claim.unclaimed')}
        </Status>
      </div>
      {value?.via || value?.at ? (
        <dl className="details">
          {value.via ? <div><dt>{t('access.claim.via')}</dt><dd>{t(value.via === 'setup' ? 'access.claim.viaSetup' : 'access.claim.viaDocument')}</dd></div> : null}
          {value.at ? <div><dt>{t('access.claim.at')}</dt><dd>{t('common.deviceClock', { time: formatDeviceClock(value.at, activeI18n.resolvedLanguage ?? 'en') })}</dd></div> : null}
        </dl>
      ) : null}
      {value?.rotationRequired ? (
        <div className="callout warning grid gap-2" role="status">
          <strong>{t('access.claim.rotation.title')}</strong>
          <p>{t('access.claim.rotation.what')}</p>
          <p>{t('access.claim.rotation.effect')}</p>
          <p>{t('access.claim.rotation.wayOut')}</p>
          <p>{t('access.claim.rotation.noCountdown')}</p>
        </div>
      ) : null}
      {claim.error ? <p className="callout error" role="alert">{errorMessage(claim.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}
