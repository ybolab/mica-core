import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ApiError, api, errorMessage, json } from '@/lib/api'
import { Button } from '@/shared/components/ui/button'
import { Card } from '@/components/ui/card'
import { formatDeviceClock } from '@/shared/components/fact'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/shared/components/ui/alert-dialog'

/// The tiers an authenticated management session may stage. Tier 3
/// (`full-factory`) is listed beside them and deliberately carries no control:
/// it is gated on a physical-presence assertion, and nothing in this build
/// writes one, so a button here would promise an operation that always fails.
const REACHABLE_TIERS = ['configuration', 'application-data'] as const

type ReachableTier = (typeof REACHABLE_TIERS)[number]

/// `access.reset` in the settings tree: the intent record a staged tier leaves
/// behind. The path is absent — 404 — while no reset is staged.
interface StagedReset {
  tier: string
  requested?: number
  presence?: string
}

/// `POST /api/v1/reset`, 202. `applies` is always `next-boot`.
interface ResetStaged {
  tier: string
  applies: string
}

export function ResetPanel() {
  const { t, i18n: activeI18n } = useTranslation()
  const queryClient = useQueryClient()
  const staged = useQuery({
    queryKey: ['settings', 'reset'],
    queryFn: () => api<StagedReset>('/api/v1/settings/reset'),
    retry: false,
  })
  const stage = useMutation({
    mutationFn: (tier: ReachableTier) => api<ResetStaged>('/api/v1/reset', json('POST', { tier })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'reset'] }),
  })
  // A 404 is the answer "nothing is staged", not a failure to read.
  const nothingStaged = staged.error instanceof ApiError && staged.error.status === 404
  const record = staged.data
  return (
    <Card>
      {record ? (
        <p className="callout warning" role="status">
          {t('system.recovery.reset.pending', { tier: tierLabel(record.tier, t) })}
          {record.requested ? ` ${t('system.recovery.reset.pendingAt', { time: formatDeviceClock(record.requested, activeI18n.resolvedLanguage ?? 'en') })}` : ''}
          {` ${t('system.recovery.reset.replaces')}`}
        </p>
      ) : null}
      <div className="collection-list">
        {REACHABLE_TIERS.map((tier) => (
          <div className="collection-row" key={tier}>
            <div><strong>{tierLabel(tier, t)}</strong><small>{t(`system.recovery.reset.tiers.${tier}.effect`)}</small></div>
            <AlertDialog>
              <AlertDialogTrigger render={<Button variant="destructive" size="sm" disabled={stage.isPending} />}>{t('system.recovery.reset.stage')}</AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader>
                  <AlertDialogTitle>{tierLabel(tier, t)}</AlertDialogTitle>
                  <AlertDialogDescription>{t('system.recovery.reset.confirm', { effect: t(`system.recovery.reset.tiers.${tier}.effect`) })}</AlertDialogDescription>
                </AlertDialogHeader>
                <AlertDialogFooter>
                  <AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel>
                  <AlertDialogAction variant="destructive" onClick={() => stage.mutate(tier)}>{t('system.recovery.reset.stage')}</AlertDialogAction>
                </AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          </div>
        ))}
        <div className="collection-row">
          <div><strong>{tierLabel('full-factory', t)}</strong><small>{t('system.recovery.reset.tiers.full-factory.effect')}</small></div>
          <span className="text-xs text-muted-foreground">{t('system.recovery.reset.presenceShort')}</span>
        </div>
      </div>
      <p className="callout warning" role="status">{t('system.recovery.reset.presenceUnavailable')}</p>
      <p className="callout" role="status">{t('system.recovery.reset.noSecureWipe')}</p>
      {stage.data ? (
        <p className="callout success" role="status">
          {t('system.recovery.reset.staged', { tier: tierLabel(stage.data.tier, t) })} {t('system.recovery.reset.rebootToApply')}
        </p>
      ) : null}
      {stage.error ? <p className="callout error" role="alert">{errorMessage(stage.error, t('common.requestFailed'))}</p> : null}
      {staged.error && !nothingStaged ? <p className="callout error" role="alert">{errorMessage(staged.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function tierLabel(tier: string, t: ReturnType<typeof useTranslation>['t']) {
  if (tier === 'configuration') return t('system.recovery.reset.tiers.configuration.name')
  if (tier === 'application-data') return t('system.recovery.reset.tiers.application-data.name')
  if (tier === 'full-factory') return t('system.recovery.reset.tiers.full-factory.name')
  return tier
}
