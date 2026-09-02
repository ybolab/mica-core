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

function AuthFrame({ eyebrow, title, copy, children }: { eyebrow: string; title: string; copy: string; children: React.ReactNode }) {
  return (
    <main className="auth-shell">
      <div className="auth-preferences"><Preferences compact /></div>
      <div className="auth-brand" aria-hidden="true">
        <span>mos</span>
        <div className="auth-orbit" />
      </div>
      <Card className="auth-card">
        <p className="eyebrow">{eyebrow}</p>
        <h1 className="mt-3 text-3xl font-semibold tracking-[-0.04em]">{title}</h1>
        <p className="mt-3 max-w-md text-sm leading-6 text-muted-foreground">{copy}</p>
        <div className="mt-7">{children}</div>
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
    <AuthFrame eyebrow={t('auth.login.eyebrow')} title={t('auth.login.title')} copy={t('auth.login.copy')}>
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
      <AuthFrame eyebrow={t('auth.complete.eyebrow')} title={t('auth.complete.title')} copy={t('auth.complete.copy')}>
        <div className="grid gap-5">
          <div className="token-box"><code>{setup.data.token}</code></div>
          <Button size="lg" onClick={finish}><ShieldCheck className="size-4" /> {t('auth.complete.submit')}</Button>
        </div>
      </AuthFrame>
    )
  }
  return (
    <AuthFrame eyebrow={t('auth.setup.eyebrow')} title={t('auth.setup.title')} copy={t('auth.setup.copy')}>
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
