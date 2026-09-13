import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { renderRoute, stubFetch } from '@/shared/testing/panel'
import { ThemeProvider } from '@/theme/theme'
import { AppShell } from './app-shell'

const routes = {
  '/api/v1/health': { apid: 'ok', micad: 'ok', checkedAt: 183_900 },
  '/api/v1/system/info': { release: { available: true, imageVersion: '2026.08.2' }, deployment: { available: true, id: '9e12aa77bb33cc44' } },
  '/api/v1/settings/hostname': 'mica-cm4',
  '/api/v1/claim': { state: 'claimed', rotationRequired: false },
}

beforeEach(() => {
  vi.stubGlobal('matchMedia', vi.fn().mockImplementation(() => ({
    matches: false, media: '', addEventListener: vi.fn(), removeEventListener: vi.fn(),
  })))
})

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

function renderShell() {
  return renderRoute(<ThemeProvider><AppShell /></ThemeProvider>)
}

describe('the shell', () => {
  it('names the device and the release it is running', async () => {
    stubFetch(routes)
    renderShell()

    expect((await screen.findAllByText('mica-cm4')).length).toBeGreaterThan(0)
    expect(await screen.findByText('2026.08.2')).toBeTruthy()
    expect(await screen.findByText('9e12aa77bb33')).toBeTruthy()
  })

  /// The language picker used to be a dialog rendered inside the settings
  /// menu, which left the menu open and painted over its own backdrop. It is
  /// its own header control now, so it is reachable without opening anything.
  /// That the menu closes is overlay behaviour and the Playwright suite is what
  /// sees it; this asserts the structure that makes the overlap impossible.
  it('puts the language control outside the settings menu', async () => {
    stubFetch(routes)
    renderShell()

    expect(await screen.findByRole('combobox', { name: 'Language' })).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Display settings' })).toBeTruthy()
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('says the device stopped answering instead of showing stale numbers as live', async () => {
    stubFetch({ ...routes, '/api/v1/health': () => new Response(null, { status: 503 }) })
    renderShell()

    await waitFor(() => expect(screen.getAllByText('Offline').length).toBeGreaterThan(0))
  })

  it('reaches every primary destination from the navigation', async () => {
    stubFetch(routes)
    renderShell()

    const nav = await screen.findByRole('navigation', { name: 'Primary navigation' })
    const destinations = ['Overview', 'Network', 'Services', 'Applications', 'Access', 'System']
    for (const name of destinations) {
      expect(within(nav).getByRole('link', { name })).toBeTruthy()
    }
  })
})

describe('the shell controls', () => {
  it('refetches everything but the session when the operator asks', async () => {
    const fetch = stubFetch(routes)
    renderShell()

    await screen.findByText('2026.08.2')
    const before = fetch.mock.calls.length
    await userEvent.click(screen.getByRole('button', { name: 'Refresh all data' }))

    await waitFor(() => expect(fetch.mock.calls.length).toBeGreaterThan(before))
    expect(fetch.mock.calls.some(([url]) => String(url) === '/api/v1/session')).toBe(false)
  })

  // Signing out and the mobile drawer are overlay behaviour: base-ui keeps the
  // popup inert while it animates, which jsdom never finishes. The Playwright
  // suite drives both against a real browser.

})
