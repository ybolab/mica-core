import type { ObservedInterface } from './types'

export interface NetworkRow {
  name: string
  configured?: Record<string, unknown>
  observed?: ObservedInterface
}

export function networkRows(
  configured: Record<string, unknown> = {},
  observed: ObservedInterface[] = [],
): NetworkRow[] {
  const rows = new Map<string, NetworkRow>()
  for (const [name, value] of Object.entries(configured)) {
    rows.set(name, {
      name,
      configured: value && typeof value === 'object' ? value as Record<string, unknown> : {},
    })
  }
  for (const iface of observed) {
    const name = iface.name ?? `#${iface.index ?? 'unknown'}`
    rows.set(name, { ...rows.get(name), name, observed: iface })
  }
  return [...rows.values()].sort((left, right) => {
    const leftIndex = left.observed?.index ?? Number.MAX_SAFE_INTEGER
    const rightIndex = right.observed?.index ?? Number.MAX_SAFE_INTEGER
    return leftIndex - rightIndex || left.name.localeCompare(right.name)
  })
}

export function configuredSummary(configured?: Record<string, unknown>) {
  if (!configured) return 'Not configured'
  const kind = typeof configured.kind === 'string' ? configured.kind : 'physical'
  const method = configured.dhcp === true
    ? 'DHCP'
    : configured.static && typeof configured.static === 'object'
      ? 'Static'
      : 'No addressing'
  return `${kind} · ${method}`
}
