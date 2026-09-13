import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ShieldCheck } from 'lucide-react'
import { api } from '@/shared/lib/http'
import { Callout } from '@/shared/components/callout'
import { FactList } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'
import { StatusDot } from '@/shared/components/status-badge'
import { failureDetail } from '@/shared/feedback/toast'
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
    <Panel
      title={t('access.claim.title')}
      description={t('access.claim.description')}
      action={<ShieldCheck className="size-5 text-muted-foreground" />}
    >
      <StatusDot state={claim.isPending ? 'pending' : claimed && !claim.isError ? 'ok' : 'warning'}>
        {claim.isPending ? t('access.claim.checking') : t(claimed ? 'access.claim.claimed' : 'access.claim.unclaimed')}
      </StatusDot>
      <FactList facts={[
        value?.via ? { id: 'via', label: t('access.claim.via'), value: t(value.via === 'setup' ? 'access.claim.viaSetup' : 'access.claim.viaDocument') } : undefined,
        value?.at ? { id: 'at', label: t('access.claim.at'), value: t('common.deviceClock', { time: formatDeviceClock(value.at, activeI18n.resolvedLanguage ?? 'en') }) } : undefined,
      ]} />
      {value?.rotationRequired ? (
        <Callout tone="warning" title={t('access.claim.rotation.title')}>
          <span className="grid gap-2">
            <span>{t('access.claim.rotation.what')}</span>
            <span>{t('access.claim.rotation.effect')}</span>
            <span>{t('access.claim.rotation.wayOut')}</span>
            <span>{t('access.claim.rotation.noCountdown')}</span>
          </span>
        </Callout>
      ) : null}
      {claim.error ? <Callout tone="danger" title={failureDetail(claim.error, t('common.requestFailed'))} /> : null}
    </Panel>
  )
}
