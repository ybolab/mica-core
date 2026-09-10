import { createRootRoute } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { api, rememberSession, type SessionStatus } from '@/shared/lib/http'
import { AppShell } from '@/features/shell/app-shell'
import { LoginView, sessionKey, SetupView } from '@/features/auth/auth'
import { Callout } from '@/shared/components/callout'
import { Spinner } from '@/shared/components/ui/spinner'

function BootScreen({ children }: { children: React.ReactNode }) {
  return <div className="grid min-h-dvh place-content-center justify-items-center gap-4 p-6 text-sm text-muted-foreground">{children}</div>
}

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
  if (session.isPending) return <BootScreen><Spinner className="size-6" /><p>{t('root.connecting')}</p></BootScreen>
  if (session.error) return <BootScreen><Callout tone="danger" title={t('root.unavailable')} /></BootScreen>
  if (session.data.state === 'setup') return <SetupView />
  if (session.data.state !== 'authenticated') return <LoginView />
  return <AppShell />
}

export const Route = createRootRoute({ component: RootComponent })
