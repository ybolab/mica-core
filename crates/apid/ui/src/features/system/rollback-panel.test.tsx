import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { RollbackPanel } from './rollback-panel'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the guarded rollback', () => {
  it('offers the mark when the guard permits it, and says it does not reboot', async () => {
    const fetch = stubFetch({
      '/api/v1/update': { rollback: { target: 'b'.repeat(64), permitted: true, reason: null } },
      'POST /api/v1/update/rollback': () => jsonResponse({ deploymentId: 'a'.repeat(64), target: 'b'.repeat(64), nextStep: 'POST /api/v1/actions/reboot' }),
    })
    renderPanel(<RollbackPanel />)

    expect(await screen.findByText(`a rollback to ${'b'.repeat(64)} is permitted`)).toBeTruthy()
    await userEvent.click(screen.getByRole('button', { name: `Roll back to ${'b'.repeat(64)}` }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: `Roll back to ${'b'.repeat(64)}` }))

    await waitFor(() => expect(fetch.mock.calls.some(([input]) => String(input) === '/api/v1/update/rollback')).toBe(true))
    expect(await screen.findByText(`Deployment ${'a'.repeat(64)} was rejected. The next boot uses ${'b'.repeat(64)}.`)).toBeTruthy()
    expect(screen.getByText(/does not reboot. Reboot the device to complete the rollback/)).toBeTruthy()
  })

  it('renders a refusal as a refusal and offers no control at all', async () => {
    stubFetch({ '/api/v1/update': { rollback: { target: 'b'.repeat(64), permitted: false, reason: 'no_usable_fallback' } } })
    renderPanel(<RollbackPanel />)

    expect(await screen.findByText('rollback not available')).toBeTruthy()
    expect(screen.getByText(/no usable retained deployment/)).toBeTruthy()
    expect(screen.queryAllByRole('button')).toHaveLength(0)
  })

  it('reads a permitted-with-a-reason document as permitted and shows no refusal beside the control', async () => {
    // `permitted` is derived from the absence of a reason, so the two states
    // are the whole vocabulary. Nothing here may render a third.
    stubFetch({ '/api/v1/update': { rollback: { target: 'b'.repeat(64), permitted: true, reason: null } } })
    renderPanel(<RollbackPanel />)

    await screen.findByText(`a rollback to ${'b'.repeat(64)} is permitted`)
    expect(screen.queryByText(/^The deployment state refuses/)).toBeNull()
  })

  it('names each refusal the slot state can produce', async () => {
    const expected: [string, RegExp][] = [
      ['candidate_pending', /candidate deployment is pending/],
      ['running_not_confirmed', /running deployment is not confirmed/],
      ['no_usable_fallback', /no usable retained deployment/],
      ['something_new', /deployment state refuses a rollback/],
    ]
    for (const [reason, copy] of expected) {
      stubFetch({ '/api/v1/update': { rollback: { target: null, permitted: false, reason } } })
      renderPanel(<RollbackPanel />)
      expect(await screen.findByText(copy)).toBeTruthy()
      cleanup()
    }
  })

  it('maps a 409 from the mark onto the same refusal vocabulary', async () => {
    stubFetch({
      '/api/v1/update': { rollback: { target: 'b'.repeat(64), permitted: true, reason: null } },
      'POST /api/v1/update/rollback': () => jsonResponse({ error: { code: 'running_not_confirmed', message: 'refused' } }, 409),
    })
    renderPanel(<RollbackPanel />)

    await userEvent.click(await screen.findByRole('button', { name: `Roll back to ${'b'.repeat(64)}` }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: `Roll back to ${'b'.repeat(64)}` }))

    expect(await screen.findByText(/running deployment is not confirmed/)).toBeTruthy()
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/update': () => jsonResponse({ error: { message: 'micad is unavailable' } }, 503) })
    renderPanel(<RollbackPanel />)

    expect(await screen.findByText('micad is unavailable')).toBeTruthy()
  })
})
