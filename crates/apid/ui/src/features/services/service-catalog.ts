import { Boxes, RadioTower, TerminalSquare } from 'lucide-react'

export interface ServiceDefinition {
  id: 'containers' | 'mqtt' | 'terminal'
  icon: typeof Boxes
  /// Absent when the device has no setting for this service, which is also
  /// what makes the service simulated.
  settingsPath?: string
  statePath?: string
}

export const serviceCatalog: readonly ServiceDefinition[] = [
  { id: 'containers', icon: Boxes, settingsPath: 'container.enabled', statePath: 'container' },
  { id: 'mqtt', icon: RadioTower, settingsPath: 'mqtt.enabled', statePath: 'mqtt' },
  { id: 'terminal', icon: TerminalSquare },
]

export function findService(id: string) {
  return serviceCatalog.find((service) => service.id === id)
}

/// The endpoint a service reports about itself. Nothing is assumed: a state
/// document that does not name an endpoint yields none, rather than a plausible
/// looking socket path the device never mentioned.
export function serviceEndpoint(state: Record<string, unknown> | undefined): string | undefined {
  for (const key of ['endpoint', 'address', 'socket', 'listen']) {
    const value = state?.[key]
    if (typeof value === 'string' && value.length > 0) return value
  }
  return undefined
}
