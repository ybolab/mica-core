import { I18nextProvider } from 'react-i18next'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { createRouter, RouterProvider } from '@tanstack/react-router'
import { i18n } from '@/i18n/i18n'
import { ThemeProvider } from '@/theme/theme'
import { routeTree } from '@/routeTree.gen'

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: 10_000, refetchOnWindowFocus: false } },
})

const router = createRouter({ routeTree, basepath: '/_ui' })

declare module '@tanstack/react-router' {
  interface Register { router: typeof router }
}

export function AppProviders() {
  return (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <ThemeProvider>
          <RouterProvider router={router} />
        </ThemeProvider>
      </QueryClientProvider>
    </I18nextProvider>
  )
}
