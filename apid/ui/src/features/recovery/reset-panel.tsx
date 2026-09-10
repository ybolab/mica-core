import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ApiError, api, json } from '@/shared/lib/http'
import { Button } from '@/shared/components/ui/button'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { Panel } from '@/shared/components/panel'
import { RowItem, RowList } from '@/shared/components/row-item'
import { failureDetail } from '@/shared/feedback/toast'
import { formatDeviceClock } from '@/shared/components/fact'

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
  // The dialog owns the report and the close; this mutation exists for the
  // cache invalidation, so it deliberately does not raise its own toast.
  const stage = useMutation({
    mutationFn: (tier: ReachableTier) => api<ResetStaged>('/api/v1/reset', json('POST', { tier })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'reset'] }),
  })
  // A 404 is the answer "nothing is staged", not a failure to read.
  const nothingStaged = staged.error instanceof ApiError && staged.error.status === 404
  const record = staged.data
  return (
    <Panel>
      {record ? (
        <Callout tone="warning" title={t('system.recovery.reset.pending', { tier: tierLabel(record.tier, t) })}>
          {record.requested ? `${t('system.recovery.reset.pendingAt', { time: formatDeviceClock(record.requested, activeI18n.resolvedLanguage ?? 'en') })} ` : ''}
          {t('system.recovery.reset.replaces')}
        </Callout>
      ) : null}
      <RowList className="rounded-lg border">
        {REACHABLE_TIERS.map((tier) => (
          <RowItem
            key={tier}
            title={tierLabel(tier, t)}
            description={t(`system.recovery.reset.tiers.${tier}.effect`)}
            actions={(
              <ConfirmDialog
                trigger={<Button variant="destructive" size="sm">{t('system.recovery.reset.stage')}</Button>}
                title={tierLabel(tier, t)}
                description={t('system.recovery.reset.confirm', { effect: t(`system.recovery.reset.tiers.${tier}.effect`) })}
                confirmLabel={t('system.recovery.reset.stage')}
                success={`${t('system.recovery.reset.staged', { tier: tierLabel(tier, t) })} ${t('system.recovery.reset.rebootToApply')}`}
                failure={t('system.recovery.reset.stage')}
                onConfirm={() => stage.mutateAsync(tier)}
              />
            )}
          />
        ))}
        <RowItem
          title={tierLabel('full-factory', t)}
          description={t('system.recovery.reset.tiers.full-factory.effect')}
          actions={<span className="text-xs text-muted-foreground">{t('system.recovery.reset.presenceShort')}</span>}
        />
      </RowList>
      <Callout tone="warning" title={t('system.recovery.reset.presenceUnavailable')} />
      <Callout title={t('system.recovery.reset.noSecureWipe')} />
      {staged.error && !nothingStaged ? <Callout tone="danger" title={failureDetail(staged.error, t('common.requestFailed'))} /> : null}
    </Panel>
  )
}

function tierLabel(tier: string, t: ReturnType<typeof useTranslation>['t']) {
  if (tier === 'configuration') return t('system.recovery.reset.tiers.configuration.name')
  if (tier === 'application-data') return t('system.recovery.reset.tiers.application-data.name')
  if (tier === 'full-factory') return t('system.recovery.reset.tiers.full-factory.name')
  return tier
}
