import { afterEach, describe, expect, it, vi } from 'vitest'
import { api, ApiError, errorMessage, json, rememberSession, uploadZip } from './http'

afterEach(() => {
  vi.unstubAllGlobals()
  rememberSession({ state: 'unauthenticated' })
})

describe('api client', () => {
  it('discards the stale CSRF token after an authentication failure', async () => {
    rememberSession({ state: 'authenticated', csrfToken: 'expired-token' })
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response('{"error":{"code":"not_authenticated"}}', { status: 401 }))
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
    vi.stubGlobal('fetch', fetch)
    await expect(api('/api/v1/health')).rejects.toMatchObject({ status: 401 })
    await api('/api/v1/session', json('POST', { password: 'new-password' }))
    expect(new Headers(fetch.mock.calls[1][1]?.headers).has('x-csrf-token')).toBe(false)
  })

  it('adds the session CSRF token only to mutations', async () => {
    rememberSession({ state: 'authenticated', csrfToken: 'csrf-value' })
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response('{"ok":true}', { status: 200 }))
      .mockResolvedValueOnce(new Response(null, { status: 204 }))
    vi.stubGlobal('fetch', fetch)

    await api('/api/v1/meta')
    await api('/api/v1/session', { method: 'DELETE' })

    expect(((fetch.mock.calls[0][1] as RequestInit).headers as Headers).has('x-csrf-token')).toBe(false)
    expect(((fetch.mock.calls[1][1] as RequestInit).headers as Headers).get('x-csrf-token')).toBe('csrf-value')
  })

  it('turns the API error envelope into a typed error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(
      JSON.stringify({ error: { code: 'csrf_invalid', message: 'CSRF rejected', path: 'network' } }),
      { status: 403, headers: { 'content-type': 'application/json' } },
    )))
    await expect(api('/api/v1/network', { method: 'PUT' })).rejects.toEqual(
      new ApiError('CSRF rejected', 403, 'csrf_invalid', 'network'),
    )
  })

  it('handles empty responses, plain proxy errors, and request helpers', async () => {
    const fetch = vi.fn()
      .mockResolvedValueOnce(new Response('', { status: 200 }))
      .mockResolvedValueOnce(new Response('not json', { status: 502, statusText: 'Bad Gateway' }))
    vi.stubGlobal('fetch', fetch)

    await expect(api('/api/v1/empty')).resolves.toBeUndefined()
    await expect(api('/api/v1/proxy')).rejects.toEqual(new ApiError('502 Bad Gateway', 502))
    expect(json('PUT', { enabled: true })).toEqual({ method: 'PUT', body: '{"enabled":true}' })
    expect(json('DELETE')).toEqual({ method: 'DELETE', body: undefined })
    expect(errorMessage(new Error('specific'), 'fallback')).toBe('specific')
    expect(errorMessage('unknown', 'fallback')).toBe('fallback')
  })

  it('uploads a ZIP with credentials, CSRF, and progress reporting', async () => {
    rememberSession({ state: 'authenticated', csrfToken: 'csrf-upload' })
    const requests: MockXmlHttpRequest[] = []
    vi.stubGlobal('XMLHttpRequest', class extends MockXmlHttpRequest {
      constructor() {
        super()
        requests.push(this)
      }
    })
    const progress = vi.fn()
    const promise = uploadZip<{ bundleId: string }>('/api/v1/ui/bundles', new File(['zip'], 'ui.zip'), progress)
    const request = requests[0]

    expect(request.method).toBe('POST')
    expect(request.path).toBe('/api/v1/ui/bundles')
    expect(request.withCredentials).toBe(true)
    expect(request.headers).toMatchObject({
      'content-type': 'application/zip',
      'x-csrf-token': 'csrf-upload',
    })
    request.emitUpload('progress', { lengthComputable: true, loaded: 2, total: 4 })
    request.respond(201, '{"bundleId":"bundle-1"}')

    await expect(promise).resolves.toEqual({ bundleId: 'bundle-1' })
    expect(progress.mock.calls).toEqual([[50], [100]])
  })

  it('reports upload protocol, connection, and cancellation failures', async () => {
    const requests: MockXmlHttpRequest[] = []
    vi.stubGlobal('XMLHttpRequest', class extends MockXmlHttpRequest {
      constructor() {
        super()
        requests.push(this)
      }
    })
    const file = new File(['zip'], 'ui.zip')

    const rejected = uploadZip('/upload', file, vi.fn())
    requests[0].respond(422, '{"error":{"code":"invalid","message":"Invalid bundle","path":"bundle"}}')
    await expect(rejected).rejects.toEqual(new ApiError('Invalid bundle', 422, 'invalid', 'bundle'))

    const malformed = uploadZip('/upload', file, vi.fn())
    requests[1].respond(500, '{broken')
    await expect(malformed).rejects.toEqual(new ApiError('500 Server Error', 500))

    const disconnected = uploadZip('/upload', file, vi.fn())
    requests[2].emit('error')
    await expect(disconnected).rejects.toEqual(new ApiError('The upload connection failed.', 0, 'ui_upload_interrupted'))

    const cancelled = uploadZip('/upload', file, vi.fn())
    requests[3].emit('abort')
    await expect(cancelled).rejects.toEqual(new ApiError('The upload was cancelled.', 0, 'ui_upload_interrupted'))
  })

  it('discards the stale CSRF token when an upload returns a non-JSON 401', async () => {
    rememberSession({ state: 'authenticated', csrfToken: 'expired-token' })
    const requests: MockXmlHttpRequest[] = []
    vi.stubGlobal('XMLHttpRequest', class extends MockXmlHttpRequest {
      constructor() { super(); requests.push(this) }
    })
    const upload = uploadZip('/upload', new File(['zip'], 'ui.zip'), vi.fn())
    requests[0].respond(401, 'Authentication required')
    await expect(upload).rejects.toMatchObject({ status: 401 })
    const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }))
    vi.stubGlobal('fetch', fetch)
    await api('/api/v1/session', json('POST', { password: 'new-password' }))
    expect(new Headers(fetch.mock.calls[0][1]?.headers).has('x-csrf-token')).toBe(false)
  })
})

type Listener = (event: ProgressEvent) => void

class MockXmlHttpRequest {
  method = ''
  path = ''
  withCredentials = false
  status = 0
  statusText = ''
  responseText = ''
  headers: Record<string, string> = {}
  private listeners = new Map<string, Listener>()
  private uploadListeners = new Map<string, Listener>()
  upload = {
    addEventListener: (type: string, listener: Listener) => this.uploadListeners.set(type, listener),
  }

  open(method: string, path: string) {
    this.method = method
    this.path = path
  }

  setRequestHeader(name: string, value: string) {
    this.headers[name] = value
  }

  addEventListener(type: string, listener: Listener) {
    this.listeners.set(type, listener)
  }

  send() {}

  emit(type: string, event = {} as ProgressEvent) {
    this.listeners.get(type)?.(event)
  }

  emitUpload(type: string, event: Partial<ProgressEvent>) {
    this.uploadListeners.get(type)?.(event as ProgressEvent)
  }

  respond(status: number, responseText: string) {
    this.status = status
    this.statusText = status === 500 ? 'Server Error' : ''
    this.responseText = responseText
    this.emit('load')
  }
}
