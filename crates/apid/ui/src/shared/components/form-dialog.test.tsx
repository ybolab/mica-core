import { cleanup, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderPanel } from '@/shared/testing/panel'
import { Input } from '@/shared/components/ui/input'
import { FormField } from './form-field'
import { FormDialog } from './form-dialog'

afterEach(cleanup)

function open(onSubmit: () => Promise<unknown> | unknown) {
  renderPanel(
    <FormDialog
      open
      onOpenChange={() => {}}
      title="Add Wi-Fi network"
      submitLabel="Add"
      success="Network saved."
      failure="The network could not be saved."
      onSubmit={onSubmit}
    >
      <FormField label="SSID">{(id) => <Input id={id} defaultValue="workshop" />}</FormField>
    </FormDialog>,
  )
}

describe('the form dialog', () => {
  it('reports the write and lets the caller close', async () => {
    const submit = vi.fn(() => Promise.resolve())
    open(submit)

    await userEvent.click(screen.getByRole('button', { name: 'Add' }))

    await waitFor(() => expect(submit).toHaveBeenCalledTimes(1))
    expect(await screen.findByText('Network saved.')).toBeTruthy()
  })

  it('keeps the form up on a refusal, with the value the device named', async () => {
    // The operator's next move is to correct the field that was refused, so
    // the dialog and its input have to still be there.
    open(() => Promise.reject(new Error('ssid must not be empty')))

    await userEvent.click(screen.getByRole('button', { name: 'Add' }))

    expect(await screen.findByText('ssid must not be empty')).toBeTruthy()
    expect(screen.getByRole('textbox', { name: 'SSID' })).toBeTruthy()
  })

  it('submits once while a write is in flight', async () => {
    let release: () => void = () => {}
    const submit = vi.fn(() => new Promise<void>((resolve) => { release = resolve }))
    open(submit)

    const button = screen.getByRole('button', { name: 'Add' })
    await userEvent.click(button)
    await userEvent.click(button, { pointerEventsCheck: 0 })

    expect(submit).toHaveBeenCalledTimes(1)
    release()
  })
})
