import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { renderPanel } from '@/shared/testing/panel'
import { Button } from '@/shared/components/ui/button'
import { ConfirmDialog } from './confirm-dialog'

afterEach(cleanup)

function open(onConfirm: () => Promise<unknown> | unknown) {
  renderPanel(
    <ConfirmDialog
      trigger={<Button>Revoke token</Button>}
      title="Revoke token"
      description="Existing automation using this token will stop working."
      confirmLabel="Revoke"
      success="Token revoked."
      failure="The token could not be revoked."
      onConfirm={onConfirm}
    />,
  )
}

describe('the confirmation dialog', () => {
  it('closes once the device has accepted, and says so', async () => {
    // The defect this component exists to close. Every one of the eleven
    // hand-built confirmations left the modal open over the page, so a
    // completed revoke and a lost click looked identical.
    const confirm = vi.fn(() => Promise.resolve())
    open(confirm)

    await userEvent.click(screen.getByRole('button', { name: 'Revoke token' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Revoke' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect(confirm).toHaveBeenCalledTimes(1)
    expect(await screen.findByText('Token revoked.')).toBeTruthy()
  })

  it('stays open when the device refuses, and reports the refusal in its own words', async () => {
    // A refusal is the one case where the operator's next move is to read and
    // decide again, so the dialog must not vanish out from under them.
    open(() => Promise.reject(new Error('this device was claimed with a bootstrap credential')))

    await userEvent.click(screen.getByRole('button', { name: 'Revoke token' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Revoke' }))

    expect(await screen.findByText('The token could not be revoked.')).toBeTruthy()
    expect(await screen.findByText(/claimed with a bootstrap credential/)).toBeTruthy()
    expect(screen.queryByRole('alertdialog')).not.toBeNull()
  })

  it('cancels without running the action', async () => {
    const confirm = vi.fn(() => Promise.resolve())
    open(confirm)

    await userEvent.click(screen.getByRole('button', { name: 'Revoke token' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Cancel' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect(confirm).not.toHaveBeenCalled()
  })

  it('refuses a second press while the first is still running', async () => {
    // Ten of the eleven old sites re-issued the request on every press, because
    // the dialog they left open kept its button live.
    let release: () => void = () => {}
    const confirm = vi.fn(() => new Promise<void>((resolve) => { release = resolve }))
    open(confirm)

    await userEvent.click(screen.getByRole('button', { name: 'Revoke token' }))
    const dialog = await screen.findByRole('alertdialog')
    const action = within(dialog).getByRole('button', { name: 'Revoke' })
    await userEvent.click(action)
    await userEvent.click(action, { pointerEventsCheck: 0 })
    await userEvent.click(action, { pointerEventsCheck: 0 })

    // Asserted on the effect rather than on the attribute: what matters is that
    // the device is asked once, however the control expresses being busy.
    expect(confirm).toHaveBeenCalledTimes(1)
    release()
    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
  })
})
