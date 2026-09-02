/// Whether this browser reached the device over the interface being edited.
/// Only an address match counts: a session opened by name could be resolving
/// to any of the device's addresses, and guessing which one would turn the
/// review dialog's warning into a coin flip.
export function isSessionInterface(addresses: readonly string[], host: string): boolean {
  const session = host.replace(/^\[/, '').replace(/\]$/, '')
  if (!session) return false
  return addresses.some((entry) => entry.split('/')[0] === session)
}

/// The bridge that claims this interface as a port, read from the desired
/// configuration rather than from the observed link.
export function bridgeMembership(configured: Record<string, unknown>, name: string): string | undefined {
  for (const [bridge, value] of Object.entries(configured)) {
    if (!value || typeof value !== 'object') continue
    const ports = (value as { bridge?: { ports?: unknown } }).bridge?.ports
    if (Array.isArray(ports) && ports.includes(name)) return bridge
  }
  return undefined
}
