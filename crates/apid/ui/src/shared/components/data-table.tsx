import type { ReactNode } from 'react'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/shared/components/ui/empty'
import { Skeleton } from '@/shared/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/shared/components/ui/table'
import { cn } from '@/shared/lib/utils'

export interface Column<Row> {
  id: string
  header: ReactNode
  cell: (row: Row) => ReactNode
  align?: 'start' | 'end'
  className?: string
}

/// The console's one table, with its loading and empty states attached.
///
/// Eight raw `<table class="data-table">` elements used to carry their own
/// header markup, their own empty paragraph and, in one case, an alignment rule
/// that only existed for the shadcn table it was not using — which is why the
/// UI-manager rows ran their version into their size.
export function DataTable<Row>({ columns, rows, rowKey, isPending, empty, emptyIcon, emptyDescription, rowHref }: {
  columns: Column<Row>[]
  rows: Row[] | undefined
  rowKey: (row: Row) => string
  isPending?: boolean
  empty: string
  emptyIcon?: ReactNode
  emptyDescription?: string
  /// Renders the whole row as a link target. The row itself stays a plain row:
  /// the anchor lives in the first cell, so there is one focus stop and one
  /// navigation, not a row handler racing a nested link.
  rowHref?: (row: Row) => ReactNode
}) {
  if (isPending) {
    return (
      <div className="flex flex-col gap-3 p-4" aria-busy="true">
        {[0, 1, 2].map((row) => (
          <div className="flex items-center gap-3" key={row}>
            <Skeleton className="h-4 w-16" />
            <Skeleton className="h-4 w-24" />
            <Skeleton className="h-4 w-36" />
            <Skeleton className="h-4 flex-1" />
          </div>
        ))}
      </div>
    )
  }
  if (rows && rows.length === 0) {
    return (
      <Empty>
        <EmptyHeader>
          {emptyIcon ? <EmptyMedia variant="icon">{emptyIcon}</EmptyMedia> : null}
          <EmptyTitle>{empty}</EmptyTitle>
          {emptyDescription ? <EmptyDescription>{emptyDescription}</EmptyDescription> : null}
        </EmptyHeader>
      </Empty>
    )
  }
  return (
    <div className="w-full overflow-x-auto">
      <Table>
        <TableHeader>
          <TableRow>
            {columns.map((column) => (
              <TableHead key={column.id} className={cn(column.align === 'end' && 'text-right', column.className)}>
                {column.header}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows?.map((row) => (
            <TableRow key={rowKey(row)}>
              {columns.map((column, index) => (
                <TableCell key={column.id} className={cn(column.align === 'end' && 'text-right', column.className)}>
                  {index === 0 && rowHref ? rowHref(row) : column.cell(row)}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}
