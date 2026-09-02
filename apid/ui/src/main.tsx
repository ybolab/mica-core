import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { RouterProvider } from '@tanstack/react-router'
import { AppProviders } from '@/app/providers'
import { createAppRouter } from '@/app/router'
import { initializeI18n } from '@/i18n/i18n'
import { initializeTheme } from '@/theme/theme'
import './styles.css'

initializeTheme()

async function bootstrap() {
  await initializeI18n()
  const root = document.getElementById('root')
  if (!root) throw new Error('Application root element was not found')
  const router = createAppRouter()
  createRoot(root).render(
    <StrictMode>
      <AppProviders><RouterProvider router={router} /></AppProviders>
    </StrictMode>,
  )
}

void bootstrap()
