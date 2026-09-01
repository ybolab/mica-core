import { createRootRoute } from '@tanstack/react-router'
import { useQuery } from '@tanstack/react-query'
import { api, rememberSession, type SessionStatus } from '@/lib/api'
import { AppShell } from '@/components/app-shell'
import { LoginView, sessionKey, SetupView } from '@/components/auth'

function RootComponent() {
  const session = useQuery({
    queryKey: sessionKey,
    queryFn: async () => {
      const result = await api<SessionStatus>('/api/v1/session')
      rememberSession(result)
      return result
    },
    retry: 1,
  })
  if (session.isPending) return <div className="boot-screen"><span className="spinner" /><p>Connecting to the device…</p></div>
  if (session.error) return <div className="boot-screen"><p className="callout error">The management API is unavailable. Retry after mosd is ready.</p></div>
  if (session.data.state === 'setup') return <SetupView />
  if (session.data.state !== 'authenticated') return <LoginView />
  return <AppShell />
}

export const Route = createRootRoute({ component: RootComponent })
