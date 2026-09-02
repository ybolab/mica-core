import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { ClaimPanel, type ClaimStatus } from './claim-panel'
import { RotationNotice } from './rotation-notice'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the device claim', () => {
  it('names the channel that claimed the device and the moment it committed, in UTC', async () => {
    stubFetch({ '/api/v1/claim': { state: 'claimed', via: 'setup', at: 1_756_000_000, rotationRequired: false } satisfies ClaimStatus })
    renderPanel(<ClaimPanel />)

    expect(await screen.findByText('claimed')).toBeTruthy()
    expect(screen.getByText('the setup wizard')).toBeTruthy()
    expect(screen.getByText(/UTC$/)).toBeTruthy()
  })

  it('says nothing about a rotation for a device claimed through setup', async () => {
    stubFetch({ '/api/v1/claim': { state: 'claimed', via: 'setup', rotationRequired: false } satisfies ClaimStatus })
    renderPanel(<ClaimPanel />)

    await screen.findByText('claimed')
    expect(screen.queryByText('This credential must be replaced')).toBeNull()
  })

  it('renders an unclaimed device without inventing a channel for it', async () => {
    stubFetch({ '/api/v1/claim': { state: 'unclaimed', rotationRequired: false } satisfies ClaimStatus })
    renderPanel(<ClaimPanel />)

    expect(await screen.findByText(/unclaimed/)).toBeTruthy()
    expect(screen.queryByText('Claimed through')).toBeNull()
    expect(screen.queryByText('Claimed at')).toBeNull()
  })

  it('explains a bootstrap claim: what happened, what it refuses, the way out, and that nothing counts down', async () => {
    stubFetch({ '/api/v1/claim': { state: 'claimed', via: 'provisioning-document', at: 1_756_000_000, rotationRequired: true } satisfies ClaimStatus })
    renderPanel(<ClaimPanel />)

    expect(await screen.findByText('This credential must be replaced')).toBeTruthy()
    expect(screen.getByText('a provisioning document')).toBeTruthy()
    expect(screen.getByText(/sat in plaintext on the medium/)).toBeTruthy()
    expect(screen.getByText(/serves every read and refuses every authenticated change/)).toBeTruthy()
    expect(screen.getByText(/Change the administrator password below/)).toBeTruthy()
    expect(screen.getByText(/Nothing counts down/)).toBeTruthy()
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/claim': () => jsonResponse({ error: { message: 'the access settings could not be read' } }, 500) })
    renderPanel(<ClaimPanel />)

    expect(await screen.findByText('the access settings could not be read')).toBeTruthy()
  })
})

describe('the shell rotation notice', () => {
  it('is silent unless the credential must be replaced', async () => {
    stubFetch({ '/api/v1/claim': { state: 'claimed', via: 'setup', rotationRequired: false } satisfies ClaimStatus })
    const { container } = renderPanel(<RotationNotice />)

    await vi.waitFor(() => expect(container.querySelector('.callout')).toBeNull())
  })

  it('announces the bound on every page and links to the pane that explains it', async () => {
    stubFetch({ '/api/v1/claim': { state: 'claimed', via: 'provisioning-document', rotationRequired: true } satisfies ClaimStatus })
    renderPanel(<RotationNotice />)

    expect(await screen.findByText(/every change is refused until it is replaced/)).toBeTruthy()
    expect(screen.getByText('Replace it')).toBeTruthy()
  })
})
