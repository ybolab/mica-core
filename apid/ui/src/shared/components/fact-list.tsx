import type { ReactNode } from 'react'
import { Item, ItemContent, ItemGroup, ItemTitle } from '@/shared/components/ui/item'
import { cn } from '@/shared/lib/utils'

export interface Fact {
  id: string
  label: ReactNode
  value: ReactNode
  mono?: boolean
}

/// Label-and-value readings of what the device reported. Four spellings of this
/// existed — `.details`, `.fact-grid`, `.compact-details`, `.review-list` — and
/// they disagreed about alignment, wrapping and whether a long identifier could
/// break.
/// Falsy entries are dropped, so a caller can write `value && { ... }` against
/// an optional field without first converting it to a boolean.
export function FactList({ facts, className }: { facts: (Fact | undefined | false | '' | null)[]; className?: string }) {
  const present = facts.filter((fact): fact is Fact => Boolean(fact))
  if (present.length === 0) return null
  return (
    <ItemGroup className={cn('divide-y', className)}>
      {present.map((fact) => (
        <Item key={fact.id} size="xs" className="justify-between gap-6 rounded-none px-0">
          <ItemContent className="flex-none basis-auto">
            <ItemTitle className="font-normal text-muted-foreground">{fact.label}</ItemTitle>
          </ItemContent>
          <div className={cn('min-w-0 text-right text-sm break-words', fact.mono && 'font-mono text-[0.8125rem]')}>
            {fact.value}
          </div>
        </Item>
      ))}
    </ItemGroup>
  )
}
