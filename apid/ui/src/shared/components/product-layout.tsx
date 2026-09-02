import type { HTMLAttributes, ReactNode } from 'react'
import { cn } from '@/shared/lib/utils'

export function Page({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn('page', className)} {...props} />
}

export function PageHeader({ title, description, action }: { title: string; description?: string; action?: ReactNode }) {
  return (
    <header className="page-head">
      <div>
        <h1>{title}</h1>
        {description ? <p>{description}</p> : null}
      </div>
      {action ? <div className="page-actions">{action}</div> : null}
    </header>
  )
}

export function Surface({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return <div className={cn('surface', className)} {...props} />
}

export function Section({ title, description, children, className }: { title: string; description?: string; children: ReactNode; className?: string }) {
  return (
    <section className={cn('section-layout', className)}>
      <div className="section-copy">
        <h2>{title}</h2>
        {description ? <p>{description}</p> : null}
      </div>
      <div className="section-content">{children}</div>
    </section>
  )
}

export function EmptyState({ title, description, action }: { title: string; description?: string; action?: ReactNode }) {
  return (
    <div className="empty-state">
      <strong>{title}</strong>
      {description ? <p>{description}</p> : null}
      {action}
    </div>
  )
}

export function InlineAlert({ tone = 'neutral', title, children }: { tone?: StatusTone; title: string; children?: ReactNode }) {
  return (
    <div className={cn('inline-alert', `inline-alert-${tone}`)} role={tone === 'danger' ? 'alert' : 'status'}>
      <strong>{title}</strong>
      {children ? <p>{children}</p> : null}
    </div>
  )
}

type StatusTone = 'neutral' | 'accent' | 'success' | 'warning' | 'danger'
