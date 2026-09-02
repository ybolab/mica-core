import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Undo2 } from 'lucide-react'
import { ApiError, api, errorMessage } from '@/lib/api'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
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
  slotName: string
  message: string
  target: string
  nextStep: string
}

export function RollbackPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  // The same query key the update pane reads: mosd derives the eligibility
  // once, and this pane must not ask a second time and get a second answer.
  const status = useQuery({ queryKey: ['update-state'], queryFn: () => api<RollbackStateDoc>('/api/v1/update') })
  const rollback = useMutation({
    mutationFn: () => api<RollbackResponse>('/api/v1/update/rollback', { method: 'POST' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['update-state'] }),
  })
  const decision = status.data?.rollback
  const permitted = decision?.permitted === true
  const target = decision?.target ?? undefined
  return (
    <Card>
      <CardHeader title={t('system.update.rollback.title')} description={t('system.update.rollback.description')} action={<Undo2 className="size-5 text-muted-foreground" />} />
      <div className="service-state">
        <Status ok={permitted}>
          {status.isPending
            ? t('system.update.rollback.checking')
            : permitted
              ? t('system.update.rollback.permitted', { target })
              : t('system.update.rollback.refused')}
        </Status>
        {permitted ? (
          <AlertDialog>
            <AlertDialogTrigger render={<Button variant="destructive" size="sm" disabled={rollback.isPending} />}>{t('system.update.rollback.action', { target })}</AlertDialogTrigger>
            <AlertDialogContent>
              <AlertDialogHeader>
                <AlertDialogTitle>{t('system.update.rollback.action', { target })}</AlertDialogTitle>
                <AlertDialogDescription>{t('system.update.rollback.confirm', { target })}</AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel>
                <AlertDialogAction variant="destructive" onClick={() => rollback.mutate()}>{t('system.update.rollback.action', { target })}</AlertDialogAction>
              </AlertDialogFooter>
            </AlertDialogContent>
          </AlertDialog>
        ) : null}
      </div>
      {decision && !permitted ? <p className="callout warning" role="status">{refusalMessage(decision.reason, t)}</p> : null}
      {rollback.data ? (
        <div className="callout success grid gap-2" role="status">
          <p>{t('system.update.rollback.marked', { slot: rollback.data.slotName, target: rollback.data.target })}</p>
          <p>{t('system.update.rollback.rebootToApply')}</p>
          <p>{rollback.data.message}</p>
        </div>
      ) : null}
      {status.error ? <p className="callout error" role="alert">{errorMessage(status.error, t('common.requestFailed'))}</p> : null}
      {rollback.error ? <p className="callout error" role="alert">{rollbackErrorMessage(rollback.error, t)}</p> : null}
    </Card>
  )
}

/// The refusal vocabulary is one set, shared by the document's `reason` and by
/// the 409's code, so one mapping answers both. An unknown verdict degrades to
/// the generic sentence rather than being reported as one of the known ones.
function refusalMessage(reason: string | null | undefined, t: ReturnType<typeof useTranslation>['t']) {
  switch (reason) {
    case 'no_alternate_slot': return t('system.update.rollback.reasons.noAlternateSlot')
    case 'alternate_is_booted_slot': return t('system.update.rollback.reasons.alternateIsBootedSlot')
    case 'alternate_never_installed': return t('system.update.rollback.reasons.alternateNeverInstalled')
    case 'alternate_marked_bad': return t('system.update.rollback.reasons.alternateMarkedBad')
    case 'alternate_is_newer': return t('system.update.rollback.reasons.alternateIsNewer')
    case 'install_order_unknown': return t('system.update.rollback.reasons.installOrderUnknown')
    case 'booted_slot_not_confirmed': return t('system.update.rollback.reasons.bootedSlotNotConfirmed')
    default: return t('system.update.rollback.reasons.refused')
  }
}

function rollbackErrorMessage(error: unknown, t: ReturnType<typeof useTranslation>['t']) {
  if (error instanceof ApiError && error.status === 409) return refusalMessage(error.code, t)
  return errorMessage(error, t('common.requestFailed'))
}
