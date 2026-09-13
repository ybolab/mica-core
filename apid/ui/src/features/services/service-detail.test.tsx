import { cleanup, screen, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderRoute, stubFetch } from '@/shared/testing/panel'
import { SimulationProvider } from '@/shared/simulation/simulation-provider'
import { TerminalWindow } from './terminal-window'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the simulated terminal window', () => {
  it('is a real dialog with a transcript and a way to end the session', async () => {
    // It used to be a div with role="dialog" over a hand-built backdrop: it
    // trapped no focus and Escape did nothing.
    stubFetch({})
    renderRoute(<SimulationProvider><TerminalWindow open onClose={() => {}} /></SimulationProvider>)

    const dialog = await screen.findByRole('dialog')
    expect(within(dialog).getByText(/systemctl --no-pager status micad/)).toBeTruthy()
    expect(within(dialog).getByText(/accepts no input/)).toBeTruthy()
    expect(within(dialog).getByRole('button', { name: 'End session' })).toBeTruthy()
  })

  it('minimizes to a pill that restores the window', async () => {
    stubFetch({})
    renderRoute(<SimulationProvider><TerminalWindow open onClose={() => {}} /></SimulationProvider>)

    await userEvent.click(within(await screen.findByRole('dialog')).getByRole('button', { name: 'Minimize' }))
    expect(screen.queryByRole('dialog')).toBeNull()

    await userEvent.click(screen.getByRole('button', { name: /Web terminal/ }))
    expect(await screen.findByRole('dialog')).toBeTruthy()
  })

  it('renders nothing when the session is closed', () => {
    stubFetch({})
    renderRoute(<SimulationProvider><TerminalWindow open={false} onClose={() => {}} /></SimulationProvider>)

    expect(screen.queryByRole('dialog')).toBeNull()
  })
})

