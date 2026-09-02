import type { SimulatedApp } from '@/shared/simulation/simulation-provider'

export interface AppFilter {
  query: string
  source: 'all' | SimulatedApp['source']
  kind: 'all' | SimulatedApp['kind']
}

export function filterApps(apps: readonly SimulatedApp[], filter: AppFilter): SimulatedApp[] {
  const needle = filter.query.trim().toLowerCase()
  return apps.filter((app) => app.runtime !== 'not-installed'
    && (!needle || app.name.toLowerCase().includes(needle))
    && (filter.source === 'all' || app.source === filter.source)
    && (filter.kind === 'all' || app.kind === filter.kind))
}

/// How many installed applications still hold data of their own. Removal keeps
/// application data, so this is the number the header counts.
export function retainedCount(apps: readonly SimulatedApp[]): number {
  return apps.filter((app) => app.health === 'retained').length
}
