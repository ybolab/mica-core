import { cleanup, screen } from '@testing-library/react'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { jsonResponse, renderPanel, stubFetch } from '@/shared/testing/panel'
import { ProvisioningPanel, type ProvisioningStatus } from './provisioning-panel'

afterEach(() => {
  cleanup()
  vi.unstubAllGlobals()
})

describe('the provisioning document status', () => {
  it('reports the applied version, its digest and the import that applied it', async () => {
    stubFetch({
      '/api/v1/provisioning/status': {
        documentVersion: 1,
        documentDigest: 'sha256:c0ffee',
        lastImport: { source: 'media', outcome: 'applied', at: 1_756_000_000 },
        unclaimed: false,
      } satisfies ProvisioningStatus,
    })
    renderPanel(<ProvisioningPanel />)

    expect(await screen.findByText('configured by a provisioning document')).toBeTruthy()
    expect(screen.getByText('sha256:c0ffee')).toBeTruthy()
    expect(screen.getByText('a removable medium')).toBeTruthy()
    expect(screen.getByText('applied')).toBeTruthy()
  })

  it('keeps the applied version visible when the last document was rejected', async () => {
    stubFetch({
      '/api/v1/provisioning/status': {
        documentVersion: 1,
        documentDigest: 'sha256:c0ffee',
        lastImport: { source: 'boot', outcome: 'rejected', reason: 'unsupported document version', at: 1_756_000_100 },
        unclaimed: false,
      } satisfies ProvisioningStatus,
    })
    renderPanel(<ProvisioningPanel />)

    expect(await screen.findByText(/rejected: unsupported document version/)).toBeTruthy()
    expect(screen.getByText(/applied version and digest are unchanged/)).toBeTruthy()
    expect(screen.getByText('sha256:c0ffee')).toBeTruthy()
  })

  it('says a claimed device refuses a document offered on a medium', async () => {
    stubFetch({ '/api/v1/provisioning/status': { documentVersion: null, documentDigest: null, lastImport: null, unclaimed: false } satisfies ProvisioningStatus })
    renderPanel(<ProvisioningPanel />)

    expect(await screen.findByText('no provisioning document has been applied')).toBeTruthy()
    expect(screen.getByText('No medium has ever been offered to this device.')).toBeTruthy()
    expect(screen.getByText(/a document offered on a medium is refused/)).toBeTruthy()
  })

  it('says an unclaimed device would apply a document offered at the next boot', async () => {
    stubFetch({ '/api/v1/provisioning/status': { lastImport: null, unclaimed: true } satisfies ProvisioningStatus })
    renderPanel(<ProvisioningPanel />)

    expect(await screen.findByText(/a document offered at the next boot would be applied/)).toBeTruthy()
  })

  it('surfaces a read failure as an error', async () => {
    stubFetch({ '/api/v1/provisioning/status': () => jsonResponse({ error: { message: 'micad failed to answer' } }, 500) })
    renderPanel(<ProvisioningPanel />)

    expect(await screen.findByText('micad failed to answer')).toBeTruthy()
  })
})
