import { cleanup, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderPanel } from '@/shared/testing/panel'
import { FilePicker } from './file-picker'

afterEach(cleanup)

function render(props: Partial<Parameters<typeof FilePicker>[0]> = {}) {
  const onSubmit = vi.fn()
  const view = renderPanel(
    <FilePicker
      label="mica UI package"
      accept=".zip"
      chooseLabel="Choose file"
      emptyLabel="No file chosen"
      submitLabel="Upload package"
      pendingLabel="Uploading…"
      onSubmit={onSubmit}
      {...props}
    />,
  )
  return { onSubmit, view }
}

describe('the file picker', () => {
  it('names the chosen file and only then offers the upload', async () => {
    // The browser's own control rendered "Choose File / No file chosen",
    // unstyled and untranslated, beside buttons from the design system.
    const { onSubmit, view } = render()

    expect(screen.getByText('No file chosen')).toBeTruthy()
    expect(screen.getByRole('button', { name: 'Upload package' }).hasAttribute('disabled')).toBe(true)

    const file = new File(['zip'], 'kiosk.mica-ui.zip', { type: 'application/zip' })
    // The real input is hidden and out of the tab order on purpose: the button
    // beside it is the control a user operates, so there is no accessible name
    // to query it by.
    await userEvent.upload(view.container.querySelector('input[type=file]')!, file)

    expect(await screen.findByText('kiosk.mica-ui.zip')).toBeTruthy()
    await userEvent.click(screen.getByRole('button', { name: 'Upload package' }))
    expect(onSubmit).toHaveBeenCalledWith(file)
  })

  it('shows the upload progress the transfer reports', () => {
    render({ pending: true, progress: 42 })

    expect(screen.getByRole('progressbar', { name: 'Uploading…' }).getAttribute('aria-valuenow')).toBe('42')
    expect(screen.getByRole('button', { name: 'Uploading…' })).toBeTruthy()
  })
})
