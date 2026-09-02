import type { ReactNode } from 'react'
import { Label } from '@/shared/components/ui/label'
import { cn } from '@/shared/lib/utils'

export function Field({ label, hint, children, className }: { label: string; hint?: string; children: ReactNode; className?: string }) {
  return (
    <Label className={cn('field', className)}>
      <span>{label}</span>
      {children}
      {hint ? <span className="field-hint">{hint}</span> : null}
    </Label>
  )
}
