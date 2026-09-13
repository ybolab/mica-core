import { useState, type ReactElement, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
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
import { Spinner } from '@/shared/components/ui/spinner'
import { failureDetail, notifyFailure, notifySuccess } from '@/shared/feedback/toast'
import type { Tone } from './callout'

/// The console's only confirmation.
///
/// There were eleven of these, hand-built per call site, and ten of them shared
/// the same defect: `AlertDialogAction` is a plain `Button` by design — the
/// registry leaves closing to the caller so an async confirm can stay open
/// while it runs — and ten callers never closed it. Pressing Revoke left the
/// modal up with the row still listed and nothing said, so the operator's only
/// reading was that the click had been lost.
///
/// The behaviour is owned here instead of documented: a caller supplies what to
/// run and what to say, and cannot express the broken variant.
///
/// Resolve closes and reports. Reject keeps the dialog open, because the
/// operator's next move is to read the refusal and decide again, and reports
/// the device's own sentence.
export function ConfirmDialog({
  trigger,
  title,
  description,
  confirmLabel,
  cancelLabel,
  tone = 'danger',
  success,
  failure,
  onConfirm,
  open,
  onOpenChange,
  disabled,
}: {
  /// Omitted when the dialog is driven by `open` from somewhere else, such as a
  /// form that confirms on submit.
  trigger?: ReactElement
  title: string
  description: ReactNode
  confirmLabel: string
  cancelLabel?: string
  tone?: Extract<Tone, 'danger' | 'neutral'>
  success: string
  failure: string
  onConfirm: () => Promise<unknown> | unknown
  open?: boolean
  onOpenChange?: (open: boolean) => void
  disabled?: boolean
}) {
  const { t } = useTranslation()
  const [uncontrolled, setUncontrolled] = useState(false)
  const [pending, setPending] = useState(false)
  const isOpen = open ?? uncontrolled
  const setOpen = (next: boolean) => {
    // A confirmation in flight is not dismissible: closing it would leave the
    // operator with no report of an operation that is already running.
    if (pending && !next) return
    onOpenChange?.(next)
    if (open === undefined) setUncontrolled(next)
  }

  const confirm = async () => {
    setPending(true)
    try {
      await onConfirm()
      notifySuccess(success)
      setPending(false)
      onOpenChange?.(false)
      if (open === undefined) setUncontrolled(false)
    } catch (error) {
      setPending(false)
      notifyFailure(failure, failureDetail(error, t('common.requestFailed')))
    }
  }

  return (
    <AlertDialog open={isOpen} onOpenChange={setOpen}>
      {trigger ? <AlertDialogTrigger disabled={disabled} render={trigger} /> : null}
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{title}</AlertDialogTitle>
          <AlertDialogDescription>{description}</AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel disabled={pending}>{cancelLabel ?? t('common.actions.cancel')}</AlertDialogCancel>
          <AlertDialogAction
            variant={tone === 'danger' ? 'destructive' : 'default'}
            disabled={pending}
            onClick={() => void confirm()}
          >
            {pending ? <Spinner /> : null}
            {confirmLabel}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
