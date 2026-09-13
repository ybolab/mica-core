import { describe, expect, it } from 'vitest'
import { filterApps, retainedCount } from './filter'
import type { SimulatedApp } from '@/shared/simulation/simulation-provider'

const apps: SimulatedApp[] = [
  { id: 'a', name: 'Node-RED', version: '4.0.9', source: 'catalog', kind: 'container', runtime: 'running', health: 'healthy', desired: 'enabled' },
  { id: 'b', name: 'System Metrics', version: '2026.08', source: 'system', kind: 'native', runtime: 'running', health: 'healthy', desired: 'enabled' },
  { id: 'c', name: 'Camera Agent', version: '2.1.0', source: 'catalog', kind: 'container', runtime: 'stopped', health: 'retained', desired: 'disabled' },
  { id: 'd', name: 'MQTT Bridge', version: '2.4.0', source: 'catalog', kind: 'container', runtime: 'not-installed', health: 'unknown', desired: 'disabled' },
]

describe('the installed list', () => {
  /// A catalog entry that was never installed is not an installed application,
  /// however much of it the catalog knows.
  it('excludes anything that is not installed', () => {
    expect(filterApps(apps, { query: '', source: 'all', kind: 'all' }).map((app) => app.id)).toEqual(['a', 'b', 'c'])
  })

  it('matches the search against the name, case-insensitively', () => {
    expect(filterApps(apps, { query: 'camera', source: 'all', kind: 'all' }).map((app) => app.id)).toEqual(['c'])
    expect(filterApps(apps, { query: 'zzz', source: 'all', kind: 'all' })).toEqual([])
  })

  it('narrows by source and by kind together', () => {
    expect(filterApps(apps, { query: '', source: 'system', kind: 'all' }).map((app) => app.id)).toEqual(['b'])
    expect(filterApps(apps, { query: '', source: 'catalog', kind: 'container' }).map((app) => app.id)).toEqual(['a', 'c'])
    expect(filterApps(apps, { query: '', source: 'catalog', kind: 'native' })).toEqual([])
  })
})

describe('the retained-data count', () => {
  /// The prototype's header says how many applications still hold data. That
  /// is the reason removal is not deletion, so it counts what is retained
  /// rather than what is stopped.
  it('counts applications whose data survived removal', () => {
    expect(retainedCount(apps)).toBe(1)
    expect(retainedCount([])).toBe(0)
  })
})
