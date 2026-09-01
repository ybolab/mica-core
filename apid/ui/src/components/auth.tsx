import { useState, type FormEvent } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { KeyRound, ShieldCheck } from 'lucide-react'
import { api, errorMessage, json, rememberSession, type SessionStatus } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Field, Input } from '@/components/ui/field'

export const sessionKey = ['session'] as const

function AuthFrame({ eyebrow, title, copy, children }: { eyebrow: string; title: string; copy: string; children: React.ReactNode }) {
  return (
    <main className="auth-shell">
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
    <AuthFrame eyebrow="Device console" title="Welcome back" copy="Sign in with the appliance administrator password. The session stays on this device and is never exposed to JavaScript.">
      <form className="grid gap-5" onSubmit={submit}>
        <Field label="Admin password">
          <Input autoFocus autoComplete="current-password" type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />
        </Field>
        {login.error ? <p className="callout error" role="alert">{errorMessage(login.error)}</p> : null}
        <Button size="lg" type="submit" disabled={login.isPending}>
          <KeyRound className="size-4" /> {login.isPending ? 'Signing in…' : 'Sign in'}
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
      <AuthFrame eyebrow="Setup complete" title="Save your API token" copy="This token is shown once. Store it in your password manager before entering the console.">
        <div className="grid gap-5">
          <div className="token-box"><code>{setup.data.token}</code></div>
          <Button size="lg" onClick={finish}><ShieldCheck className="size-4" /> I saved the token</Button>
        </div>
      </AuthFrame>
    )
  }
  return (
    <AuthFrame eyebrow="First run" title="Make this device yours" copy="Set the administrator password and, optionally, a hostname. Network interfaces can be inspected from the console next.">
      <form className="grid gap-5" onSubmit={submit}>
        <Field label="Hostname" hint="Optional; letters, numbers and hyphens.">
          <Input autoFocus autoComplete="off" value={hostname} onChange={(event) => setHostname(event.target.value)} placeholder="mos" />
        </Field>
        <Field label="Admin password" hint="At least 8 characters.">
          <Input autoComplete="new-password" minLength={8} type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />
        </Field>
        <Field label="Confirm password">
          <Input autoComplete="new-password" minLength={8} type="password" value={confirm} onChange={(event) => setConfirm(event.target.value)} required />
        </Field>
        {password && confirm && password !== confirm ? <p className="callout error" role="alert">Passwords do not match.</p> : null}
        {setup.error ? <p className="callout error" role="alert">{errorMessage(setup.error)}</p> : null}
        <Button size="lg" type="submit" disabled={setup.isPending || password !== confirm}>
          {setup.isPending ? 'Configuring…' : 'Configure device'}
        </Button>
      </form>
    </AuthFrame>
  )
}
