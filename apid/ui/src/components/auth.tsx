import { useState, type FormEvent } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { KeyRound, ShieldCheck } from 'lucide-react'
import { api, errorMessage, json, rememberSession, type SessionStatus } from '@/lib/api'
import { Button } from '@/shared/components/ui/button'
import { Card } from '@/components/ui/card'
import { Field } from '@/shared/components/field'
import { Input } from '@/shared/components/ui/input'
import { Preferences } from '@/components/preferences'

export const sessionKey = ['session'] as const

function AuthFrame({ title, copy, children }: { title: string; copy: string; children: React.ReactNode }) {
  return (
    <main className="auth-shell">
      <div className="auth-preferences"><Preferences /></div>
      <Card className="auth-card">
        <i className="corner tl" aria-hidden="true" /><i className="corner tr" aria-hidden="true" />
        <i className="corner bl" aria-hidden="true" /><i className="corner br" aria-hidden="true" />
        <div className="auth-mark"><span className="logo-mark">m</span><strong>mos</strong></div>
        <h1>{title}</h1>
        <p>{copy}</p>
        {children}
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
  return (
    <AuthFrame title={t('auth.login.title')} copy={t('auth.login.copy')}>
      <form className="grid gap-5" onSubmit={submit}>
        <Field label={t('auth.login.password')}>
          <Input autoFocus autoComplete="current-password" type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />
        </Field>
        {login.error ? <p className="callout error" role="alert">{errorMessage(login.error, t('common.requestFailed'))}</p> : null}
        <Button size="lg" type="submit" disabled={login.isPending}>
          <KeyRound className="size-4" /> {login.isPending ? t('auth.login.pending') : t('auth.login.submit')}
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
          <div className="token-box"><code>{setup.data.token}</code></div>
          <Button size="lg" onClick={finish}><ShieldCheck className="size-4" /> {t('auth.complete.submit')}</Button>
        </div>
      </AuthFrame>
    )
  }
  return (
    <AuthFrame title={t('auth.setup.title')} copy={t('auth.setup.copy')}>
      <form className="grid gap-5" onSubmit={submit}>
        <Field label={t('auth.setup.hostname')} hint={t('auth.setup.hostnameHint')}>
          <Input autoFocus autoComplete="off" value={hostname} onChange={(event) => setHostname(event.target.value)} placeholder={t('auth.setup.hostnamePlaceholder')} />
        </Field>
        <Field label={t('auth.setup.password')} hint={t('auth.setup.passwordHint')}>
          <Input autoComplete="new-password" minLength={8} type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />
        </Field>
        <Field label={t('auth.setup.confirmPassword')}>
          <Input autoComplete="new-password" minLength={8} type="password" value={confirm} onChange={(event) => setConfirm(event.target.value)} required />
        </Field>
        {password && confirm && password !== confirm ? <p className="callout error" role="alert">{t('auth.setup.mismatch')}</p> : null}
        {setup.error ? <p className="callout error" role="alert">{errorMessage(setup.error, t('common.requestFailed'))}</p> : null}
        <Button size="lg" type="submit" disabled={setup.isPending || password !== confirm}>
          {setup.isPending ? t('auth.setup.pending') : t('auth.setup.submit')}
        </Button>
      </form>
    </AuthFrame>
  )
}
