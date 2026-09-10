import type { ReactNode } from 'react'
import { CircleAlert, CircleCheck, Info, OctagonX } from 'lucide-react'
import { Alert, AlertDescription, AlertTitle } from '@/shared/components/ui/alert'
import { cn } from '@/shared/lib/utils'

export type Tone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger'

const icons = {
  neutral: Info,
  accent: Info,
  success: CircleCheck,
  warning: CircleAlert,
  danger: OctagonX,
} as const

const variants = {
  neutral: 'default',
  accent: 'default',
  success: 'success',
  warning: 'warning',
  danger: 'destructive',
} as const

/// A condition the device is in, stated where it applies.
///
/// This is deliberately not the channel for the outcome of a click — that is a
/// toast, and it goes away. A callout is for a fact that must still be there
/// after a reload: a staged reset, a credential that has to be rotated, a
/// simulated surface. The two were mixed before, which is why success was
/// reported by 8 inline paragraphs and 30-odd writes reported nothing at all.
export function Callout({ tone = 'neutral', title, children, className }: {
  tone?: Tone
  title?: string
  children?: ReactNode
  className?: string
}) {
  const Icon = icons[tone]
  return (
    <Alert
      variant={variants[tone]}
      role={tone === 'danger' ? 'alert' : 'status'}
      className={cn(tone === 'neutral' || tone === 'accent' ? 'border-border' : undefined, className)}
    >
      <Icon aria-hidden="true" />
      {title ? <AlertTitle>{title}</AlertTitle> : null}
      {children ? <AlertDescription>{children}</AlertDescription> : null}
    </Alert>
  )
}
