import { createContext, useContext, useMemo, useState, type ReactNode } from 'react'

/// The prototype's in-memory device, for the surfaces the daemon does not
/// serve yet.
///
/// **The automatic-update policy left this file** (PLAN-071 U7). The channel,
/// the address, the mode, the cadence and the maintenance windows are now
/// writes against `POST /api/v1/update/config`, so there is no simulated
/// `automaticUpdates` to hold: a control that wrote in production and
/// simulated in the demo would be one code path that behaves two ways, which
/// is the shape that hides a regression. `updatePhase` stays, because it backs
/// the applications prototype's progress animation and nothing on the update
/// tab reads it.

export type SimulatedRuntime = 'not-installed' | 'running' | 'stopped' | 'blocked'

export interface SimulatedApp {
  id: string
  name: string
  version: string
  source: 'catalog' | 'local' | 'system'
  kind: 'container' | 'native'
  runtime: SimulatedRuntime
  health: 'healthy' | 'unknown' | 'retained'
  /// What the operator asked for, which the prototype shows beside what is
  /// actually running. They disagree whenever an application failed to start.
  desired: 'enabled' | 'disabled'
}

export interface SimulatedActivity {
  id: string
  action: 'update' | 'start' | 'remove' | 'install' | 'changeRuntime'
  app: string
  result: 'succeeded' | 'failed'
  time: 'justNow' | 'hourAgo' | 'dayAgo' | 'twoDaysAgo'
}

export type UpdatePhase = 'idle' | 'checking' | 'ready' | 'installing' | 'reboot-required' | 'succeeded'

interface SimulationValue {
  apps: SimulatedApp[]
  activity: SimulatedActivity[]
  terminalEnabled: boolean
  updatePhase: UpdatePhase
  supportAccess: boolean
  installApp: (id: string) => void
  toggleApp: (id: string) => void
  removeApp: (id: string) => void
  updateApp: (id: string) => void
  setTerminalEnabled: (enabled: boolean) => void
  advanceUpdate: () => void
  setSupportAccess: (enabled: boolean) => void
}

const initialApps: SimulatedApp[] = [
  { id: 'node-red', name: 'Node-RED', version: '4.0.9', source: 'catalog', kind: 'container', runtime: 'running', health: 'healthy', desired: 'enabled' },
  { id: 'modbus', name: 'Modbus Gateway', version: '1.8.2', source: 'local', kind: 'container', runtime: 'running', health: 'healthy', desired: 'enabled' },
  { id: 'metrics', name: 'System Metrics', version: '2026.08', source: 'system', kind: 'native', runtime: 'running', health: 'healthy', desired: 'enabled' },
  { id: 'camera', name: 'Camera Agent', version: '2.1.0', source: 'catalog', kind: 'container', runtime: 'stopped', health: 'retained', desired: 'disabled' },
  { id: 'mqtt-bridge', name: 'MQTT Bridge', version: '2.4.0', source: 'catalog', kind: 'container', runtime: 'not-installed', health: 'unknown', desired: 'disabled' },
  { id: 'serial-bridge', name: 'Serial Bridge', version: '1.2.1', source: 'catalog', kind: 'container', runtime: 'not-installed', health: 'unknown', desired: 'disabled' },
  { id: 'device-agent', name: 'Device Agent', version: '0.9.0', source: 'catalog', kind: 'native', runtime: 'not-installed', health: 'unknown', desired: 'disabled' },
]

const initialActivity: SimulatedActivity[] = [
  { id: 'activity-1', action: 'update', app: 'Modbus Gateway', result: 'succeeded', time: 'hourAgo' },
  { id: 'activity-2', action: 'start', app: 'Node-RED', result: 'succeeded', time: 'dayAgo' },
  { id: 'activity-3', action: 'remove', app: 'Camera Agent', result: 'succeeded', time: 'twoDaysAgo' },
]

const SimulationContext = createContext<SimulationValue | null>(null)

export function SimulationProvider({ children }: { children: ReactNode }) {
  const [apps, setApps] = useState(initialApps)
  const [activity, setActivity] = useState(initialActivity)
  const [terminalEnabled, setTerminalEnabled] = useState(false)
  const [updatePhase, setUpdatePhase] = useState<UpdatePhase>('ready')
  const [supportAccess, setSupportAccess] = useState(false)

  const record = (action: SimulatedActivity['action'], app: SimulatedApp) => {
    setActivity((items) => [
      { id: `activity-${items.length + 1}`, action, app: app.name, result: 'succeeded', time: 'justNow' },
      ...items,
    ])
  }
  const changeApp = (id: string, action: SimulatedActivity['action'], change: (app: SimulatedApp) => SimulatedApp) => {
    const current = apps.find((app) => app.id === id)
    if (!current) return
    const next = change(current)
    setApps((items) => items.map((app) => app.id === id ? next : app))
    record(action, next)
  }
  const installApp = (id: string) => changeApp(id, 'install', (app) => ({ ...app, runtime: 'running', health: 'healthy', desired: 'enabled' }))
  const toggleApp = (id: string) => changeApp(id, 'changeRuntime', (app) => app.runtime === 'running'
    ? { ...app, runtime: 'stopped', desired: 'disabled' }
    : { ...app, runtime: 'running', desired: 'enabled' })
  const updateApp = (id: string) => changeApp(id, 'update', (app) => ({ ...app, version: app.id === 'modbus' ? '1.9.0' : app.version }))
  const removeApp = (id: string) => changeApp(id, 'remove', (app) => ({ ...app, runtime: 'not-installed', health: 'retained', desired: 'disabled' }))
  const advanceUpdate = () => setUpdatePhase((phase) => ({
    idle: 'checking',
    checking: 'ready',
    ready: 'installing',
    installing: 'reboot-required',
    'reboot-required': 'succeeded',
    succeeded: 'idle',
  })[phase] as UpdatePhase)

  const value = useMemo<SimulationValue>(() => ({
    apps,
    activity,
    terminalEnabled,
    updatePhase,
    supportAccess,
    installApp,
    toggleApp,
    removeApp,
    updateApp,
    setTerminalEnabled,
    advanceUpdate,
    setSupportAccess,
  }), [apps, activity, terminalEnabled, updatePhase, supportAccess])

  return <SimulationContext value={value}>{children}</SimulationContext>
}

export function useSimulation() {
  const value = useContext(SimulationContext)
  if (!value) throw new Error('useSimulation must be used within SimulationProvider')
  return value
}
