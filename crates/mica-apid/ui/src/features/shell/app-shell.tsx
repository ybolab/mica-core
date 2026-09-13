import { Link, Outlet, useRouterState } from '@tanstack/react-router'
import { useIsFetching, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { CircleAlert, Cpu, Key, LayoutGrid, LogOut, Menu, Network, Package, RefreshCw, Server, Settings2, X } from 'lucide-react'
import { useState } from 'react'
import { api, rememberSession } from '@/shared/lib/http'
import { sessionKey } from '@/features/auth/auth'
import { LanguageControl, ThemeControl } from '@/features/preferences/preferences'
import { Button } from '@/shared/components/ui/button'
import { DropdownMenu, DropdownMenuContent, DropdownMenuGroup, DropdownMenuItem, DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger } from '@/shared/components/ui/dropdown-menu'
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

/// Controls that sit on the chrome bar. Declared once, at the call site, in
/// the same token vocabulary as everything else — the previous console pushed
/// this into the stylesheet as three `!important` rules keyed on the header's
/// descendants, which broke whenever the button's variant classes changed.
const CHROME_CONTROL = 'border-chrome-border bg-transparent text-chrome-foreground hover:bg-chrome-hover hover:text-chrome-foreground'

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

  const state = connectionState({ isError: health.isError, failureCount: health.failureCount, micad: health.data?.micad })
  const freshness = fetching ? t('shell.refreshing') : freshnessLabel(health.dataUpdatedAt, t)
  const release = information.data?.release
  const releaseLabel = release?.available ? release.imageVersion ?? release.versionId ?? release.name ?? '—' : '—'
  const deployment = information.data?.deployment
  const deploymentLabel = deployment?.available ? deployment.id?.slice(0, 12) ?? '—' : '—'

  return (
    <div className="flex min-h-dvh flex-col">
      <header className="sticky top-0 z-40 border-b border-chrome-border bg-chrome text-chrome-foreground">
        <div className="mx-auto flex h-14 w-full max-w-[1280px] items-center gap-2 px-4 sm:px-6 lg:px-8">
          <Link to="/" className="flex min-w-0 flex-1 items-center gap-2.5 text-inherit no-underline" aria-label={t('shell.homeLabel')}>
            <span className="logo-mark">m</span>
            <span className="flex min-w-0 flex-col leading-tight">
              <strong className="text-sm font-semibold">{t('shell.product')}</strong>
              <small className="truncate font-mono text-xs text-chrome-muted">{hostname.data ?? 'mica'}</small>
            </span>
          </Link>
          <nav className="hidden flex-none justify-center gap-0.5 md:flex" aria-label={t('shell.navigationLabel')}>
            {nav.map((item) => <NavItem key={item.to} {...item} active={isActive(path, item.to)} />)}
          </nav>
          <div className="flex min-w-0 flex-1 items-center justify-end gap-2">
            <Button variant="outline" size="icon" className={CHROME_CONTROL} onClick={refresh} aria-label={t('shell.refresh')} title={t('shell.refresh')}>
              <RefreshCw className={fetching ? 'animate-spin' : undefined} aria-hidden="true" />
            </Button>
            {/* The language picker is its own control rather than a dialog
                nested in the settings menu, which used to leave the menu open
                on top of its own backdrop. */}
            <LanguageControl className={`hidden w-40 lg:flex ${CHROME_CONTROL} [&_input]:text-chrome-foreground [&_input]:placeholder:text-chrome-muted`} />
            <DropdownMenu>
              <DropdownMenuTrigger render={<Button variant="outline" size="icon" className={CHROME_CONTROL} aria-label={t('shell.settings')} title={t('shell.settings')} />}>
                <Settings2 aria-hidden="true" />
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-64 p-2">
                {/* A label is a group's label: base-ui refuses one outside a
                    group, and the refusal takes the whole menu down. */}
                <DropdownMenuGroup>
                  <DropdownMenuLabel>{t('preferences.appearance')}</DropdownMenuLabel>
                  <ThemeControl />
                </DropdownMenuGroup>
                <DropdownMenuSeparator />
                <DropdownMenuGroup className="lg:hidden">
                  <DropdownMenuLabel>{t('preferences.language')}</DropdownMenuLabel>
                  <div className="px-1 pb-1"><LanguageControl className="w-full" /></div>
                  <DropdownMenuSeparator />
                </DropdownMenuGroup>
                <DropdownMenuItem disabled={logout.isPending} onClick={() => logout.mutate()}>
                  <LogOut aria-hidden="true" />{t('shell.signOut')}
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
            <Sheet open={menuOpen} onOpenChange={setMenuOpen}>
              <SheetTrigger render={<Button className={`${CHROME_CONTROL} md:hidden`} variant="outline" size="icon" aria-label={t('shell.menu')} />}>
                <Menu aria-hidden="true" />
              </SheetTrigger>
              <SheetContent side="right" showCloseButton={false} className="w-75 max-w-[84vw] gap-0 p-0">
                <SheetHeader className="flex-row items-center justify-between border-b p-3 pl-4">
                  <SheetTitle className="sr-only">{t('shell.navigationLabel')}</SheetTitle>
                  <div className="flex min-w-0 items-center gap-2.5">
                    <span className="logo-mark bg-primary text-primary-foreground">m</span>
                    <span className="flex min-w-0 flex-col leading-tight">
                      <strong className="text-sm font-semibold">{t('shell.product')}</strong>
                      <small className="truncate font-mono text-xs text-muted-foreground">{hostname.data ?? 'mica'}</small>
                    </span>
                  </div>
                  <SheetClose render={<Button variant="ghost" size="icon" aria-label={t('common.actions.close')} />}><X /></SheetClose>
                </SheetHeader>
                <nav className="grid gap-0.5 p-2" aria-label={t('shell.navigationLabel')}>
                  {nav.map((item) => <NavItem key={item.to} {...item} mobile active={isActive(path, item.to)} onClick={() => setMenuOpen(false)} />)}
                </nav>
                <div className="mt-auto flex flex-col gap-2 border-t p-4 text-sm text-muted-foreground">
                  <ConnectionState state={state} freshness={freshness} />
                  <div className="flex justify-between gap-3">
                    <span>{t('shell.release')}</span><code className="bg-transparent p-0 text-foreground">{releaseLabel}</code>
                  </div>
                </div>
              </SheetContent>
            </Sheet>
          </div>
        </div>
      </header>
      {state === 'connected' ? null : (
        <div className={`flex items-start gap-2.5 border-b px-4 py-2.5 text-sm sm:px-6 lg:px-8 ${state === 'reconnecting' ? 'bg-warning-background text-warning' : 'bg-danger-background text-danger'}`} role="status">
          <CircleAlert className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
          <div className="flex flex-col gap-0.5">
            <strong className="font-semibold">{t(`shell.banner.${state}`)}</strong>
            <span className="text-sm opacity-90">{t(`shell.banner.${state}Copy`)}</span>
          </div>
        </div>
      )}
      <main className="min-w-0 flex-1"><RotationNotice /><Outlet /></main>
      <footer className="sticky bottom-0 z-20 mt-auto border-t bg-background">
        <div className="mx-auto flex w-full max-w-[1280px] min-h-11 flex-wrap items-center gap-3 px-4 py-1.5 text-sm text-muted-foreground sm:px-6 lg:px-8">
          <ConnectionState state={state} freshness={freshness} detail={t(`shell.${state}Detail`)} />
          <div className="ml-auto hidden items-center gap-4 sm:flex">
            <span>{t('shell.release')} <code className="bg-transparent p-0 text-foreground">{releaseLabel}</code></span>
            <span>{t('shell.deployment')} <code className="bg-transparent p-0 text-foreground">{deploymentLabel}</code></span>
          </div>
        </div>
      </footer>
    </div>
  )
}

