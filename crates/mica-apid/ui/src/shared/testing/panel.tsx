import type { ReactNode } from 'react'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { render } from '@testing-library/react'
import { I18nextProvider } from 'react-i18next'
import {
  RouterProvider,
  createMemoryHistory,
  createRootRoute,
  createRouter,
} from '@tanstack/react-router'
import { vi } from 'vitest'
import { i18n } from '@/i18n/i18n'
import { Toaster } from '@/shared/components/ui/toast'

/// Panels are rendered with the feedback channel mounted, because the outcome
/// of a write is now reported through it. A harness without the toaster would
/// let a silent write pass its own test.
export function renderPanel(node: ReactNode) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <Toaster>{node}</Toaster>
      </QueryClientProvider>
    </I18nextProvider>,
  )
}

/// The same providers plus a memory router, for a page that renders a `Link`.
/// The route tree is one root: these tests assert what the page renders, not
/// where its links go, and the real tree is generated.
export function renderRoute(node: ReactNode, path = '/') {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  })
  const rootRoute = createRootRoute({
    component: () => (
      <I18nextProvider i18n={i18n}>
        <QueryClientProvider client={queryClient}>
          <Toaster>{node}</Toaster>
        </QueryClientProvider>
      </I18nextProvider>
    ),
  })
  const router = createRouter({
    routeTree: rootRoute,
    history: createMemoryHistory({ initialEntries: [path] }),
  })
  return render(<RouterProvider router={router as never} />)
}

export function jsonResponse(value: unknown, status = 200) {
  return new Response(status === 204 ? null : JSON.stringify(value), {
    status,
    headers: { 'content-type': 'application/json' },
  })
}

type Route = unknown | (() => Response)

/// Routes a stubbed fetch by `"METHOD /path"`, falling back to `"/path"`, so a
/// panel that reads several endpoints can be given a distinct answer for each.
/// An unstubbed call answers 500 rather than silently resolving, because a
/// panel reaching an endpoint the test did not name is itself a failure.
export function stubFetch(routes: Record<string, Route>) {
  const fetch = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const path = String(input)
    const method = (init?.method ?? 'GET').toUpperCase()
    const route = routes[`${method} ${path}`] ?? routes[path]
    if (route === undefined) {
      return Promise.resolve(jsonResponse({ error: { message: `no stub for ${method} ${path}` } }, 500))
    }
    return Promise.resolve(typeof route === 'function' ? (route as () => Response)() : jsonResponse(route))
  })
  vi.stubGlobal('fetch', fetch)
  return fetch
}
