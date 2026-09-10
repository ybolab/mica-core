import { useState, type FormEvent, type ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { Button } from '@/shared/components/ui/button'
import {
  Dialog,
  DialogBody,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/shared/components/ui/dialog'
import { Spinner } from '@/shared/components/ui/spinner'
import { failureDetail, notifyFailure, notifySuccess } from '@/shared/feedback/toast'

/// A dialog that submits a form, with the same outcome contract as
/// `ConfirmDialog`: resolve closes and reports, reject stays open and reports
/// the device's sentence so the operator can correct the field that was
/// refused.
///
/// The body scrolls between a pinned header and a pinned footer. That is the
/// whole reason this exists as a composite rather than four hand-assembled
/// dialogs: three of the four used to clip their own buttons off a short
/// screen, each in its own way.
export function FormDialog({
  open,
  onOpenChange,
  title,
  description,
  submitLabel,
  cancelLabel,
  success,
  failure,
  onSubmit,
  children,
  size,
  submitDisabled,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description?: string
  submitLabel: string
  cancelLabel?: string
  success: string
  failure: string
  onSubmit: () => Promise<unknown> | unknown
  children: ReactNode
  size?: 'sm' | 'default' | 'lg'
  submitDisabled?: boolean
}) {
  const { t } = useTranslation()
  const [pending, setPending] = useState(false)

  const submit = async (event: FormEvent) => {
    event.preventDefault()
    setPending(true)
    try {
      await onSubmit()
      notifySuccess(success)
      setPending(false)
      onOpenChange(false)
    } catch (error) {
      setPending(false)
      notifyFailure(failure, failureDetail(error, t('common.requestFailed')))
    }
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!pending || next) onOpenChange(next) }}>
      <DialogContent size={size} showCloseButton={false}>
        <DialogHeader>
          <DialogTitle>{title}</DialogTitle>
          {description ? <DialogDescription>{description}</DialogDescription> : null}
        </DialogHeader>
        <form className="contents" onSubmit={(event) => void submit(event)}>
          <DialogBody className="grid gap-4 py-1">{children}</DialogBody>
          <DialogFooter>
            <Button type="button" variant="outline" disabled={pending} onClick={() => onOpenChange(false)}>
              {cancelLabel ?? t('common.actions.cancel')}
            </Button>
            <Button type="submit" disabled={pending || submitDisabled}>
              {pending ? <Spinner /> : null}
              {submitLabel}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
