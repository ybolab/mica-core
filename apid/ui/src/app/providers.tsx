import { I18nextProvider } from 'react-i18next'
import { QueryClientProvider } from '@tanstack/react-query'
import type { ReactNode } from 'react'
import { i18n } from '@/i18n/i18n'
import { ThemeProvider } from '@/theme/theme'
import { SimulationProvider } from '@/shared/simulation/simulation-provider'
import { Toaster } from '@/shared/components/ui/toast'
import { queryClient } from './query-client'

export function AppProviders({ children }: { children: ReactNode }) {
  return (
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        <ThemeProvider>
          <SimulationProvider>
            {/* One feedback surface for the whole console. It is mounted above
                the router so a toast raised by a write survives the navigation
                that write triggers. */}
            <Toaster>{children}</Toaster>
          </SimulationProvider>
        </ThemeProvider>
      </QueryClientProvider>
    </I18nextProvider>
  )
}
