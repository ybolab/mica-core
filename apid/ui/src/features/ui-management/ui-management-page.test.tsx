import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { UiManagementPage } from './ui-management-page'

const bundles = {
  activeGeneration: null,
  retentionLimit: 32,
  bundles: [
    { generation: 3, name: 'kiosk', version: '1.2.0', usable: true, compatible: true, digest: 'sha256:9e12aaff7712', expandedBytes: 5_242_880, compressedBytes: 1_048_576 },
    { generation: 2, name: 'kiosk', version: '1.1.0', usable: false, compatible: false, unavailableReason: 'digestMismatch', expandedBytes: 4_194_304, compressedBytes: 950_000 },
  ],
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the UI version manager', () => {
  /// The name, the version and the two sizes are each their own element. They
  /// always were; what the hand-rolled table got wrong was leaving them inline,
  /// so a row rendered as "kiosk1.2.0". That is a layout property and the
  /// Playwright baseline is what sees it — this asserts the structure the
  /// layout needs.
  it('gives the name, the version and each size its own element', async () => {
    stubFetch({ '/api/v1/ui/bundles': bundles })
    renderPanel(<UiManagementPage />)

    const row = (await screen.findAllByRole('row')).find((candidate) => candidate.textContent?.includes('1.2.0'))!
    expect(within(row).getByText('kiosk')).toBeTruthy()
    expect(within(row).getByText('1.2.0')).toBeTruthy()
    expect(within(row).getByText('5.0 MiB')).toBeTruthy()
    expect(within(row).getByText('1.0 MiB package')).toBeTruthy()
  })

  it('reports an activation, which used to change nothing visible', async () => {
    stubFetch({
      '/api/v1/ui/bundles': bundles,
      'PUT /api/v1/ui/active': { mode: 'custom' },
    })
    renderPanel(<UiManagementPage />)

    await userEvent.click((await screen.findAllByRole('button', { name: 'Activate' }))[0])

    expect((await screen.findAllByText('Version 3 is active.')).length).toBeGreaterThan(0)
  })

  it('closes the delete confirmation and names the version that went', async () => {
    stubFetch({
      '/api/v1/ui/bundles': bundles,
      'DELETE /api/v1/ui/bundles/3': () => new Response(null, { status: 204 }),
    })
    renderPanel(<UiManagementPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Delete generation 3' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Delete' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect((await screen.findAllByText('Version 3 deleted.')).length).toBeGreaterThan(0)
  })

  it('refuses to activate a bundle the device could not validate', async () => {
    stubFetch({ '/api/v1/ui/bundles': bundles })
    renderPanel(<UiManagementPage />)

    const buttons = await screen.findAllByRole('button', { name: 'Activate' })
    expect(buttons[1].hasAttribute('disabled') || buttons[1].getAttribute('aria-disabled') === 'true' || buttons[1].dataset.disabled !== undefined).toBe(true)
  })

  it('surfaces a refused read rather than rendering an empty table', async () => {
    stubFetch({ '/api/v1/ui/bundles': () => jsonResponse({ error: { message: 'the bundle directory is unreadable' } }, 500) })
    renderPanel(<UiManagementPage />)

    expect(await screen.findByText('the bundle directory is unreadable')).toBeTruthy()
  })
})
