import type { HTMLAttributes } from 'react'
import { cn } from '@/shared/lib/utils'

export type StatusTone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger'

export function StatusBadge({ tone = 'neutral', className, ...props }: HTMLAttributes<HTMLSpanElement> & { tone?: StatusTone }) {
  return <span className={cn('status-badge', `status-${tone}`, className)} {...props} />
}
