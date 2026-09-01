export type SessionState = 'setup' | 'unauthenticated' | 'authenticated'

export interface SessionStatus {
  state: SessionState
  csrfToken?: string
}

export interface ApiErrorBody {
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
  if (init.body !== undefined && !headers.has('content-type')) {
    headers.set('content-type', 'application/json')
  }
  if (!['GET', 'HEAD', 'OPTIONS'].includes(method) && csrfToken) {
    headers.set('x-csrf-token', csrfToken)
  }
  const response = await fetch(path, {
    ...init,
    headers,
    credentials: 'same-origin',
  })
  if (!response.ok) {
    let body: ApiErrorBody = {}
    try {
      body = (await response.json()) as ApiErrorBody
    } catch {
      // The status remains useful when an intermediary returned no JSON.
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

export function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : 'The request could not be completed.'
}
