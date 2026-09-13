import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import type { UiStatus } from '@/lib/types'
import { i18n } from '@/i18n/i18n'
import { UiPanel, UpdateChecks, UpdatePanel } from '@/features/system/system-page'

function response(value: UiStatus) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

function renderPanel() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <UiPanel />
      </QueryClientProvider>
    </I18nextProvider>,
  )
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('custom UI selector', () => {
  it('disables the custom choice when no retained bundle exists', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ mode: 'builtIn' })))
    renderPanel()

    expect(await screen.findByText('No retained custom UI is installed.')).toBeTruthy()
    const selector = screen.getByRole('switch', { name: 'Use custom UI at root' })
    expect(selector.hasAttribute('data-disabled')).toBe(true)
  })

  it('explains why an installed bundle is not selectable', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({
      mode: 'builtIn',
      availableCustom: {
        generation: 4,
        indexReadable: true,
        usable: false,
        unavailableReason: 'missingActivationRecord',
      },
    })))
    renderPanel()

    expect(await screen.findByText(/activation record is missing/)).toBeTruthy()
    expect(screen.getByText('not verified')).toBeTruthy()
    expect(screen.getByRole('switch', { name: 'Use custom UI at root' }).hasAttribute('data-disabled')).toBe(true)
  })

  it('puts the active resource when a retained bundle is selected', async () => {
    const builtIn: UiStatus = {
      mode: 'builtIn',
      availableCustom: {
        generation: 7,
        indexReadable: true,
        digestMatches: true,
        compatible: true,
        usable: true,
        name: 'operator console',
        version: '2.0',
      },
    }
    const custom: UiStatus = {
      ...builtIn,
      mode: 'custom',
      custom: builtIn.availableCustom,
    }
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(response(builtIn))
      .mockResolvedValueOnce(response(custom))
    vi.stubGlobal('fetch', fetch)
    renderPanel()

    await userEvent.click(await screen.findByRole('switch', { name: 'Use custom UI at root' }))

    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(2))
    expect(fetch.mock.calls[1][0]).toBe('/api/v1/ui/active')
    expect((fetch.mock.calls[1][1] as RequestInit).method).toBe('PUT')
    expect((fetch.mock.calls[1][1] as RequestInit).body).toBe('{"generation":7}')
    expect((await screen.findByRole('link', { name: 'Open custom UI at root' })).getAttribute('href')).toBe('/')
  })

  it('deletes the active resource when built-in is selected', async () => {
    const candidate = {
      generation: 3,
      indexReadable: true,
      digestMatches: true,
      usable: true,
    }
    const active: UiStatus = {
      mode: 'custom',
      custom: candidate,
      availableCustom: candidate,
    }
    const builtIn: UiStatus = { mode: 'builtIn', availableCustom: candidate }
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(response(active))
      .mockResolvedValueOnce(response(builtIn))
    vi.stubGlobal('fetch', fetch)
    renderPanel()

    const selector = await screen.findByRole('switch', { name: 'Use custom UI at root' })
    await waitFor(() => expect(selector.getAttribute('aria-checked')).toBe('true'))
    await userEvent.click(selector)

    await waitFor(() => expect(fetch).toHaveBeenCalledTimes(2))
    expect((fetch.mock.calls[1][1] as RequestInit).method).toBe('DELETE')
  })
})

function renderUpdatePanel() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <UpdatePanel />
        <UpdateChecks />
      </QueryClientProvider>
    </I18nextProvider>,
  )
}

function jsonResponse(value: unknown) {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

describe('update status panel', () => {
  it('renders the lifecycle state, staged bundle and a closed reboot gate', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({
      lifecycle: {
        state: 'ready',
        reason: 'a verified bundle is staged for install',
        deploymentId: 'a'.repeat(64),
        client: { available: true },
        reboot_gate: { safe: false, reasons: ['health.exporter reports `blocking`: mid-transaction'] },
      },
    })))
    renderUpdatePanel()

    expect(await screen.findByText('ready')).toBeTruthy()
    expect(screen.getByText('a'.repeat(64))).toBeTruthy()
    expect(screen.getByText('Safe to reboot')).toBeTruthy()
    expect(screen.getByText('Blocked')).toBeTruthy()
    expect(screen.getByText(/mid-transaction/)).toBeTruthy()
  })

  it('shows an unready update workspace as its named state with the reason', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({
      lifecycle: {
        state: 'update-unavailable',
        reason: 'degraded read-only: /mica is mounted read-only (/dev/mmcblk0p7)',
        workspace: { root: '/mica/updates', status: 'degraded', kind: 'read-only' },
        client: { available: true },
        reboot_gate: { safe: true, reasons: [] },
      },
    })))
    renderUpdatePanel()

    expect(await screen.findByText('update-unavailable')).toBeTruthy()
    expect(screen.getByText('degraded read-only: /mica is mounted read-only (/dev/mmcblk0p7)')).toBeTruthy()
  })

  it('reports an absent update client instead of hiding it', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({
      lifecycle: {
        state: 'idle',
        client: { available: false, reason: '/usr/bin/mica-deploy is not present on this image' },
        reboot_gate: { safe: true, reasons: [] },
      },
    })))
    renderUpdatePanel()

    expect(await screen.findByText(/Update client unavailable/)).toBeTruthy()
    expect(screen.getByText('Safe to reboot.')).toBeTruthy()
  })
})
