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

interface ConfiguredSummaryLabels {
  notConfigured: string
  physical: string
  dhcp: string
  static: string
  noAddressing: string
  format: (kind: string, method: string) => string
}

const englishSummaryLabels: ConfiguredSummaryLabels = {
  notConfigured: 'Not configured',
  physical: 'physical',
  dhcp: 'DHCP',
  static: 'Static',
  noAddressing: 'No addressing',
  format: (kind, method) => `${kind} · ${method}`,
}

export function configuredSummary(
  configured?: Record<string, unknown>,
  labels: ConfiguredSummaryLabels = englishSummaryLabels,
) {
  if (!configured) return labels.notConfigured
  const kind = typeof configured.kind === 'string' ? configured.kind : labels.physical
  const method = configured.dhcp === true
    ? labels.dhcp
    : configured.static && typeof configured.static === 'object'
      ? labels.static
      : labels.noAddressing
  return labels.format(kind, method)
}
