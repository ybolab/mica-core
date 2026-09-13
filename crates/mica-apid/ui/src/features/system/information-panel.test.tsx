import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { SystemInformation, SystemTelemetry } from '@/lib/types'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { InformationPanel, TelemetryPanel } from './information-panel'

const available = { available: true }

function information(overrides: Partial<SystemInformation> = {}): SystemInformation {
  return {
    machineId: { ...available, id: '7f1c2ad0f0e4' },
    board: { ...available, model: 'Radxa CM3576', source: 'device-tree' },
    kernel: { ...available, release: '6.12.0-mica', version: '#1 SMP' },
    release: { ...available, prettyName: 'mica 2026.09', imageVersion: '2026.09.0' },
    system: {
      ...available,
      version: '2026.09.0',
      package: 'mica-system',
      gitStamp: { commit: 'abc1234', dirty: false, consistent: true, stamps: ['abc1234'] },
      commitDate: { available: true, date: '2026-09-01T12:34:56+08:00' },
      fileEpoch: { available: true, epoch: 1_577_836_800, date: '2020-01-01T00:00:00Z' },
    },
    daemon: { ...available, name: 'micad', version: '0.4.1', commit: 'abc1234' },
    packages: { ...available, count: 2, micaCount: 1, entries: [
      { name: 'mica-system', version: '2026.09.0', architecture: 'arm64', mica: true },
      { name: 'busybox', version: '1.36.1', architecture: 'arm64', mica: false },
    ] },
    deployment: { ...available, id: 'a'.repeat(64), version: '2026.09.0', confirmed: true },
    uptime: { ...available, seconds: 93_784 },
    ...overrides,
  }
}

const telemetryAbsent: SystemTelemetry = {
  thermal: { available: false, detail: 'no thermal zone and no hwmon temperature input is exported under sysfs' },
  watchdog: { available: false, detail: 'no watchdog device is exported under sysfs' },
  reset: { available: false, reason: 'unknown', detail: "this board's kernel exports no reset-reason source: no watchdog bootstatus and no pstore" },
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('system information', () => {
  it('renders identity, image provenance, boot slot and the package manifest', async () => {
    stubFetch({ '/api/v1/system/info': information(), '/api/v1/system/telemetry': telemetryAbsent })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('7f1c2ad0f0e4')).toBeTruthy()
    expect(screen.getByText('Radxa CM3576 · device-tree')).toBeTruthy()
    expect(screen.getByText('2026.09.0 · mica-system · git abc1234 (consistent)')).toBeTruthy()
    expect(screen.getByText('2026-09-01T12:34:56+08:00')).toBeTruthy()
    expect(screen.getByText(`${'a'.repeat(64)} · 2026.09.0`)).toBeTruthy()
    expect(screen.getByText('1d 2h 3m')).toBeTruthy()
    expect(screen.getByText('2 packages, including 1 mica packages.')).toBeTruthy()
    expect(screen.getByText('busybox')).toBeTruthy()
  })

  it('renders an absent fact as absence with its reason, not as a zero and not as a failure', async () => {
    stubFetch({
      '/api/v1/system/info': information({
        machineId: { available: false, detail: '/etc/machine-id is empty' },
        deployment: { available: false, detail: 'native deployment status is unavailable' },
      }),
      '/api/v1/system/telemetry': telemetryAbsent,
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('Unavailable — /etc/machine-id is empty')).toBeTruthy()
    expect(screen.getByText('Unavailable — native deployment status is unavailable')).toBeTruthy()
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('states an absent source commit date as absence, never as the image file epoch', async () => {
    const base = information()
    stubFetch({
      '/api/v1/system/info': information({
        system: {
          ...base.system,
          commitDate: { available: false, detail: '/usr/share/mica/release-identity.env states no COMMIT_DATE' },
        },
      }),
      '/api/v1/system/telemetry': telemetryAbsent,
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('Unavailable — /usr/share/mica/release-identity.env states no COMMIT_DATE')).toBeTruthy()
    // The pinned file epoch is the same instant in every image ever built; it
    // must never stand in for the date the image's source was committed.
    expect(screen.queryByText('2020-01-01T00:00:00Z')).toBeNull()
  })

  it('states an empty manifest instead of drawing an empty table', async () => {
    stubFetch({
      '/api/v1/system/info': information({ packages: { available: true, count: 0, micaCount: 0, entries: [] } }),
      '/api/v1/system/telemetry': telemetryAbsent,
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('The manifest lists no packages.')).toBeTruthy()
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('reports an unreadable manifest with its reason', async () => {
    stubFetch({
      '/api/v1/system/info': information({
        packages: { available: false, detail: '/usr/share/mica/packages.tsv is not readable' },
      }),
      '/api/v1/system/telemetry': telemetryAbsent,
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('Unavailable — /usr/share/mica/packages.tsv is not readable')).toBeTruthy()
    expect(screen.queryByRole('table')).toBeNull()
  })

  it('warns when the manifest was truncated or carried malformed rows', async () => {
    stubFetch({
      '/api/v1/system/info': information({
        packages: {
          available: true,
          count: 4096,
          micaCount: 12,
          truncated: true,
          malformedRows: 3,
          entries: [{ name: 'mica-system', version: '2026.09.0', architecture: 'arm64', mica: true }],
        },
      }),
      '/api/v1/system/telemetry': telemetryAbsent,
    })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText(/this list is truncated/)).toBeTruthy()
    expect(screen.getByText('3 malformed manifest rows were skipped.')).toBeTruthy()
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/system/info': () => jsonResponse({ error: { message: 'micad is not reachable' } }, 503) })
    renderPanel(<InformationPanel />)

    expect(await screen.findByText('micad is not reachable')).toBeTruthy()
  })
})

describe('board telemetry', () => {
  it('names each absent source rather than showing a healthy reading', async () => {
    stubFetch({ '/api/v1/system/telemetry': telemetryAbsent })
    renderPanel(<TelemetryPanel />)

    expect(await screen.findByText(/no thermal zone and no hwmon temperature input/)).toBeTruthy()
    expect(screen.getByText(/no watchdog device is exported under sysfs/)).toBeTruthy()
    expect(screen.getByText(/exports no reset-reason source/)).toBeTruthy()
  })

  it('renders temperature, watchdog boot status and a watchdog reset reason', async () => {
    stubFetch({
      '/api/v1/system/telemetry': {
        thermal: { available: true, zones: [{ sensor: 'thermal_zone0', label: 'soc', milliCelsius: 46_200 }], hwmon: [] },
        watchdog: { available: true, devices: [{
          device: 'watchdog0',
          identity: 'dw_wdt',
          state: 'active',
          timeoutSeconds: 30,
          bootstatus: { available: true, raw: 32, flags: ['cardReset'] },
        }] },
        reset: { available: true, reason: 'watchdog', detail: 'a watchdog reports WDIOF_CARDRESET: the last reboot was a watchdog reset' },
      } satisfies SystemTelemetry,
    })
    renderPanel(<TelemetryPanel />)

    expect(await screen.findByText('soc 46.2 °C')).toBeTruthy()
    expect(screen.getByText('watchdog0 · dw_wdt · active · timeout 30s · cardReset')).toBeTruthy()
    expect(screen.getByText(/watchdog reset · a watchdog reports WDIOF_CARDRESET/)).toBeTruthy()
  })
})
