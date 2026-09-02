import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import type { StorageStatus } from '@/lib/types'
import { i18n } from '@/i18n/i18n'
import { LifecyclePanel, MediaPanel, NamespacesPanel, TiersPanel } from './storage'

function response(value: StorageStatus) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

const POLICY: StorageStatus['policy'] = {
  warningPercent: 80,
  warningClearPercent: 75,
  criticalPercent: 90,
  criticalClearPercent: 85,
  updateWorkspaceReservedBytes: 268435456,
  updateWorkspaceRoot: '/mos/updates',
  watchedTiers: ['data', 'state'],
}

const NO_NAMESPACES: StorageStatus['namespaces'] = {
  sharedCapacityTier: 'data',
  detail: 'one filesystem, two namespaces',
  binds: [],
}

function renderPanel(node: React.ReactNode, value: StorageStatus) {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response(value)))
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>{node}</QueryClientProvider>
    </I18nextProvider>,
  )
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('storage tiers', () => {
  it('names an absent tier instead of showing it as empty', async () => {
    renderPanel(<TiersPanel />, {
      tiers: [
        {
          name: 'esp',
          role: 'esp',
          partitionLabel: 'esp',
          present: false,
          detail: 'no partition named `esp` on this board',
          check: { recorded: false },
        },
      ],
      media: [],
      namespaces: NO_NAMESPACES,
      policy: POLICY,
      lifecycle: {},
    })

    expect(await screen.findByText('not present on this board')).toBeTruthy()
  })

  it('reports space, pressure and a check that corrected errors', async () => {
    renderPanel(<TiersPanel />, {
      tiers: [
        {
          name: 'data',
          role: 'ext4',
          partitionLabel: 'data',
          present: true,
          mounted: true,
          mount: '/mnt/data',
          readOnly: false,
          space: {
            totalBytes: 1024 * 1024 * 1024,
            usedBytes: 900 * 1024 * 1024,
            freeBytes: 74 * 1024 * 1024,
            reservedBytes: 50 * 1024 * 1024,
            usedPercent: 87,
          },
          pressure: 'warning',
          updateWorkspace: { root: '/mos/updates', reservedBytes: 268435456, available: false },
          check: { unit: 'systemd-fsck@dev-mmcblk0p11.service', result: 'success', exitStatus: 1 },
        },
      ],
      media: [],
      namespaces: NO_NAMESPACES,
      policy: POLICY,
      lifecycle: {},
    })

    const summary = await screen.findByText(/900\.0 MiB/)
    // The reserved pool is its own number: free space root can use and an
    // application cannot is not free space.
    expect(summary.textContent).toContain('50.0 MiB reserved for root')
    expect(summary.textContent).toContain('low space')
    // Exit 1 is "errors were corrected", which the operator has to see as
    // repair rather than as a bare number.
    expect(summary.textContent).toContain('corrected errors')
    // An exhausted reservation says an update will be refused, because that
    // is the consequence the operator acts on.
    expect(screen.getByText(/an update will be refused/)).toBeTruthy()
  })

  it('says a tier was never checked rather than leaving it blank', async () => {
    renderPanel(<TiersPanel />, {
      tiers: [
        {
          name: 'rootfs-b',
          role: 'verity-slot',
          partitionLabel: 'rootfs-b',
          present: true,
          mounted: false,
          partitionBytes: 268435456,
          check: { recorded: false },
        },
      ],
      media: [],
      namespaces: NO_NAMESPACES,
      policy: POLICY,
      lifecycle: {},
    })

    const summary = await screen.findByText(/not mounted/)
    expect(summary.textContent).toContain('never checked')
    // An unmounted slot still has a partition size, which is the only
    // capacity number it has.
    expect(summary.textContent).toContain('256.0 MiB')
  })
})

