import { useState, type FormEvent, type ReactNode } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { KeyRound, ShieldCheck } from 'lucide-react'
import { api, json, rememberSession, type SessionStatus } from '@/shared/lib/http'
import { Button } from '@/shared/components/ui/button'
import { Card, CardContent } from '@/shared/components/ui/card'
import { Input } from '@/shared/components/ui/input'
import { Callout } from '@/shared/components/callout'
import { CopyField } from '@/shared/components/copy-field'
import { FormField } from '@/shared/components/form-field'
import { failureDetail } from '@/shared/feedback/toast'
import { Preferences } from '@/features/preferences/preferences'

export const sessionKey = ['session'] as const

/// The sign-in plate. The registration marks in the corners are the approved
/// prototype's blueprint framing; they are decoration on a card, not a
/// component.
function AuthFrame({ title, copy, children }: { title: string; copy: string; children: ReactNode }) {
  return (
    <main className="relative grid min-h-dvh place-items-center bg-background px-6 py-16">
      <div className="absolute top-4 right-4 max-sm:static max-sm:mb-4 max-sm:justify-self-stretch"><Preferences /></div>
      <Card className="auth-plate relative w-full max-w-100 rounded-none">
        <i className="corner tl" aria-hidden="true" /><i className="corner tr" aria-hidden="true" />
        <i className="corner bl" aria-hidden="true" /><i className="corner br" aria-hidden="true" />
        <CardContent className="flex flex-col gap-5 p-6 sm:p-8">
          <div className="flex items-center gap-2.5">
            <span className="logo-mark size-8 text-base">m</span>
            <strong className="font-condensed text-xl leading-none font-semibold">mica</strong>
          </div>
          <div className="flex flex-col gap-1">
            <h1 className="text-lg font-semibold tracking-tight">{title}</h1>
            <p className="text-sm text-muted-foreground">{copy}</p>
          </div>
          {children}
        </CardContent>
      </Card>
    </main>
  )
}

export function LoginView() {
  const { t } = useTranslation()
  const [password, setPassword] = useState('')
  const queryClient = useQueryClient()
  const login = useMutation({
    mutationFn: () => api<SessionStatus>('/api/v1/session', json('POST', { password })),
    onSuccess: (session) => {
      rememberSession(session)
      queryClient.setQueryData(sessionKey, session)
    },
  })
  const submit = (event: FormEvent) => {
    event.preventDefault()
    login.mutate()
  }
  // A refused sign-in stays on the form rather than becoming a toast: it is the
  // state of the field the operator is about to correct.
  return (
    <AuthFrame title={t('auth.login.title')} copy={t('auth.login.copy')}>
      <form className="grid gap-5" onSubmit={submit}>
        <FormField label={t('auth.login.password')}>
          {(id) => <Input id={id} autoFocus autoComplete="current-password" type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />}
        </FormField>
        {login.error ? <Callout tone="danger" title={failureDetail(login.error, t('common.requestFailed'))} /> : null}
        <Button size="lg" type="submit" disabled={login.isPending}>
          <KeyRound /> {login.isPending ? t('auth.login.pending') : t('auth.login.submit')}
        </Button>
      </form>
    </AuthFrame>
  )
}

interface SetupResult {
  token: string
  csrfToken: string
}

export function SetupView() {
  const { t } = useTranslation()
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')
  const [hostname, setHostname] = useState('')
  const queryClient = useQueryClient()
  const setup = useMutation({
    mutationFn: () => api<SetupResult>('/api/v1/setup', json('POST', {
      password,
      ...(hostname ? { hostname } : {}),
    })),
  })
  const submit = (event: FormEvent) => {
    event.preventDefault()
    if (password !== confirm) return
    setup.mutate()
  }
  if (setup.data) {
    const finish = () => {
      const session: SessionStatus = { state: 'authenticated', csrfToken: setup.data.csrfToken }
      rememberSession(session)
      queryClient.setQueryData(sessionKey, session)
    }
    return (
      <AuthFrame title={t('auth.complete.title')} copy={t('auth.complete.copy')}>
        <div className="grid gap-5">
          {/* Shown once and never again, so it is offered with a copy control
              rather than left to be selected by hand. */}
          <CopyField value={setup.data.token} label={t('common.actions.copy')} />
          <Button size="lg" onClick={finish}><ShieldCheck /> {t('auth.complete.submit')}</Button>
        </div>
      </AuthFrame>
    )
  }
  return (
    <AuthFrame title={t('auth.setup.title')} copy={t('auth.setup.copy')}>
      <form className="grid gap-5" onSubmit={submit}>
        <FormField label={t('auth.setup.hostname')} hint={t('auth.setup.hostnameHint')}>
          {(id) => <Input id={id} autoFocus autoComplete="off" value={hostname} onChange={(event) => setHostname(event.target.value)} placeholder={t('auth.setup.hostnamePlaceholder')} />}
        </FormField>
        <FormField label={t('auth.setup.password')} hint={t('auth.setup.passwordHint')}>
          {(id) => <Input id={id} autoComplete="new-password" minLength={8} type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />}
        </FormField>
        <FormField label={t('auth.setup.confirmPassword')}>
          {(id) => <Input id={id} autoComplete="new-password" minLength={8} type="password" value={confirm} onChange={(event) => setConfirm(event.target.value)} required />}
        </FormField>
        {password && confirm && password !== confirm ? <Callout tone="danger" title={t('auth.setup.mismatch')} /> : null}
        {setup.error ? <Callout tone="danger" title={failureDetail(setup.error, t('common.requestFailed'))} /> : null}
        <Button size="lg" type="submit" disabled={setup.isPending || password !== confirm}>
          {setup.isPending ? t('auth.setup.pending') : t('auth.setup.submit')}
        </Button>
      </form>
    </AuthFrame>
  )
}
