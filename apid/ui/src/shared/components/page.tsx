import type { ReactNode } from 'react'
import { cn } from '@/shared/lib/utils'

export function Page({ children, className }: { children: ReactNode; className?: string }) {
  return <div className={cn('mx-auto flex w-full max-w-[1280px] flex-col gap-6 px-4 py-6 sm:px-6 lg:px-8', className)}>{children}</div>
}

export function PageHeader({ title, description, action, back, media }: {
  title: string
  description?: string
  action?: ReactNode
  back?: ReactNode
  /// An icon tile beside the title, as the service and interface pages carry.
  media?: ReactNode
}) {
  return (
    <header className="flex flex-wrap items-start justify-between gap-4">
      <div className="flex flex-col items-start gap-2">
        {back}
        <div className="flex items-center gap-3">
          {media ? <span className="grid size-11 shrink-0 place-items-center rounded-lg bg-muted [&_svg]:size-5">{media}</span> : null}
          <div className="flex flex-col gap-1">
            <h1 className="text-xl font-semibold tracking-tight">{title}</h1>
            {description ? <p className="max-w-[70ch] text-sm text-muted-foreground">{description}</p> : null}
          </div>
        </div>
      </div>
      {action ? <div className="flex flex-wrap items-center gap-2">{action}</div> : null}
    </header>
  )
}

/// A titled band with its explanation in a column beside the controls on wide
/// screens, stacked on narrow ones.
export function PageSection({ title, description, children, tone = 'default', className }: {
  title: string
  description?: string
  children: ReactNode
  tone?: 'default' | 'danger'
  className?: string
}) {
  return (
    <section className={cn('grid items-start gap-4 lg:grid-cols-[minmax(0,16rem)_minmax(0,1fr)] lg:gap-8', className)}>
      <div className="flex flex-col gap-1">
        <h2 className={cn('text-base font-semibold', tone === 'danger' && 'text-destructive')}>{title}</h2>
        {description ? <p className="text-sm text-muted-foreground">{description}</p> : null}
      </div>
      <div className="grid gap-3">{children}</div>
    </section>
  )
}
