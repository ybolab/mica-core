import { useState } from 'react'
import { useTranslation } from 'react-i18next'
import { Check, Copy } from 'lucide-react'
import { InputGroup, InputGroupAddon, InputGroupButton } from '@/shared/components/ui/input-group'
import { notifyFailure } from '@/shared/feedback/toast'
import { cn } from '@/shared/lib/utils'

/// A value the operator has to carry somewhere else — a bootstrap token, an API
/// token, a public key.
///
/// The copy is awaited. The previous spelling called `writeText` behind a `void`
/// and set "Copied" unconditionally, so a rejected write — the clipboard is
/// permission-gated and refuses on an unfocused document — reported success and
/// the operator pasted nothing.
export function CopyField({ value, label, className }: { value: string; label: string; className?: string }) {
  const { t } = useTranslation()
  const [copied, setCopied] = useState(false)

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 2_000)
    } catch (error) {
      notifyFailure(t('common.copyFailed'), error instanceof Error ? error.message : undefined)
    }
  }

  return (
    <InputGroup className={cn('h-auto', className)}>
      <code className="min-w-0 flex-1 px-3 py-2 font-mono text-[0.8125rem] break-all">{value}</code>
      <InputGroupAddon align="inline-end">
        <InputGroupButton aria-label={label} onClick={() => void copy()}>
          {copied ? <Check /> : <Copy />}
          {copied ? t('access.tokens.copied') : t('common.actions.copy')}
        </InputGroupButton>
      </InputGroupAddon>
    </InputGroup>
  )
}
