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
  it.each([['Reboot', 'reboot'], ['Power off', 'poweroff']])('closes the %s confirmation and reports acceptance', async (label, action) => {
    const fetch = stubFetch({ [`POST /api/v1/actions/${action}`]: () => new Response(null, { status: 202 }) })
    renderPanel(<PowerPanel />)
    await userEvent.click(screen.getByRole('button', { name: label }))
    expect(fetch).not.toHaveBeenCalled()
    await userEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: label }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect(await screen.findByText('Power action accepted.')).toBeTruthy()
    expect(fetch).toHaveBeenCalledTimes(1)
  })

  it('keeps the confirmation open on a refusal and shows the device sentence', async () => {
    // The device is the only thing that knows an install is still running, so
    // its sentence is what reaches the operator, and the dialog stays up
    // because their next move is to decide again.
    const fetch = stubFetch({ 'POST /api/v1/actions/reboot': () => jsonResponse({ error: { code: 'power_refused', message: 'An installation is still running.' } }, 409) })
    renderPanel(<PowerPanel />)
    await userEvent.click(screen.getByRole('button', { name: 'Reboot' }))
    await userEvent.click(within(screen.getByRole('alertdialog')).getByRole('button', { name: 'Reboot' }))

    expect(await screen.findByText('An installation is still running.')).toBeTruthy()
    expect(screen.queryByText('Power action accepted.')).toBeNull()
    expect(screen.queryByRole('alertdialog')).not.toBeNull()
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
