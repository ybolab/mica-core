import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { CredentialRecoveryPanel } from './credential-recovery-panel'
import { ResetPanel } from './reset-panel'

const nothingStaged = () => jsonResponse({ error: { code: 'settings_not_found', message: 'no such dot-path' } }, 404)

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the reset tiers', () => {
  it('stages an authenticated tier by name and says the second step is a reboot', async () => {
    const fetch = stubFetch({
      '/api/v1/settings/reset': nothingStaged,
      'POST /api/v1/reset': () => jsonResponse({ tier: 'configuration', applies: 'next-boot' }, 202),
    })
    renderPanel(<ResetPanel />)

    const controls = await screen.findAllByRole('button', { name: 'Stage reset' })
    await userEvent.click(controls[0])
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Stage reset' }))

    await waitFor(() => expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'POST')).toBe(true))
    const post = fetch.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'POST')!
    expect(post[0]).toBe('/api/v1/reset')
    expect((post[1] as RequestInit).body).toBe('{"tier":"configuration"}')

    expect(await screen.findByText(/configuration reset is staged and runs on the next boot/)).toBeTruthy()
    expect(screen.getByText(/reboot the device to apply it/)).toBeTruthy()
  })

  it('offers no control for the full factory reset and says why', async () => {
    stubFetch({ '/api/v1/settings/reset': nothingStaged })
    renderPanel(<ResetPanel />)

    expect(await screen.findByText('full factory reset')).toBeTruthy()
    expect(screen.getByText('requires physical presence')).toBeTruthy()
    expect(screen.getByText(/nothing in this build writes one, so the device refuses every such request/)).toBeTruthy()
    // Two reachable tiers, two controls. A third would promise tier 3.
    expect(screen.getAllByRole('button', { name: 'Stage reset' })).toHaveLength(2)
  })

  it('answers a request for a secure wipe rather than leaving the absence to be inferred', async () => {
    stubFetch({ '/api/v1/settings/reset': nothingStaged })
    renderPanel(<ResetPanel />)

    expect(await screen.findByText(/There is no secure-wipe tier/)).toBeTruthy()
  })

  it('keeps a staged tier visible across a reload, with the moment it was asked for', async () => {
    stubFetch({ '/api/v1/settings/reset': { tier: 'application-data', requested: 1_756_000_000 } })
    renderPanel(<ResetPanel />)

    expect(await screen.findByText(/application-data reset is staged and runs on the next boot/)).toBeTruthy()
    expect(screen.getByText(/Requested at .* UTC/)).toBeTruthy()
    expect(screen.getByText(/Staging another tier replaces it/)).toBeTruthy()
  })

  it('does not report an absent staged reset as a failed read', async () => {
    stubFetch({ '/api/v1/settings/reset': nothingStaged })
    renderPanel(<ResetPanel />)

    await screen.findByText('configuration reset')
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('surfaces a refused staging verbatim', async () => {
    stubFetch({
      '/api/v1/settings/reset': nothingStaged,
      'POST /api/v1/reset': () => jsonResponse({ error: { code: 'rotation_required', message: 'this device was claimed with a bootstrap credential that has not been replaced' } }, 409),
    })
    renderPanel(<ResetPanel />)

    const controls = await screen.findAllByRole('button', { name: 'Stage reset' })
    await userEvent.click(controls[0])
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Stage reset' }))

    expect(await screen.findByText(/claimed with a bootstrap credential/)).toBeTruthy()
  })
})

describe('credential recovery', () => {
  it('is rendered and offers no control, because presence cannot be asserted on this build', () => {
    renderPanel(<CredentialRecoveryPanel />)

    expect(screen.getByText('Credential recovery')).toBeTruthy()
    expect(screen.getByText(/never reveals, decrypts or derives the old one/)).toBeTruthy()
    expect(screen.getByText(/nothing in it writes an assertion/)).toBeTruthy()
    expect(screen.getByText(/no software path back in/)).toBeTruthy()
    expect(screen.queryAllByRole('button')).toHaveLength(0)
  })
})