function ConnectionState({ state, freshness, detail }: { state: string; freshness: string; detail?: string }) {
  const { t } = useTranslation()
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span className={`size-2 shrink-0 rounded-full ${state === 'connected' ? 'bg-success' : state === 'reconnecting' ? 'animate-pulse bg-warning' : 'bg-danger'}`} />
      <strong className="font-semibold whitespace-nowrap text-foreground">{t(`shell.${state}`)}</strong>
      {detail ? <span className="hidden truncate sm:inline">· {detail}</span> : null}
      <span className="truncate">· {freshness}</span>
    </div>
  )
}

function NavItem({ to, label, icon: Icon, active, onClick, mobile }: typeof nav[number] & { active: boolean; onClick?: () => void; mobile?: boolean }) {
  const { t } = useTranslation()
  const name = t(label)
  // The medium breakpoint hides the label and leaves only the icon, so the
  // accessible name has to come from the item itself rather than its text.
  const base = 'flex items-center gap-2 rounded-md text-sm font-medium whitespace-nowrap no-underline transition-colors'
  const desktop = 'min-h-9 px-2.5 text-chrome-foreground hover:bg-chrome-hover data-[active]:bg-chrome-active data-[active]:font-semibold data-[active]:text-chrome-active-foreground'
  const drawer = 'min-h-12 px-3 text-foreground hover:bg-muted data-[active]:bg-primary data-[active]:font-semibold data-[active]:text-primary-foreground'
  return (
    <Link
      to={to}
      className={`${base} ${mobile ? drawer : desktop}`}
      data-active={active || undefined}
      aria-current={active ? 'page' : undefined}
      aria-label={name}
      title={name}
      onClick={onClick}
    >
      <Icon aria-hidden="true" className={mobile ? 'size-4.5' : 'size-4'} />
      <span className={mobile ? undefined : 'hidden lg:inline'}>{name}</span>
    </Link>
  )
}

function isActive(path: string, to: typeof nav[number]['to']) {
  const normalized = path.replace(/^\/_ui/, '') || '/'
  return to === '/' ? normalized === '/' : normalized.startsWith(to)
}
