import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { PowerPanel } from './system-page'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('power confirmation', () => {
  it.each([['Reboot', 'reboot'], ['Power off', 'poweroff']])('closes %s confirmation and exposes acceptance', async (label, action) => {
    const fetch = stubFetch({ [`POST /api/v1/actions/${action}`]: () => new Response(null, { status: 202 }) })
    renderPanel(<PowerPanel />)
    await userEvent.click(screen.getByRole('button', { name: label }))
    expect(fetch).not.toHaveBeenCalled()
    await userEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: label }))
    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect((await screen.findByRole('status')).textContent).toBe('Power action accepted.')
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('shows the backend refusal instead of hiding it behind confirmation', async () => {
    const fetch = stubFetch({ 'POST /api/v1/actions/reboot': () => jsonResponse({ error: { code: 'power_refused', message: 'An installation is still running.' } }, 409) })
    renderPanel(<PowerPanel />)
    await userEvent.click(screen.getByRole('button', { name: 'Reboot' }))
    await userEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Reboot' }))
    expect((await screen.findByRole('alert')).textContent).toBe('An installation is still running.')
    expect(screen.queryByRole('status')).toBeNull()
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('does not dispatch a cancelled confirmation', async () => {
    const fetch = stubFetch({})
    renderPanel(<PowerPanel />)
    await userEvent.click(screen.getByRole('button', { name: 'Reboot' }))
    await userEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Cancel' }))
    expect(fetch).not.toHaveBeenCalled()
  })
})
