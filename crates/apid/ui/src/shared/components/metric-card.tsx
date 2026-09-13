import type { ReactNode } from 'react'
import { Card, CardContent } from '@/shared/components/ui/card'
import { cn } from '@/shared/lib/utils'

/// A single headline reading with its caption. Rendered as a link where the
/// reading has a page behind it.
export function MetricCard({ label, value, caption, mono, render, className }: {
  label: ReactNode
  value: ReactNode
  caption?: ReactNode
  mono?: boolean
  /// A wrapper such as a router `Link`; the card becomes its child.
  render?: (children: ReactNode) => ReactNode
  className?: string
}) {
  const body = (
    <Card className={cn('h-full gap-0 transition-colors', render && 'hover:bg-muted', className)}>
      <CardContent className="flex flex-col gap-1.5">
        <span className="text-sm text-muted-foreground">{label}</span>
        <strong className={cn('text-lg font-semibold tracking-tight', mono && 'font-mono')}>{value}</strong>
        {caption ? <span className="text-sm text-muted-foreground">{caption}</span> : null}
      </CardContent>
    </Card>
  )
  return render ? render(body) : body
}
