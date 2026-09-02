import { describe, expect, it } from 'vitest'
import { isNavActive } from './app-shell'

describe('navigation active state', () => {
  it('matches route boundaries instead of similarly prefixed pages', () => {
    expect(isNavActive('/_ui/network', '/network')).toBe(true)
    expect(isNavActive('/_ui/network-status', '/network')).toBe(false)
    expect(isNavActive('/_ui/network-status', '/network-status')).toBe(true)
    expect(isNavActive('/_ui/system-information', '/system')).toBe(false)
    expect(isNavActive('/_ui/system/ui', '/system')).toBe(true)
  })
})
