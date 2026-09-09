import { cleanup, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { AutomaticUpdatesPanel } from './automatic-updates-panel'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

/// A device following the image on both keys: nothing in `operator`.
const bakedOnly = {
  baked: { update: { source: 'https://updates.mos.example/repo', channel: 'stable', policy: 'check' } },
  operator: {},
  effective: { update: { source: 'https://updates.mos.example/repo', channel: 'stable', policy: 'check' } },
}

/// The same device re-pointed at another server and moved to `beta`.
const repointed = {
  baked: { update: { source: 'https://updates.mos.example/repo', channel: 'stable', policy: 'check' } },
  operator: { update: { source: 'https://mirror.site.example/repo', channel: 'beta' } },
  effective: { update: { source: 'https://mirror.site.example/repo', channel: 'beta', policy: 'check' } },
}

const policy = {
  lifecycle: {
    policy: {
      policy: 'check',
      checkIntervalMinutes: 1440,
      sourceUrl: 'https://updates.mos.example/repo',
      channel: 'stable',
      rebootPolicy: 'manual',
      maintenanceWindows: [],
    },
  },
}

describe('the automatic update policy', () => {
  /// PLAN-071 U7's gate.
  it('shows a re-pointed device which address it is using and that it is not the baked one', async () => {
    stubFetch({ '/api/v1/update': policy, '/api/v1/provisioning/status': repointed })
    renderPanel(<AutomaticUpdatesPanel />)

    expect(await screen.findByText('https://mirror.site.example/repo')).toBeTruthy()
    expect(screen.getByText('set here — the image says https://updates.mos.example/repo')).toBeTruthy()
    expect(screen.getByText('beta')).toBeTruthy()
    expect(screen.getByText('set here — the image says stable')).toBeTruthy()
  })

  it('says a device that never overrode either key is following its image', async () => {
    stubFetch({ '/api/v1/update': policy, '/api/v1/provisioning/status': bakedOnly })
    renderPanel(<AutomaticUpdatesPanel />)

    expect(await screen.findAllByText('from the image')).toHaveLength(2)
    // The form holds the OPERATOR's value, which is empty here. Seeding it
    // from the effective value would let a save pin the image's default into
    // the operator's layer without anyone asking for it.
    expect((screen.getByLabelText(/Update server address/) as HTMLInputElement).value).toBe('')
    expect((screen.getByLabelText(/Release channel/) as HTMLInputElement).value).toBe('')
  })

  it('writes the channel as a patch that names nothing else', async () => {
    const fetch = stubFetch({
      '/api/v1/update': policy,
      '/api/v1/provisioning/status': bakedOnly,
      'POST /api/v1/update/config': () => jsonResponse({ source: { channel: 'beta' } }),
    })
    renderPanel(<AutomaticUpdatesPanel />)

    await userEvent.type(await screen.findByLabelText(/Release channel/), 'beta')
    await userEvent.click(screen.getByRole('button', { name: 'Save source' }))

    await waitFor(() => expect(fetch.mock.calls.some(([input]) => String(input) === '/api/v1/update/config')).toBe(true))
    const call = fetch.mock.calls.find(([input]) => String(input) === '/api/v1/update/config')
    expect(call?.[1]?.method).toBe('POST')
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({ source: { url: null, channel: 'beta' } })
  })

  it('writes the mode, the cadence and the reboot policy together', async () => {
    const fetch = stubFetch({
      '/api/v1/update': policy,
      '/api/v1/provisioning/status': bakedOnly,
      'POST /api/v1/update/config': () => jsonResponse({ policy: 'check' }),
    })
    renderPanel(<AutomaticUpdatesPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Save policy' }))

    const call = fetch.mock.calls.find(([input]) => String(input) === '/api/v1/update/config')
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({
      policy: 'check',
      checkIntervalMinutes: 1440,
      rebootPolicy: 'manual',
    })
  })

  it('edits the window list the device holds rather than a single window', async () => {
    const twoWindows = {
      lifecycle: {
        policy: {
          ...policy.lifecycle.policy,
          maintenanceWindows: [
            { days: ['mon'], start: '02:00', end: '04:00' },
            { days: [], start: '22:00', end: '23:00' },
          ],
        },
      },
    }
    const fetch = stubFetch({
      '/api/v1/update': twoWindows,
      '/api/v1/provisioning/status': bakedOnly,
      'POST /api/v1/update/config': () => jsonResponse({}),
    })
    renderPanel(<AutomaticUpdatesPanel />)

    expect(await screen.findAllByLabelText(/From \(UTC\)/)).toHaveLength(2)
    await userEvent.click(screen.getByRole('button', { name: 'Save windows' }))

    const call = fetch.mock.calls.find(([input]) => String(input) === '/api/v1/update/config')
    expect(JSON.parse(String(call?.[1]?.body))).toEqual({
      maintenance: {
        windows: [
          { days: ['mon'], start: '02:00', end: '04:00' },
          { days: [], start: '22:00', end: '23:00' },
        ],
      },
    })
  })

  it('reports a refusal from the device verbatim, naming the rule it broke', async () => {
    stubFetch({
      '/api/v1/update': policy,
      '/api/v1/provisioning/status': bakedOnly,
      'POST /api/v1/update/config': () => jsonResponse({
        error: {
          code: 'validation_failed',
          message: '/mos/config/updates.json: policy `auto` requires at least one maintenance window',
        },
      }, 422),
    })
    renderPanel(<AutomaticUpdatesPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Save policy' }))

    expect(await screen.findByText(/requires at least one maintenance window/)).toBeTruthy()
  })

  it('names every deferral the automatic path records and shows how long it has waited', async () => {
    const expected: [string, RegExp][] = [
      ['no-newer-release', /publishes nothing newer/],
      ['outside-window', /outside the maintenance window/],
      ['clock-untrusted', /does not trust its clock/],
      ['reboot-gate-closed', /must not be interrupted/],
      // The fallback the daemon can actually produce: it clamps every
      // reason to the published set and reports anything else as `unknown`
      // (PLAN-076 B4), so a made-up word here would be testing a case no
      // device sends.
      ['unknown', /no name for/],
    ]
    for (const [reason, copy] of expected) {
      stubFetch({
        '/api/v1/update': {
          lifecycle: {
            policy: policy.lifecycle.policy,
            deferred: { reason, detail: 'the device said this', waitedSeconds: 7200, attempts: 5 },
          },
        },
        '/api/v1/provisioning/status': bakedOnly,
      })
      renderPanel(<AutomaticUpdatesPanel />)
      expect(await screen.findByText(copy)).toBeTruthy()
      expect(screen.getByText('the device said this')).toBeTruthy()
      expect(screen.getByText('Refused for 120 minutes, over 5 attempts.')).toBeTruthy()
      cleanup()
    }
  })

  it('reports a configuration that could not be read instead of an empty form', async () => {
    stubFetch({
      '/api/v1/update': { lifecycle: { policy_error: 'parse /mos/config/updates.json: expected value' } },
      '/api/v1/provisioning/status': () => jsonResponse({ error: { message: 'parse /mos/config/updates.json' } }, 409),
    })
    renderPanel(<AutomaticUpdatesPanel />)

    expect(await screen.findByText(/could not be read/)).toBeTruthy()
  })
})
