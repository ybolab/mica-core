import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Undo2 } from 'lucide-react'
import { ApiError, api } from '@/shared/lib/http'
import { Button } from '@/shared/components/ui/button'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { Panel } from '@/shared/components/panel'
import { StatusDot } from '@/shared/components/status-badge'
import { failureDetail } from '@/shared/feedback/toast'

/// The `rollback` object of the update state document. `permitted` is derived
/// from the absence of a reason, so "permitted with a reason" is not a state
/// this pane can be handed and not a state it renders.
interface RollbackStateDoc {
  rollback?: {
    target?: string | null
    permitted?: boolean
    reason?: string | null
  }
}

/// `POST /api/v1/update/rollback`, 200.
interface RollbackResponse {
  deploymentId: string
  target: string
  nextStep: string
}

export function RollbackPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  // The same query key the update pane reads: micad derives the eligibility
  // once, and this pane must not ask a second time and get a second answer.
  const status = useQuery({ queryKey: ['update-state'], queryFn: () => api<RollbackStateDoc>('/api/v1/update') })
  const rollback = useMutation({
    mutationFn: () => api<RollbackResponse>('/api/v1/update/rollback', { method: 'POST' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['update-state'] }),
  })
  const decision = status.data?.rollback
  const permitted = decision?.permitted === true && !!decision.target
  const target = decision?.target ?? undefined
  return (
    <Panel title={t('system.update.rollback.title')} description={t('system.update.rollback.description')} action={<Undo2 className="size-5 text-muted-foreground" />}>
      <div className="flex flex-wrap items-center justify-between gap-4">
        <StatusDot state={status.isPending ? 'pending' : permitted ? 'ok' : 'warning'}>
          {status.isPending
            ? t('system.update.rollback.checking')
            : permitted
              ? t('system.update.rollback.permitted', { target })
              : t('system.update.rollback.refused')}
        </StatusDot>
        {permitted ? (
          <ConfirmDialog
            trigger={<Button variant="destructive" size="sm">{t('system.update.rollback.action', { target })}</Button>}
            title={t('system.update.rollback.action', { target })}
            description={t('system.update.rollback.confirm', { target })}
            confirmLabel={t('system.update.rollback.action', { target })}
            success={t('system.update.rollback.accepted')}
            failure={t('system.update.rollback.action', { target })}
            onConfirm={() => rollback.mutateAsync()}
          />
        ) : null}
      </div>
      {decision && !permitted ? <Callout tone="warning" title={refusalMessage(decision.reason, t)} /> : null}
      {/* The accepted rollback stays on the page: it names the deployment that
          will boot next and is still true after a reload, unlike the toast that
          reported the click. */}
      {rollback.data ? (
        <Callout tone="success" title={t('system.update.rollback.rejected', { deploymentId: rollback.data.deploymentId, target: rollback.data.target })}>
          {t('system.update.rollback.rebootToApply')}
        </Callout>
      ) : null}
      {status.error ? <Callout tone="danger" title={failureDetail(status.error, t('common.requestFailed'))} /> : null}
      {rollback.error ? <Callout tone="danger" title={rollbackErrorMessage(rollback.error, t)} /> : null}
    </Panel>
  )
}

/// The refusal vocabulary is one set, shared by the document's `reason` and by
/// the 409's code, so one mapping answers both. An unknown verdict degrades to
/// the generic sentence rather than being reported as one of the known ones.
function refusalMessage(reason: string | null | undefined, t: ReturnType<typeof useTranslation>['t']) {
  switch (reason) {
    case 'candidate_pending': return t('system.update.rollback.reasons.candidatePending')
    case 'running_not_confirmed': return t('system.update.rollback.reasons.runningNotConfirmed')
    case 'no_usable_fallback': return t('system.update.rollback.reasons.noUsableFallback')
    default: return t('system.update.rollback.reasons.refused')
  }
}

function rollbackErrorMessage(error: unknown, t: ReturnType<typeof useTranslation>['t']) {
  if (error instanceof ApiError && error.status === 409) return refusalMessage(error.code, t)
  return failureDetail(error, t('common.requestFailed'))
}
