import type { ReactNode } from 'react'
import { ToggleGroup, ToggleGroupItem } from '@/shared/components/ui/toggle-group'

export interface Segment<Value extends string> {
  value: Value
  label: string
  icon?: ReactNode
  /// Dropped where only the icons fit, as on the sign-in screen.
  labelHidden?: boolean
}

/// An exclusive choice between two or three options, shown in full rather than
/// behind a select. Replaces the two hand-rolled `role="radiogroup"` widgets,
/// which had no arrow-key navigation between their buttons.
export function SegmentedControl<Value extends string>({ value, onValueChange, segments, label }: {
  value: Value
  onValueChange: (value: Value) => void
  segments: Segment<Value>[]
  label: string
}) {
  return (
    <ToggleGroup
      aria-label={label}
      value={[value]}
      onValueChange={(next) => {
        // Single-select: pressing the pressed item yields an empty array, which
        // is not a choice the operator can act on, so the current value stands.
        // Resolving through `segments` also narrows the primitive's `string`
        // back to `Value` without a cast.
        const chosen = segments.find((segment) => segment.value === next[0])
        if (chosen) onValueChange(chosen.value)
      }}
    >
      {segments.map((segment) => (
        <ToggleGroupItem
          key={segment.value}
          value={segment.value}
          aria-label={segment.label}
          title={segment.label}
        >
          {segment.icon}
          {segment.labelHidden ? null : segment.label}
        </ToggleGroupItem>
      ))}
    </ToggleGroup>
  )
}
