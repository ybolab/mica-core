import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { Activity, Boxes, Cable, Clock3, KeyRound, LogOut, Settings2 } from 'lucide-react'
import { api, rememberSession } from '@/lib/api'
import { sessionKey } from '@/components/auth'
import { Button } from '@/components/ui/button'

const nav = [
  { to: '/' as const, label: 'Overview', icon: Activity },
  { to: '/network' as const, label: 'Network', icon: Cable },
  { to: '/services' as const, label: 'Services', icon: Boxes },
  { to: '/access' as const, label: 'Access', icon: KeyRound },
  { to: '/time' as const, label: 'Time', icon: Clock3 },
  { to: '/system' as const, label: 'System', icon: Settings2 },
]

export function AppShell() {
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
        <Link to="/" className="brand" aria-label="mos console home">
          <span className="brand-mark">m</span>
          <span><strong>mos</strong><small>device console</small></span>
        </Link>
        <nav aria-label="Primary navigation">
          {nav.map(({ to, label, icon: Icon }) => {
            const active = to === '/' ? path === '/ui/' || path === '/ui' : path.startsWith(`/ui${to}`)
            return (
              <Link key={to} to={to} className="nav-link" data-active={active || undefined}>
                <Icon className="size-[18px]" aria-hidden="true" /> {label}
              </Link>
            )
          })}
        </nav>
        <div className="sidebar-foot">
          <p>Built-in UI</p>
          <Button variant="ghost" className="w-full justify-start" onClick={() => logout.mutate()} disabled={logout.isPending}>
            <LogOut className="size-4" /> Sign out
          </Button>
        </div>
      </aside>
      <main className="content"><Outlet /></main>
    </div>
  )
}
