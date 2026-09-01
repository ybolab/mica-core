import { cn } from '@/lib/utils'

export function Status({ ok, children }: { ok: boolean; children: React.ReactNode }) {
  return (
    <span className={cn('inline-flex items-center gap-2 text-sm font-medium', ok ? 'text-success' : 'text-warning')}>
      <span className={cn('size-2 rounded-full', ok ? 'bg-success' : 'bg-warning')} aria-hidden="true" />
      {children}
    </span>
  )
}
