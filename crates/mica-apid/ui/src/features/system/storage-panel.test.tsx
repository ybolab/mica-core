import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { StorageStatus } from '@/lib/types'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { StoragePanel } from './storage-panel'

function status(overrides: Partial<StorageStatus> = {}): StorageStatus {
  return {
    tiers: [
      {
        name: 'data', role: 'data', partitionLabel: 'DATA', present: true, device: '/dev/mmcblk0p8',
        mounted: true, mount: '/mnt/data', filesystem: 'ext4', readOnly: false,
        space: { totalBytes: 8_589_934_592, usedBytes: 4_294_967_296, freeBytes: 4_294_967_296, reservedBytes: 429_496_729, usedPercent: 50 },
        pressure: 'normal',
        check: { unit: 'systemd-fsck@dev-mmcblk0p8.service', result: 'success', exitStatus: 0 },
      },
      {
        name: 'boot', role: 'boot', partitionLabel: 'BOOT', present: false,
        detail: 'this board boots from SPI', check: { recorded: false },
      },
    ],
    namespaces: {
      sharedCapacityTier: 'data',
      detail: '/mica and /srv are binds of the DATA filesystem',
      binds: [
        { name: 'mica', mount: '/mica', source: '/mnt/data/mica', owner: 'system', readiness: 'ready', mounted: true, sourceOnData: true, sourceIsDirectory: true, probe: { attempted: true, passed: true } },
        { name: 'srv', mount: '/srv', source: '/mnt/data/srv', owner: 'user', readiness: 'ready', mounted: true, sourceOnData: true, sourceIsDirectory: true, probe: { attempted: true, passed: true } },
      ],
    },
    media: [
      {
        name: 'mmcblk0', kind: 'mmc', sizeBytes: 31_268_536_320, model: 'SDINBDA4',
        health: {
          supported: true, source: '/sys/class/mmc_host',
          raw: { lifeTime: '0x02 0x01', preEolInfo: '0x01' },
          lifetimeEstimates: [{ raw: '0x02', usedPercentMin: 10, usedPercentMax: 20 }],
          preEol: 'normal',
        },
      },
    ],
    policy: {
      warningPercent: 85, warningClearPercent: 80, criticalPercent: 95, criticalClearPercent: 90,
      watchedTiers: ['data'],
    },
    lifecycle: { backupRestore: 'unsupported', secureErase: 'unsupported' },
    ...overrides,
  }
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('storage status', () => {
  it('states the pool once, on the tier that owns it, and never per bind', async () => {
    stubFetch({ '/api/v1/storage/status': status() })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText(/4.0 GiB of 8.0 GiB used/)).toBeTruthy()
    expect(screen.getByText(/Both are views of one filesystem and share its capacity, reported once on the data tier/)).toBeTruthy()
    // A per-bind size bar would be a lie: /mica and /srv are two views of one
    // pool, and a second number here invites a reader to add them together.
    for (const mount of ['/mica', '/srv']) {
      const row = screen.getByText(mount).closest('div')!
      expect(row.textContent).not.toMatch(/GiB|MiB|%/)
    }
    expect(screen.getAllByText(/of 8.0 GiB used/)).toHaveLength(1)
  })

  it('reports an absent tier, its reserved pool and check evidence', async () => {
    stubFetch({ '/api/v1/storage/status': status() })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText('not present on this board')).toBeTruthy()
    expect(screen.getByText(/409.6 MiB reserved for root/)).toBeTruthy()
    expect(screen.getByText(/last check: success \(exit 0\)/)).toBeTruthy()
  })

  it('uses the exact readiness words for a degraded and an unavailable bind', async () => {
    stubFetch({
      '/api/v1/storage/status': status({
        namespaces: {
          sharedCapacityTier: 'data',
          detail: '/mica and /srv are binds of the DATA filesystem',
          binds: [
            { name: 'mica', mount: '/mica', source: '/mnt/data/mica', owner: 'system', readiness: 'degraded', mounted: true, readOnly: true, sourceOnData: true, probe: { attempted: false, reason: 'the mount is read-only' } },
            { name: 'srv', mount: '/srv', source: '/srv', owner: 'user', readiness: 'unavailable', mounted: true, sourceOnData: false, probe: { attempted: true, passed: false, error: 'EROFS' } },
          ],
        },
      }),
    })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText(/degraded — mounted on DATA but not fully writable/)).toBeTruthy()
    expect(screen.getByText(/no write probe: the mount is read-only/)).toBeTruthy()
    expect(screen.getByText(/unavailable — nothing may be written here/)).toBeTruthy()
    expect(screen.getByText(/its mount is not backed by DATA \(\/srv\)/)).toBeTruthy()
    expect(screen.getByText(/write probe failed: EROFS/)).toBeTruthy()
  })

  it('shows eMMC wear as the JEDEC bucket range, not one averaged percentage', async () => {
    stubFetch({ '/api/v1/storage/status': status() })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText(/lifetime used: 10–20% \(10% resolution\)/)).toBeTruthy()
    expect(screen.getByText(/end-of-life indicator: normal/)).toBeTruthy()
    expect(screen.queryByText(/lifetime used: 15%/)).toBeNull()
  })

  it('states unsupported wear with its reason and an empty media list as empty', async () => {
    stubFetch({
      '/api/v1/storage/status': status({
        media: [{ name: 'sda', kind: 'scsi', health: { supported: false, reason: 'this medium exports no wear counters' } }],
      }),
    })
    const first = renderPanel(<StoragePanel />)
    expect(await screen.findByText(/wear reporting unsupported: this medium exports no wear counters/)).toBeTruthy()
    first.unmount()

    stubFetch({ '/api/v1/storage/status': status({ media: [] }) })
    renderPanel(<StoragePanel />)
    expect(await screen.findByText('no media reported')).toBeTruthy()
  })

  it('names each lifecycle decision explicitly', async () => {
    stubFetch({ '/api/v1/storage/status': status() })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText('Backup and restore')).toBeTruthy()
    expect(screen.getAllByText('not supported')).toHaveLength(2)
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/storage/status': () => jsonResponse({ error: { message: 'micad is not reachable' } }, 503) })
    renderPanel(<StoragePanel />)

    expect(await screen.findByText('micad is not reachable')).toBeTruthy()
  })
})
