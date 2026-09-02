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
      '/api/v1/update': { rollback: { target: 'rootfs.1', permitted: true, reason: null } },
      'POST /api/v1/update/rollback': () => jsonResponse({ slotName: 'rootfs.0', message: 'marked slot rootfs.0 as bad', target: 'rootfs.1', nextStep: 'POST /api/v1/actions/reboot' }),
    })
    renderPanel(<RollbackPanel />)

    expect(await screen.findByText('a rollback to rootfs.1 is permitted')).toBeTruthy()
    await userEvent.click(screen.getByRole('button', { name: 'Roll back to rootfs.1' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Roll back to rootfs.1' }))

    await waitFor(() => expect(fetch.mock.calls.some(([input]) => String(input) === '/api/v1/update/rollback')).toBe(true))
    expect(await screen.findByText('rootfs.0 was marked bad. The next boot comes from rootfs.1.')).toBeTruthy()
    expect(screen.getByText(/does not reboot. Reboot the device to complete the rollback/)).toBeTruthy()
    expect(screen.getByText('marked slot rootfs.0 as bad')).toBeTruthy()
  })

  it('renders a refusal as a refusal and offers no control at all', async () => {
    stubFetch({ '/api/v1/update': { rollback: { target: 'rootfs.1', permitted: false, reason: 'alternate_never_installed' } } })
    renderPanel(<RollbackPanel />)

    expect(await screen.findByText('rollback not available')).toBeTruthy()
    expect(screen.getByText(/never been written, so there is no system there to fall back to/)).toBeTruthy()
    expect(screen.queryAllByRole('button')).toHaveLength(0)
  })

  it('reads a permitted-with-a-reason document as permitted and shows no refusal beside the control', async () => {
    // `permitted` is derived from the absence of a reason, so the two states
    // are the whole vocabulary. Nothing here may render a third.
    stubFetch({ '/api/v1/update': { rollback: { target: 'rootfs.1', permitted: true, reason: null } } })
    renderPanel(<RollbackPanel />)

    await screen.findByText('a rollback to rootfs.1 is permitted')
    expect(screen.queryByText(/^The device's slot state refuses/)).toBeNull()
  })

  it('names each refusal the slot state can produce', async () => {
    const expected: [string, RegExp][] = [
      ['no_alternate_slot', /RAUC names no booted slot/],
      ['alternate_is_booted_slot', /single slot/],
      ['alternate_marked_bad', /already condemned the other slot/],
      ['alternate_is_newer', /pending update, not a rollback target/],
      ['install_order_unknown', /cannot be ordered by install time/],
      ['booted_slot_not_confirmed', /attempt counter/],
      ['something_new', /slot state refuses a rollback/],
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
      '/api/v1/update': { rollback: { target: 'rootfs.1', permitted: true, reason: null } },
      'POST /api/v1/update/rollback': () => jsonResponse({ error: { code: 'booted_slot_not_confirmed', message: 'refused' } }, 409),
    })
    renderPanel(<RollbackPanel />)

    await userEvent.click(await screen.findByRole('button', { name: 'Roll back to rootfs.1' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Roll back to rootfs.1' }))

    expect(await screen.findByText(/attempt counter/)).toBeTruthy()
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/update': () => jsonResponse({ error: { message: 'mosd is unavailable' } }, 503) })
    renderPanel(<RollbackPanel />)

    expect(await screen.findByText('mosd is unavailable')).toBeTruthy()
  })
})
