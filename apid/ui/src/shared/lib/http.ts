export type SessionState = 'setup' | 'unauthenticated' | 'authenticated'

export interface SessionStatus {
  state: SessionState
  csrfToken?: string
}

interface ApiErrorBody {
  error?: { code?: string; message?: string; path?: string }
}

export class ApiError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code?: string,
    readonly path?: string,
  ) {
    super(message)
  }
}

let csrfToken: string | undefined

export function rememberSession(session: SessionStatus) {
  csrfToken = session.csrfToken
}

export async function api<T>(path: string, init: RequestInit = {}): Promise<T> {
  const method = (init.method ?? 'GET').toUpperCase()
  const headers = new Headers(init.headers)
  if (init.body !== undefined && !headers.has('content-type')) headers.set('content-type', 'application/json')
  if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && csrfToken) headers.set('x-csrf-token', csrfToken)

  const response = await fetch(path, { ...init, headers, credentials: 'same-origin' })
  if (response.status === 401) rememberSession({ state: 'unauthenticated' })
  if (!response.ok) {
    let body: ApiErrorBody = {}
    try {
      body = (await response.json()) as ApiErrorBody
    } catch {
      // The HTTP status remains useful when an intermediary returned no JSON.
    }
    throw new ApiError(
      body.error?.message ?? `${response.status} ${response.statusText}`,
      response.status,
      body.error?.code,
      body.error?.path,
    )
  }
  if (response.status === 204) return undefined as T
  const text = await response.text()
  return (text ? JSON.parse(text) : undefined) as T
}

export function json(method: string, body?: unknown): RequestInit {
  return { method, body: body === undefined ? undefined : JSON.stringify(body) }
}

export function errorMessage(error: unknown, fallback = 'The request could not be completed.') {
  return error instanceof Error ? error.message : fallback
}

export function uploadZip<T>(path: string, file: File, onProgress: (percent: number) => void): Promise<T> {
  return new Promise((resolve, reject) => {
    const request = new XMLHttpRequest()
    request.open('POST', path)
    request.withCredentials = true
    request.setRequestHeader('content-type', 'application/zip')
    if (csrfToken) request.setRequestHeader('x-csrf-token', csrfToken)
    request.upload.addEventListener('progress', (event) => {
      if (event.lengthComputable && event.total > 0) onProgress(Math.round((event.loaded / event.total) * 100))
    })
    request.addEventListener('load', () => {
      if (request.status === 401) rememberSession({ state: 'unauthenticated' })
      let body: ApiErrorBody & T
      try {
        body = request.responseText ? JSON.parse(request.responseText) as ApiErrorBody & T : {} as ApiErrorBody & T
      } catch {
        reject(new ApiError(`${request.status} ${request.statusText}`, request.status))
        return
      }
      if (request.status >= 200 && request.status < 300) {
        onProgress(100)
        resolve(body)
      } else {
        reject(new ApiError(body?.error?.message ?? `${request.status} ${request.statusText}`, request.status, body?.error?.code, body?.error?.path))
      }
    })
    request.addEventListener('error', () => reject(new ApiError('The upload connection failed.', 0, 'ui_upload_interrupted')))
    request.addEventListener('abort', () => reject(new ApiError('The upload was cancelled.', 0, 'ui_upload_interrupted')))
    request.send(file)
  })
}
