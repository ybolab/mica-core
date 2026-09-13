import { describe, expect, it } from 'vitest'
import { configuredSummary, networkRows } from './network'

describe('network rows', () => {
  it('keeps observed-only and configured-but-missing interfaces visible', () => {
    const rows = networkRows(
      { eth0: { kind: 'physical', dhcp: true }, missing0: { kind: 'vlan' } },
      [
        { index: 1, name: 'lo', operationalState: 'carrier' },
        { index: 2, name: 'eth0', operationalState: 'routable' },
      ],
    )

    expect(rows.map((row) => row.name)).toEqual(['lo', 'eth0', 'missing0'])
    expect(rows[0].configured).toBeUndefined()
    expect(rows[1].configured).toEqual({ kind: 'physical', dhcp: true })
    expect(rows[2].observed).toBeUndefined()
  })

  it('describes the configured kind and addressing method separately', () => {
    expect(configuredSummary({ kind: 'bridge', static: { address: '10.0.0.2/24' } }))
      .toBe('bridge · Static')
    expect(configuredSummary()).toBe('Not configured')
  })
})
