import { describe, expect, it } from 'vitest'
import { bridgeMembership, isSessionInterface } from './interface-facts'

describe('the interface this session arrived on', () => {
  /// The review dialog claims whether a change can cut the operator off. That
  /// claim is only worth making if it is derived from the address the browser
  /// actually connected to.
  it('matches the browser host against the interface addresses', () => {
    expect(isSessionInterface(['192.168.1.24/24'], '192.168.1.24')).toBe(true)
    expect(isSessionInterface(['192.168.1.24/24'], '192.168.1.25')).toBe(false)
    expect(isSessionInterface([], '192.168.1.24')).toBe(false)
  })

  it('unwraps a bracketed IPv6 host', () => {
    expect(isSessionInterface(['fd00::2/64'], '[fd00::2]')).toBe(true)
  })

  /// A name-based session cannot be attributed to one interface, so it must
  /// not be attributed to the one being edited either.
  it('claims nothing when the browser connected by name', () => {
    expect(isSessionInterface(['192.168.1.24/24'], 'mica-edge-07.local')).toBe(false)
  })
})

describe('bridge membership', () => {
  it('names the bridge that lists the interface as a port', () => {
    const configured = { br0: { kind: 'bridge', bridge: { ports: ['eth1', 'eth2'] } }, eth1: { dhcp: false } }
    expect(bridgeMembership(configured, 'eth1')).toBe('br0')
    expect(bridgeMembership(configured, 'eth0')).toBeUndefined()
  })

  it('ignores a bridge with no port list', () => {
    expect(bridgeMembership({ br0: { kind: 'bridge' } }, 'eth1')).toBeUndefined()
  })
})
