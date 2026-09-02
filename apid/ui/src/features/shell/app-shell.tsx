import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import { useIsFetching, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { CircleAlert, Cpu, Key, LayoutGrid, LogOut, Menu, Network, Package, RefreshCw, Server, Settings2, X } from 'lucide-react'
import { useState } from 'react'
import { api, rememberSession } from '@/shared/lib/http'
import { sessionKey } from '@/components/auth'
import { LanguageControl, ThemeControl } from '@/components/preferences'
import { Button } from '@/shared/components/ui/button'
import { DropdownMenu, DropdownMenuContent, DropdownMenuTrigger } from '@/shared/components/ui/dropdown-menu'
import { Sheet, SheetClose, SheetContent, SheetHeader, SheetTitle, SheetTrigger } from '@/shared/components/ui/sheet'
import type { Health, SystemInformation } from '@/lib/types'
import { RotationNotice } from '@/features/onboarding/rotation-notice'
import { connectionState, freshnessLabel } from './connection'

const nav = [
  { to: '/' as const, label: 'shell.nav.overview' as const, icon: LayoutGrid },
  { to: '/network' as const, label: 'shell.nav.network' as const, icon: Network },
  { to: '/services' as const, label: 'shell.nav.services' as const, icon: Server },
  { to: '/applications' as const, label: 'shell.nav.applications' as const, icon: Package },
  { to: '/access' as const, label: 'shell.nav.access' as const, icon: Key },
  { to: '/system' as const, label: 'shell.nav.system' as const, icon: Cpu },
]

export function AppShell() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [menuOpen, setMenuOpen] = useState(false)
  const path = useRouterState({ select: (state) => state.location.pathname })
  const fetching = useIsFetching() > 0
  const health = useQuery({ queryKey: ['shell-health'], queryFn: () => api<Health>('/api/v1/health'), refetchInterval: 15_000 })
  const information = useQuery({ queryKey: ['system-information'], queryFn: () => api<SystemInformation>('/api/v1/system/info'), retry: false })
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

  const state = connectionState({ isError: health.isError, failureCount: health.failureCount, mosd: health.data?.mosd })
  const freshness = fetching ? t('shell.refreshing') : freshnessLabel(health.dataUpdatedAt, t)
  const release = information.data?.release
  const releaseLabel = release?.available ? release.imageVersion ?? release.versionId ?? release.name ?? '—' : '—'
  const slot = information.data?.slot
  const slotLabel = slot?.available ? slot.booted ?? '—' : '—'

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
              <RefreshCw className={fetching ? 'animate-spin' : undefined} aria-hidden="true" />
            </Button>
            <DropdownMenu>
              <DropdownMenuTrigger render={<Button variant="outline" size="icon" aria-label={t('shell.settings')} title={t('shell.settings')} />}>
                <Settings2 aria-hidden="true" />
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="settings-menu">
                <LanguageControl inMenu />
                <div className="menu-divider" />
                <span className="menu-label">{t('preferences.appearance')}</span>
                <ThemeControl />
                <div className="menu-divider" />
                <button type="button" className="menu-item" onClick={() => logout.mutate()} disabled={logout.isPending}>
                  <span>{t('shell.signOut')}</span><LogOut aria-hidden="true" />
                </button>
              </DropdownMenuContent>
            </DropdownMenu>
            <Sheet open={menuOpen} onOpenChange={setMenuOpen}>
              <SheetTrigger render={<Button className="mobile-menu-button" variant="outline" size="icon" aria-label={t('shell.menu')} />}>
                <Menu aria-hidden="true" />
              </SheetTrigger>
              <SheetContent side="right" showCloseButton={false} className="mobile-sheet">
                <SheetHeader className="mobile-sheet-header">
                  <SheetTitle className="sr-only">{t('shell.navigationLabel')}</SheetTitle>
                  <div className="drawer-brand"><span className="logo-mark">m</span><span><strong>{t('shell.product')}</strong><small>{hostname.data ?? 'mos'}</small></span></div>
                  <SheetClose render={<Button variant="ghost" size="icon" aria-label={t('common.actions.close')} />}><X /></SheetClose>
                </SheetHeader>
                <nav className="mobile-nav" aria-label={t('shell.navigationLabel')}>
                  {nav.map((item) => <NavItem key={item.to} {...item} active={isActive(path, item.to)} onClick={() => setMenuOpen(false)} />)}
                </nav>
                <div className="drawer-status">
                  <div className="connection-state"><span className={`connection-dot connection-${state}`} /><strong>{t(`shell.${state}`)}</strong><span>· {freshness}</span></div>
                  <div className="drawer-release"><span>{t('shell.release')}</span><code>{releaseLabel}</code></div>
                </div>
              </SheetContent>
            </Sheet>
          </div>
        </div>
      </header>
      {state === 'connected' ? null : (
        <div className={`shell-banner shell-banner-${state}`} role="status">
          <CircleAlert aria-hidden="true" />
          <div><strong>{t(`shell.banner.${state}`)}</strong><span>{t(`shell.banner.${state}Copy`)}</span></div>
        </div>
      )}
      <main className="app-main"><RotationNotice /><Outlet /></main>
      <footer className="app-footer">
        <div className="footer-row">
          <div className="connection-state">
            <span className={`connection-dot connection-${state}`} />
            <strong>{t(`shell.${state}`)}</strong>
            <span className="footer-detail">· {t(`shell.${state}Detail`)}</span>
            <span className="footer-detail">· {freshness}</span>
          </div>
          <div className="release-state">
            <span>{t('shell.release')} <code>{releaseLabel}</code></span>
            <span>{t('shell.slot')} <code>{slotLabel}</code></span>
          </div>
        </div>
      </footer>
    </div>
  )
}

function NavItem({ to, label, icon: Icon, active, onClick }: typeof nav[number] & { active: boolean; onClick?: () => void }) {
  const { t } = useTranslation()
  const name = t(label)
  // The medium breakpoint hides the label and leaves only the icon, so the
  // accessible name has to come from the item itself rather than its text.
  return (
    <Link to={to} className="shell-nav-item" data-active={active || undefined} aria-current={active ? 'page' : undefined} aria-label={name} title={name} onClick={onClick}>
      <Icon aria-hidden="true" />
      <span className="nav-label">{name}</span>
    </Link>
  )
}

function isActive(path: string, to: typeof nav[number]['to']) {
  const normalized = path.replace(/^\/_ui/, '') || '/'
  return to === '/' ? normalized === '/' : normalized.startsWith(to)
}
