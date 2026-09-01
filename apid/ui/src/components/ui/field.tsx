import type { InputHTMLAttributes, ReactNode } from 'react'
import { cn } from '@/lib/utils'

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="grid gap-2 text-sm font-medium">
      <span>{label}</span>
      {children}
      {hint ? <span className="text-xs font-normal text-muted-foreground">{hint}</span> : null}
    </label>
  )
}

export function Input({ className, ...props }: InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        'h-11 w-full rounded-lg border border-input bg-surface px-3 text-sm outline-none transition placeholder:text-muted-foreground/70 focus:border-ring focus:ring-3 focus:ring-ring/25',
        className,
      )}
      {...props}
    />
  )
}
