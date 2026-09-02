import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { AppProviders } from '@/app/providers'
import { initializeI18n } from '@/i18n/i18n'
import { initializeTheme } from '@/theme/theme'
import './styles.css'

initializeTheme()

async function bootstrap() {
  await initializeI18n()
  createRoot(document.getElementById('root')!).render(
    <StrictMode>
      <AppProviders />
    </StrictMode>,
  )
}

void bootstrap()
