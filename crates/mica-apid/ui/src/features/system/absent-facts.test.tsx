import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderPanel, stubFetch } from '@/shared/testing/panel'
import { InformationPanel, TelemetryPanel } from './information-panel'
import { StoragePanel } from './storage-panel'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

const absent = (detail: string) => ({ available: false, detail })

/// The console's standing rule: a fact the device could not observe is stated
/// as unavailable with the reason, never as a dash and never as a zero.
/// "not observed" and "observed as nothing" are different answers, and an
/// operator acts differently on each.
describe('facts the device could not observe', () => {
  it('names the reason on every identity and software row', async () => {
    stubFetch({
      '/api/v1/system/info': {
        machineId: absent('no /etc/machine-id'),
        board: absent('no device tree'),
        release: absent('no os-release'),
        kernel: absent('uname failed'),
        system: absent('no manifest'),
        daemon: absent('bus unreachable'),
        deployment: absent('no deployment record'),
        uptime: absent('no /proc/uptime'),
        packages: absent('dpkg status unreadable'),
      },
      '/api/v1/system/telemetry': { thermal: absent('no thermal zones'), watchdog: absent('no watchdog'), reset: absent('no reset reason') },
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText(/no \/etc\/machine-id/)).toBeTruthy()
    expect(screen.getByText(/no device tree/)).toBeTruthy()
    expect(screen.getByText(/dpkg status unreadable/)).toBeTruthy()
  })

  it('reports an empty package list as empty rather than as absent', async () => {
    stubFetch({
      '/api/v1/system/info': {
        machineId: { available: true, id: 'abc' },
        board: { available: true }, release: { available: true }, kernel: { available: true },
        system: { available: true }, daemon: { available: true }, deployment: { available: true },
        uptime: { available: true, seconds: 60 },
        packages: { available: true, count: 0, micaCount: 0, entries: [] },
      },
      '/api/v1/system/telemetry': { thermal: absent('x'), watchdog: absent('x'), reset: absent('x') },
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('The manifest lists no packages.')).toBeTruthy()
  })

  it('renders a thermal and watchdog reading when the device does report one', async () => {
    stubFetch({
      '/api/v1/system/telemetry': {
        thermal: { available: true, zones: [{ sensor: 'cpu-thermal', label: 'CPU', milliCelsius: 47_300 }], hwmon: [] },
        watchdog: { available: true, devices: [{ device: '/dev/watchdog0', identity: 'dw_wdt', state: 'active', timeoutSeconds: 30, bootstatus: { available: true, flags: [] } }] },
        reset: { available: true, reason: 'power-on', detail: 'cold boot' },
      },
    })
    renderPanel(<TelemetryPanel />)

    expect(await screen.findByText(/CPU 47.3 °C/)).toBeTruthy()
    expect(screen.getByText(/dw_wdt/)).toBeTruthy()
  })

  it('states an absent storage tier and an unsupported wear metric', async () => {
    stubFetch({
      '/api/v1/storage/status': {
        tiers: [
          { name: 'DATA', present: false, mounted: false, check: { recorded: false } },
          { name: 'STATE', present: true, mounted: false, check: { recorded: true, unit: 'fsck', exitStatus: 1 } },
        ],
        namespaces: { sharedCapacityTier: 'DATA', binds: [{ name: 'state', mount: '/var/lib/mica', owner: 'micad', readiness: 'ready', mounted: true, sourceOnData: false, source: '/tmp/state', sourceIsDirectory: false, readOnly: true, probe: { attempted: false, reason: 'tier absent' } }] },
        media: [{ name: 'mmcblk0', health: { supported: false, reason: 'no eMMC health page', source: 'sysfs', lifetimeEstimates: [] } }],
        lifecycle: { discard: 'supported', secureErase: 'unsupported' },
      },
    })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText(/not present/)).toBeTruthy()
    expect(screen.getByText(/last check corrected errors/)).toBeTruthy()
    expect(screen.getByText(/no eMMC health page/)).toBeTruthy()
    expect(screen.getByText(/not backed by DATA/)).toBeTruthy()
  })
})
