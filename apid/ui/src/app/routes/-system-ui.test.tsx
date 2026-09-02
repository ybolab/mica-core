import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { cleanup, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { I18nextProvider } from 'react-i18next'
import { i18n } from '@/i18n/i18n'
import { UiManagementPage } from '@/features/ui-management/ui-management-page'

const versions = {
  activeGeneration: 2,
  retentionLimit: 32,
  bundles: [
    {
      generation: 2,
      name: 'Active console',
      version: '2.0.0',
      indexReadable: true,
      digestMatches: true,
      compatible: true,
      usable: true,
      digest: 'a'.repeat(64),
      compressedBytes: 1024,
      expandedBytes: 2048,
    },
    {
      generation: 1,
      name: 'UI package one',
      version: '1.0.0',
      indexReadable: true,
      digestMatches: true,
      compatible: true,
      usable: true,
      digest: 'b'.repeat(64),
      compressedBytes: 512,
      expandedBytes: 1024,
    },
  ],
}

function renderPage() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <UiManagementPage />
      </QueryClientProvider>
    </I18nextProvider>,
  )
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('custom UI version manager', () => {
  it('protects the active version and confirms deletion of an inactive one', async () => {
    const fetch = vi.fn().mockImplementation((path: string, init?: RequestInit) => {
      if (init?.method === 'DELETE') return Promise.resolve(new Response(null, { status: 204 }))
      return Promise.resolve(new Response(JSON.stringify(versions), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }))
    })
    vi.stubGlobal('fetch', fetch)
    renderPage()

    expect(await screen.findByText('UI package one')).toBeTruthy()
    expect((screen.getByRole('button', { name: 'Delete generation 2' }) as HTMLButtonElement).disabled).toBe(true)

    await userEvent.click(screen.getByRole('button', { name: 'Delete generation 1' }))
    expect(await screen.findByText('Delete UI generation 1? This cannot be undone.')).toBeTruthy()
    await userEvent.click(screen.getByRole('button', { name: 'Delete' }))

    await waitFor(() => expect(fetch).toHaveBeenCalledWith(
      '/api/v1/ui/bundles/1',
      expect.objectContaining({ method: 'DELETE' }),
    ))
  })
})
