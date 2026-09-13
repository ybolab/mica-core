import type { ReactNode } from 'react'
import { Badge } from '@/shared/components/ui/badge'
import { Spinner } from '@/shared/components/ui/spinner'
import { cn } from '@/shared/lib/utils'
import type { Tone } from './callout'

export type StatusTone = Tone

const variants = {
  neutral: 'outline',
  accent: 'accent',
  success: 'success',
  warning: 'warning',
  danger: 'destructive',
} as const

export function StatusBadge({ tone = 'neutral', children, className }: {
  tone?: StatusTone
  children: ReactNode
  className?: string
}) {
  return <Badge variant={variants[tone]} className={className}>{children}</Badge>
}

/// The dot-and-label reading of a single subsystem. `pending` is a third state
/// beside ok and not-ok: a device that has not answered yet is not unhealthy,
/// and showing it as a warning dot taught operators to ignore the warning.
export function StatusDot({ state, children }: { state: 'ok' | 'warning' | 'pending'; children: ReactNode }) {
  return (
    <span className={cn(
      'inline-flex items-center gap-2 text-sm font-medium',
      state === 'ok' ? 'text-success' : state === 'warning' ? 'text-warning' : 'text-muted-foreground',
    )}>
      {state === 'pending'
        ? <Spinner className="size-3" />
        : <span className={cn('size-2 rounded-full', state === 'ok' ? 'bg-success' : 'bg-warning')} aria-hidden="true" />}
      {children}
    </span>
  )
}
