import { createRootRoute } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { api, rememberSession, type SessionStatus } from '@/lib/api'
import { AppShell } from '@/features/shell/app-shell'
import { LoginView, sessionKey, SetupView } from '@/components/auth'

function RootComponent() {
  const { t } = useTranslation()
  const session = useQuery({
    queryKey: sessionKey,
    queryFn: async () => {
      const result = await api<SessionStatus>('/api/v1/session')
      rememberSession(result)
      return result
    },
    retry: 1,
  })
  if (session.isPending) return <div className="boot-screen"><span className="spinner" /><p>{t('root.connecting')}</p></div>
  if (session.error) return <div className="boot-screen"><p className="callout error">{t('root.unavailable')}</p></div>
  if (session.data.state === 'setup') return <SetupView />
  if (session.data.state !== 'authenticated') return <LoginView />
  return <AppShell />
}

export const Route = createRootRoute({ component: RootComponent })
