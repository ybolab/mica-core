import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Activity, Boxes, Cable, Clock3, KeyRound, LogOut, Settings2 } from 'lucide-react'
import { api, rememberSession } from '@/lib/api'
import { sessionKey } from '@/components/auth'
import { Button } from '@/components/ui/button'
import { Preferences } from '@/components/preferences'

const nav = [
  { to: '/' as const, label: 'shell.nav.overview' as const, icon: Activity },
  { to: '/network' as const, label: 'shell.nav.network' as const, icon: Cable },
  { to: '/services' as const, label: 'shell.nav.services' as const, icon: Boxes },
  { to: '/access' as const, label: 'shell.nav.access' as const, icon: KeyRound },
  { to: '/time' as const, label: 'shell.nav.time' as const, icon: Clock3 },
  { to: '/system' as const, label: 'shell.nav.system' as const, icon: Settings2 },
]

export function AppShell() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const path = useRouterState({ select: (state) => state.location.pathname })
  const logout = useMutation({
    mutationFn: () => api<void>('/api/v1/session', { method: 'DELETE' }),
    onSuccess: () => {
      rememberSession({ state: 'unauthenticated' })
      queryClient.setQueryData(sessionKey, { state: 'unauthenticated' })
      queryClient.removeQueries({ predicate: (query) => query.queryKey[0] !== 'session' })
    },
  })
  return (
    <div className="app-grid">
      <aside className="sidebar">
        <Link to="/" className="brand" aria-label={t('shell.homeLabel')}>
          <span className="brand-mark">m</span>
          <span><strong>{t('shell.product')}</strong><small>{t('shell.subtitle')}</small></span>
        </Link>
        <nav aria-label={t('shell.navigationLabel')}>
          {nav.map(({ to, label, icon: Icon }) => {
            const active = to === '/' ? path === '/ui/' || path === '/ui' : path.startsWith(`/ui${to}`)
            return (
              <Link key={to} to={to} className="nav-link" data-active={active || undefined} aria-current={active ? 'page' : undefined}>
                <Icon className="size-[18px]" aria-hidden="true" /> {t(label)}
              </Link>
            )
          })}
        </nav>
        <div className="sidebar-foot">
          <p>{t('shell.builtInUi')}</p>
          <Preferences compact />
          <Button variant="ghost" className="w-full justify-start" onClick={() => logout.mutate()} disabled={logout.isPending}>
            <LogOut className="size-4" /> {t('shell.signOut')}
          </Button>
        </div>
      </aside>
      <main className="content"><Outlet /></main>
    </div>
  )
}
