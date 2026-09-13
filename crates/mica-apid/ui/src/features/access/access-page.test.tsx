import { cleanup, screen, waitFor, within } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { AccessPage } from './access-page'

const routes = {
  '/api/v1/tokens': [{ id: 'tok_01HX', name: 'fleet agent', created: 1_756_000_000 }],
  '/api/v1/settings/access.ssh.enabled': true,
  '/api/v1/ssh/authorized-keys': { keys: [{ key: 'ssh-ed25519 AAAA', comment: 'roy@laptop', fingerprint: 'SHA256:abcd' }], notice: 'Every authorized key grants root.' },
  '/api/v1/claim': { state: 'claimed', via: 'setup', at: 1_756_000_000, rotationRequired: false },
  '/api/v1/provisioning/status': { documentVersion: 3, unclaimed: false },
}

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the access page', () => {
  it('closes the revoke confirmation and reports which token went', async () => {
    const fetch = stubFetch({
      ...routes,
      'DELETE /api/v1/tokens/tok_01HX': () => new Response(null, { status: 204 }),
    })
    renderPanel(<AccessPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Revoke fleet agent' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Revoke' }))

    await waitFor(() => expect(screen.queryByRole('alertdialog')).toBeNull())
    expect((await screen.findAllByText('Token fleet agent revoked.')).length).toBeGreaterThan(0)
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'DELETE')).toBe(true)
  })

  it('names the action that failed rather than pooling refusals into one line', async () => {
    // Four mutations used to share a single callout, so a failed revoke and a
    // failed key removal were the same sentence in the same place.
    stubFetch({
      ...routes,
      'DELETE /api/v1/tokens/tok_01HX': () => jsonResponse({ error: { code: 'rotation_required', message: 'this device still holds its bootstrap credential' } }, 409),
    })
    renderPanel(<AccessPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Revoke fleet agent' }))
    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Revoke' }))

    expect((await screen.findAllByText('this device still holds its bootstrap credential')).length).toBeGreaterThan(0)
    expect(screen.queryByRole('alertdialog')).not.toBeNull()
  })

  it('reports the SSH server switch, which used to change nothing visible', async () => {
    stubFetch({ ...routes, 'PUT /api/v1/settings/access.ssh.enabled': () => jsonResponse({ taskId: 'task-1' }, 202) })
    renderPanel(<AccessPage />)

    await userEvent.click(await screen.findByRole('switch', { name: 'SSH server' }))

    expect((await screen.findAllByText('SSH server disabled.')).length).toBeGreaterThan(0)
  })

  it('keeps the password mismatch beside the fields it describes', async () => {
    stubFetch(routes)
    renderPanel(<AccessPage />)

    await userEvent.type(await screen.findByLabelText('New admin password'), 'correct-horse')
    await userEvent.type(screen.getByLabelText('Confirm new password'), 'battery-staple')

    expect(await screen.findByText('Passwords do not match.')).toBeTruthy()
  })
})

describe('the access page forms', () => {
  it('mints a token, reveals it once and offers a copy control', async () => {
    const writeText = vi.fn(() => Promise.resolve())
    vi.stubGlobal('navigator', { ...navigator, clipboard: { writeText } })
    stubFetch({ ...routes, 'POST /api/v1/tokens': { id: 'tok_new', name: 'ci', created: 1_756_100_000, token: 'mica_tok_secret' } })
    renderPanel(<AccessPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Mint token' }))
    const dialog = await screen.findByRole('dialog')
    await userEvent.type(within(dialog).getByLabelText('New token label'), 'ci')
    await userEvent.click(within(dialog).getByRole('button', { name: 'Create token' }))

    expect(await screen.findByText('mica_tok_secret')).toBeTruthy()
    expect((await screen.findAllByText('Token created.')).length).toBeGreaterThan(0)
  })

  it('adds an authorized key through the shared form dialog', async () => {
    stubFetch({ ...routes, 'POST /api/v1/ssh/authorized-keys': {} })
    renderPanel(<AccessPage />)

    await userEvent.click(await screen.findByRole('button', { name: 'Add key' }))
    const dialog = await screen.findByRole('dialog')
    await userEvent.type(within(dialog).getByLabelText('Authorized key'), 'ssh-ed25519 AAAA operator')
    await userEvent.click(within(dialog).getByRole('button', { name: 'Add key' }))

    expect((await screen.findAllByText('Authorized key added.')).length).toBeGreaterThan(0)
  })

  it('confirms the transient root password before it is sent', async () => {
    const fetch = stubFetch({ ...routes, 'POST /api/v1/actions/transient-root-password': () => jsonResponse({ taskId: 'task-1' }, 202) })
    renderPanel(<AccessPage />)

    await userEvent.type(await screen.findByLabelText('Root password'), 'temporary-1')
    await userEvent.click(screen.getByRole('button', { name: 'Set until reboot' }))
    expect(fetch.mock.calls.some(([, init]) => (init as RequestInit | undefined)?.method === 'POST')).toBe(false)

    await userEvent.click(within(await screen.findByRole('alertdialog')).getByRole('button', { name: 'Set until reboot' }))
    expect((await screen.findAllByText('Transient root password accepted.')).length).toBeGreaterThan(0)
  })
})
