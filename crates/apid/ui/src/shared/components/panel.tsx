import type { ReactNode } from 'react'
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from '@/shared/components/ui/card'
import { cn } from '@/shared/lib/utils'

/// The console's one card. Every page used to reach for either a local `Card`
/// that rendered `.surface` or a `Surface` that rendered the same markup, and
/// then style the result inline; both are this.
export function Panel({ title, description, action, children, className, contentClassName }: {
  title?: string
  description?: string
  action?: ReactNode
  children?: ReactNode
  className?: string
  contentClassName?: string
}) {
  return (
    <Card className={className}>
      {title ? (
        <CardHeader>
          <CardTitle><h2>{title}</h2></CardTitle>
          {description ? <CardDescription>{description}</CardDescription> : null}
          {action ? <CardAction>{action}</CardAction> : null}
        </CardHeader>
      ) : null}
      <CardContent className={cn('flex flex-col gap-4', contentClassName)}>{children}</CardContent>
    </Card>
  )
}

/// A card whose content is a full-bleed collection — a table or a row list —
/// so the rows meet the card border instead of floating inside its padding.
export function CollectionPanel({ title, description, action, children, footer, className }: {
  title?: string
  description?: string
  action?: ReactNode
  children?: ReactNode
  footer?: ReactNode
  className?: string
}) {
  return (
    <Card className={cn('overflow-hidden', className)}>
      {title ? (
        <CardHeader className="border-b">
          <CardTitle><h2>{title}</h2></CardTitle>
          {description ? <CardDescription>{description}</CardDescription> : null}
          {action ? <CardAction>{action}</CardAction> : null}
        </CardHeader>
      ) : null}
      <CardContent className="p-0">{children}</CardContent>
      {footer ? <div className="border-t p-3">{footer}</div> : null}
    </Card>
  )
}
