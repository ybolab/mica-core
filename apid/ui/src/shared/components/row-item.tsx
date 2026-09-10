import type { ReactNode } from 'react'
import { Item, ItemActions, ItemContent, ItemDescription, ItemGroup, ItemTitle } from '@/shared/components/ui/item'
import { cn } from '@/shared/lib/utils'

/// One entry in a list of things the operator acts on: a key, a snapshot, a
/// reset tier, an item needing attention. Replaces `.panel-row`,
/// `.collection-row` and `.attention-row`, which differed only in padding.
export function RowItem({ title, description, media, actions, className }: {
  title: ReactNode
  description?: ReactNode
  media?: ReactNode
  actions?: ReactNode
  className?: string
}) {
  return (
    <Item className={cn('rounded-none', className)}>
      {media}
      <ItemContent>
        <ItemTitle>{title}</ItemTitle>
        {description ? <ItemDescription>{description}</ItemDescription> : null}
      </ItemContent>
      {actions ? <ItemActions>{actions}</ItemActions> : null}
    </Item>
  )
}

export function RowList({ children, className }: { children: ReactNode; className?: string }) {
  return <ItemGroup className={cn('divide-y', className)}>{children}</ItemGroup>
}
