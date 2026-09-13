import { cleanup, render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import { i18n, setLocaleChoice } from '@/i18n/i18n'
import { SimulationNotice } from './simulation-notice'
import { SimulationProvider, useSimulation } from './simulation-provider'

function Probe() {
  const simulation = useSimulation()
  const mqtt = simulation.apps.find((item) => item.id === 'mqtt-bridge')
  const modbus = simulation.apps.find((item) => item.id === 'modbus')
  return (
    <div>
      <span data-testid="mqtt-runtime">{mqtt?.runtime}</span>
      <span data-testid="mqtt-health">{mqtt?.health}</span>
      <span data-testid="modbus-version">{modbus?.version}</span>
      <span data-testid="terminal">{String(simulation.terminalEnabled)}</span>
      <span data-testid="update-phase">{simulation.updatePhase}</span>
      <span data-testid="support">{String(simulation.supportAccess)}</span>
      <span data-testid="activity-count">{simulation.activity.length}</span>
      <button type="button" onClick={() => simulation.installApp('mqtt-bridge')}>install</button>
      <button type="button" onClick={() => simulation.toggleApp('mqtt-bridge')}>toggle</button>
      <button type="button" onClick={() => simulation.updateApp('modbus')}>update modbus</button>
      <button type="button" onClick={() => simulation.updateApp('mqtt-bridge')}>update mqtt</button>
      <button type="button" onClick={() => simulation.removeApp('mqtt-bridge')}>remove</button>
      <button type="button" onClick={() => simulation.installApp('missing')}>missing</button>
      <button type="button" onClick={() => simulation.setTerminalEnabled(true)}>terminal</button>
      <button type="button" onClick={simulation.advanceUpdate}>advance</button>
      <button type="button" onClick={() => simulation.setSupportAccess(true)}>support</button>
    </div>
  )
}

afterEach(async () => {
  cleanup()
  vi.unstubAllGlobals()
  await i18n.changeLanguage('en')
})

describe('simulation boundary', () => {
  it('labels simulated pages in the active locale', async () => {
    const { rerender } = render(
      <I18nextProvider i18n={i18n}>
        <SimulationNotice scope="Applications" />
      </I18nextProvider>,
    )

    expect(screen.getByText('Simulation')).toBeTruthy()
    expect(screen.getByText(/does not change this device/)).toBeTruthy()

    await setLocaleChoice('zh-CN')
    rerender(
      <I18nextProvider i18n={i18n}>
        <SimulationNotice scope="应用" />
      </I18nextProvider>,
    )
    expect(screen.getByText('模拟界面')).toBeTruthy()
  })

  it('keeps simulated changes ephemeral and off the network', async () => {
    const fetch = vi.fn()
    vi.stubGlobal('fetch', fetch)
    const first = render(<SimulationProvider><Probe /></SimulationProvider>)

    expect(screen.getByTestId('mqtt-runtime').textContent).toBe('not-installed')
    await userEvent.click(screen.getByRole('button', { name: 'install' }))
    expect(screen.getByTestId('mqtt-runtime').textContent).toBe('running')
    expect(fetch).not.toHaveBeenCalled()

    first.unmount()
    render(<SimulationProvider><Probe /></SimulationProvider>)
    expect(screen.getByTestId('mqtt-runtime').textContent).toBe('not-installed')
  })

  it('executes every in-memory application and service transition', async () => {
    render(<SimulationProvider><Probe /></SimulationProvider>)

    await userEvent.click(screen.getByRole('button', { name: 'missing' }))
    expect(screen.getByTestId('activity-count').textContent).toBe('3')

    await userEvent.click(screen.getByRole('button', { name: 'install' }))
    await userEvent.click(screen.getByRole('button', { name: 'toggle' }))
    expect(screen.getByTestId('mqtt-runtime').textContent).toBe('stopped')
    await userEvent.click(screen.getByRole('button', { name: 'toggle' }))
    expect(screen.getByTestId('mqtt-runtime').textContent).toBe('running')

    await userEvent.click(screen.getByRole('button', { name: 'update modbus' }))
    expect(screen.getByTestId('modbus-version').textContent).toBe('1.9.0')
    await userEvent.click(screen.getByRole('button', { name: 'update mqtt' }))
    await userEvent.click(screen.getByRole('button', { name: 'remove' }))
    expect(screen.getByTestId('mqtt-runtime').textContent).toBe('not-installed')
    expect(screen.getByTestId('mqtt-health').textContent).toBe('retained')

    await userEvent.click(screen.getByRole('button', { name: 'terminal' }))
    await userEvent.click(screen.getByRole('button', { name: 'support' }))
    expect(screen.getByTestId('terminal').textContent).toBe('true')
    expect(screen.getByTestId('support').textContent).toBe('true')
  })

  it('cycles through all simulated update phases', async () => {
    render(<SimulationProvider><Probe /></SimulationProvider>)
    const phases = ['installing', 'reboot-required', 'succeeded', 'idle', 'checking', 'ready']

    for (const phase of phases) {
      await userEvent.click(screen.getByRole('button', { name: 'advance' }))
      expect(screen.getByTestId('update-phase').textContent).toBe(phase)
    }
  })
})
