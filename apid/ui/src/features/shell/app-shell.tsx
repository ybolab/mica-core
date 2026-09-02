import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Activity, Boxes, Cable, KeyRound, LogOut, Menu, Package, RefreshCw, Settings2, X } from 'lucide-react'
import { useState } from 'react'
import { api, rememberSession } from '@/shared/lib/http'
import { sessionKey } from '@/components/auth'
import { Preferences } from '@/components/preferences'
import { Button } from '@/shared/components/ui/button'
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from '@/shared/components/ui/dropdown-menu'
import { Sheet, SheetClose, SheetContent, SheetHeader, SheetTitle, SheetTrigger } from '@/shared/components/ui/sheet'
import type { Meta } from '@/lib/types'
import { RotationNotice } from '@/features/onboarding/rotation-notice'

const nav = [
  { to: '/' as const, label: 'shell.nav.overview' as const, icon: Activity },
  { to: '/network' as const, label: 'shell.nav.network' as const, icon: Cable },
  { to: '/services' as const, label: 'shell.nav.services' as const, icon: Boxes },
  { to: '/applications' as const, label: 'shell.nav.applications' as const, icon: Package },
  { to: '/access' as const, label: 'shell.nav.access' as const, icon: KeyRound },
  { to: '/system' as const, label: 'shell.nav.system' as const, icon: Settings2 },
]

export function AppShell() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [menuOpen, setMenuOpen] = useState(false)
  const path = useRouterState({ select: (state) => state.location.pathname })
  const health = useQuery({ queryKey: ['shell-health'], queryFn: () => api<{ mosd: string }>('/api/v1/health'), refetchInterval: 15_000 })
  const meta = useQuery({ queryKey: ['meta'], queryFn: () => api<Meta>('/api/v1/meta') })
  const hostname = useQuery({ queryKey: ['settings', 'hostname'], queryFn: () => api<string>('/api/v1/settings/hostname') })
  const logout = useMutation({
    mutationFn: () => api<void>('/api/v1/session', { method: 'DELETE' }),
    onSuccess: () => {
      rememberSession({ state: 'unauthenticated' })
      queryClient.setQueryData(sessionKey, { state: 'unauthenticated' })
      queryClient.removeQueries({ predicate: (query) => query.queryKey[0] !== 'session' })
    },
  })
  const refresh = () => void queryClient.invalidateQueries({ predicate: (query) => query.queryKey[0] !== 'session' })
  const unavailable = health.isError || (health.data !== undefined && health.data.mosd !== 'ok')

  return (
    <div className="app-shell">
      <header className="app-header">
        <div className="header-row">
          <Link to="/" className="header-brand" aria-label={t('shell.homeLabel')}>
            <span className="logo-mark">m</span>
            <span><strong>{t('shell.product')}</strong><small>{hostname.data ?? 'mos'}</small></span>
          </Link>
          <nav className="desktop-nav" aria-label={t('shell.navigationLabel')}>
            {nav.map((item) => <NavItem key={item.to} {...item} active={isActive(path, item.to)} />)}
          </nav>
          <div className="header-actions">
            <Button variant="outline" size="icon" onClick={refresh} aria-label={t('shell.refresh')} title={t('shell.refresh')}>
              <RefreshCw aria-hidden="true" />
            </Button>
            <DropdownMenu>
              <DropdownMenuTrigger render={<Button variant="outline" size="icon" aria-label={t('shell.settings')} title={t('shell.settings')} />}>
                <Settings2 aria-hidden="true" />
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="settings-menu">
                <Preferences compact />
                <div className="menu-divider" />
                <Button variant="ghost" className="w-full justify-between" onClick={() => logout.mutate()} disabled={logout.isPending}>
                  {t('shell.signOut')}<LogOut aria-hidden="true" />
                </Button>
              </DropdownMenuContent>
            </DropdownMenu>
            <Sheet open={menuOpen} onOpenChange={setMenuOpen}>
              <SheetTrigger render={<Button className="mobile-menu-button" variant="outline" size="icon" aria-label={t('shell.menu')} />}>
                <Menu aria-hidden="true" />
              </SheetTrigger>
              <SheetContent side="right" showCloseButton={false} className="mobile-sheet">
                <SheetHeader className="mobile-sheet-header">
                  <SheetTitle>{t('shell.navigationLabel')}</SheetTitle>
                  <SheetClose render={<Button variant="ghost" size="icon" aria-label={t('common.actions.close')} />}><X /></SheetClose>
                </SheetHeader>
                <nav className="mobile-nav" aria-label={t('shell.navigationLabel')}>
                  {nav.map((item) => <NavItem key={item.to} {...item} active={isActive(path, item.to)} onClick={() => setMenuOpen(false)} />)}
                </nav>
              </SheetContent>
            </Sheet>
          </div>
        </div>
      </header>
      <main className="app-main"><RotationNotice /><Outlet /></main>
      <footer className="app-footer">
        <div className="footer-row">
          <div className="connection-state">
            <span className={unavailable ? 'connection-dot connection-offline' : health.isFetching ? 'connection-dot connection-pending' : 'connection-dot'} />
            <strong>{unavailable ? t('shell.offline') : health.isFetching ? t('shell.refreshing') : t('shell.connected')}</strong>
            <span className="footer-detail">· {unavailable ? t('shell.unavailable') : t('shell.fresh')}</span>
          </div>
          <div className="release-state"><span>{t('shell.api')}</span><code>{meta.data?.api ?? '—'}</code><span>{t('shell.schema')}</span><code>{meta.data?.settingsSchemaVersion ?? '—'}</code></div>
        </div>
      </footer>
    </div>
  )
}

function NavItem({ to, label, icon: Icon, active, onClick }: typeof nav[number] & { active: boolean; onClick?: () => void }) {
  const { t } = useTranslation()
  return (
    <Link to={to} className="shell-nav-item" data-active={active || undefined} aria-current={active ? 'page' : undefined} onClick={onClick}>
      <Icon aria-hidden="true" />
      <span className="nav-label">{t(label)}</span>
    </Link>
  )
}

function isActive(path: string, to: typeof nav[number]['to']) {
  const normalized = path.replace(/^\/_ui/, '') || '/'
  return to === '/' ? normalized === '/' : normalized.startsWith(to)
}
