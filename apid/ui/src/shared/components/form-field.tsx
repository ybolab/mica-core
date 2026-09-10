import { useId, type ReactNode } from 'react'
import { Field, FieldDescription, FieldLabel } from '@/shared/components/ui/field'

/// A labelled control with its hint. The label is bound by id rather than by
/// wrapping, so a control that renders its own interactive element — a select
/// trigger, a switch — is still named without nesting one control inside a
/// label.
export function FormField({ label, hint, children, className, htmlFor }: {
  label: string
  hint?: ReactNode
  children: (id: string) => ReactNode
  className?: string
  htmlFor?: string
}) {
  const generated = useId()
  const id = htmlFor ?? generated
  return (
    <Field className={className}>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      {children(id)}
      {hint ? <FieldDescription>{hint}</FieldDescription> : null}
    </Field>
  )
}

/// A switch with its explanation, boxed. Replaces `.ui-selector` and the switch
/// half of `.panel-row`.
export function ToggleField({ title, description, control }: {
  title: ReactNode
  description?: ReactNode
  control: ReactNode
}) {
  return (
    <Field orientation="horizontal" className="rounded-lg border p-3">
      <div className="flex min-w-0 flex-col gap-0.5">
        <span className="text-sm font-medium">{title}</span>
        {description ? <span className="text-sm text-muted-foreground">{description}</span> : null}
      </div>
      {control}
    </Field>
  )
}