describe('storage namespaces', () => {
  function withBinds(binds: StorageStatus['namespaces']['binds']): StorageStatus {
    return {
      tiers: [],
      namespaces: { ...NO_NAMESPACES, binds },
      media: [],
      policy: POLICY,
      lifecycle: {},
    }
  }

  it('says the two namespaces share one capacity pool', async () => {
    renderPanel(<NamespacesPanel />, withBinds([
      { name: 'mos', mount: '/mos', source: '/mnt/data/mos', owner: 'system', readiness: 'ready', mounted: true, sourceOnData: true, probe: { attempted: true, passed: true } },
      { name: 'srv', mount: '/srv', source: '/mnt/data/srv', owner: 'user', readiness: 'ready', mounted: true, sourceOnData: true, probe: { attempted: false, reason: 'user-owned' } },
    ]))

    // The shared-pool sentence is the point: two mounts, one capacity, and a
    // reader must not add them together.
    expect(await screen.findByText(/share its capacity/)).toBeTruthy()
    const mos = await screen.findByText(/system-owned/, { selector: 'dd' })
    expect(mos.textContent).toContain('ready')
    expect(mos.textContent).toContain('write probe passed')
    // No probe in the user namespace, and it says why rather than passing.
    const srv = screen.getByText(/user-owned/, { selector: 'dd' })
    expect(srv.textContent).toContain('no write probe')
    expect(srv.textContent).not.toContain('write probe passed')
  })

  it('names a bind that is not backed by DATA as an error, not as ok', async () => {
    renderPanel(<NamespacesPanel />, withBinds([
      { name: 'mos', mount: '/mos', source: '/mnt/data/mos', owner: 'system', readiness: 'unavailable', mounted: true, sourceOnData: false, probe: { attempted: false, reason: 'not on DATA' } },
    ]))

    const row = await screen.findByText(/unavailable/, { selector: 'dd' })
    expect(row.textContent).toContain('nothing may be written here')
    expect(row.textContent).toContain('not backed by DATA')
  })

  it('reports a failed write probe with its error', async () => {
    renderPanel(<NamespacesPanel />, withBinds([
      { name: 'mos', mount: '/mos', source: '/mnt/data/mos', owner: 'system', readiness: 'degraded', mounted: true, sourceOnData: true, readOnly: true, probe: { attempted: true, passed: false, error: 'EROFS' } },
    ]))

    const row = await screen.findByText(/degraded/, { selector: 'dd' })
    expect(row.textContent).toContain('write probe failed: EROFS')
    expect(row.textContent).toContain('read-only')
  })
})

describe('storage media', () => {
  it('states why wear is unsupported instead of leaving the medium blank', async () => {
    renderPanel(<MediaPanel />, {
      tiers: [],
      media: [
        {
          name: 'nvme0n1',
          kind: 'nvme',
          sizeBytes: 256 * 1024 * 1024 * 1024,
          health: { supported: false, reason: 'this image ships no smartctl or nvme-cli, by design' },
        },
      ],
      namespaces: NO_NAMESPACES,
      policy: POLICY,
      lifecycle: {},
    })

    expect(await screen.findByText(/no smartctl or nvme-cli/)).toBeTruthy()
  })

  it('reports eMMC lifetime as the bucket it is, never as one number', async () => {
    renderPanel(<MediaPanel />, {
      tiers: [],
      media: [
        {
          name: 'mmcblk0',
          kind: 'mmc',
          sizeBytes: 32 * 1024 * 1024 * 1024,
          health: {
            supported: true,
            source: 'sysfs mmc life_time / pre_eol_info',
            raw: { lifeTime: '0x03 0x02', preEolInfo: '0x01' },
            lifetimeEstimates: [{ raw: '0x03', usedPercentMin: 20, usedPercentMax: 30 }],
            preEol: 'normal',
          },
        },
      ],
      namespaces: NO_NAMESPACES,
      policy: POLICY,
      lifecycle: {},
    })

    const wear = await screen.findByText(/lifetime used/)
    expect(wear.textContent).toContain('20–30%')
    expect(wear.textContent).toContain('end-of-life indicator: normal')
  })
})

describe('data lifecycle', () => {
  it('shows every decision as explicitly unsupported', async () => {
    renderPanel(<LifecyclePanel />, {
      tiers: [],
      media: [],
      namespaces: NO_NAMESPACES,
      policy: POLICY,
      lifecycle: {
        backupRestore: 'unsupported',
        encryption: 'unsupported',
        factoryReset: 'unsupported',
      },
    })

    expect(await screen.findByText('Backup and restore')).toBeTruthy()
    expect(screen.getByText('Encryption at rest')).toBeTruthy()
    expect(screen.getAllByText('not supported')).toHaveLength(3)
  })
})
