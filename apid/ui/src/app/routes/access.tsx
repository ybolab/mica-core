import { createFileRoute } from '@tanstack/react-router'
import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Copy, KeyRound, Shield, Trash2 } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field, Input } from '@/components/ui/field'
import { Status } from '@/components/ui/status'
import { TaskProgress } from '@/components/task-progress'
import type { TaskAccepted } from '@/lib/types'

interface TokenSummary { id: string; name: string; created: number }
interface MintedToken extends TokenSummary { token: string }
interface AuthorizedKey { key: string; comment?: string; fingerprint?: string }
interface AuthorizedKeys { keys: AuthorizedKey[]; notice: string }

function AccessPage() {
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">Credentials</p><h1>Access</h1><p>Browser sessions, automation tokens and root SSH access.</p></div></header>
      <TokenPanel />
      <div className="split-grid"><SshPanel /><PasswordPanel /></div>
    </div>
  )
}

function PasswordPanel() {
  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [transientPassword, setTransientPassword] = useState('')
  const change = useMutation({
    mutationFn: () => api<void>('/api/v1/actions/change-password', json('POST', { currentPassword, newPassword })),
    onSuccess: () => { setCurrentPassword(''); setNewPassword(''); setConfirmPassword('') },
  })
  const transient = useMutation({
    mutationFn: () => api<TaskAccepted>('/api/v1/actions/transient-root-password', json('POST', { password: transientPassword })),
    onSuccess: () => setTransientPassword(''),
  })
  const changeSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (newPassword === confirmPassword) change.mutate()
  }
  const transientSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (window.confirm('Set this root password until the next reboot?')) transient.mutate()
  }
  return (
    <Card>
      <CardHeader title="Passwords" description="Change the web administrator credential or open a root password channel until reboot." action={<KeyRound className="size-5 text-muted-foreground" />} />
      <form className="grid gap-4" onSubmit={changeSubmit}>
        <Field label="Current admin password"><Input autoComplete="current-password" type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} required /></Field>
        <Field label="New admin password" hint="At least 8 characters."><Input autoComplete="new-password" minLength={8} type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} required /></Field>
        <Field label="Confirm new password"><Input autoComplete="new-password" minLength={8} type="password" value={confirmPassword} onChange={(event) => setConfirmPassword(event.target.value)} required /></Field>
        {newPassword && confirmPassword && newPassword !== confirmPassword ? <p className="callout error" role="alert">Passwords do not match.</p> : null}
        <Button type="submit" disabled={change.isPending || newPassword !== confirmPassword}>{change.isPending ? 'Changing…' : 'Change admin password'}</Button>
        {change.isSuccess ? <p className="callout success" role="status">Administrator password changed.</p> : null}
        {change.error ? <p className="callout error" role="alert">{errorMessage(change.error)}</p> : null}
      </form>
      <div className="section-divider" />
      <form className="grid gap-4" onSubmit={transientSubmit}>
        <Field label="Transient root password" hint="8–72 bytes; removed at the next reboot."><Input autoComplete="new-password" minLength={8} maxLength={72} type="password" value={transientPassword} onChange={(event) => setTransientPassword(event.target.value)} required /></Field>
        <Button variant="secondary" type="submit" disabled={transient.isPending}>{transient.isPending ? 'Setting…' : 'Set until reboot'}</Button>
        <TaskProgress taskId={transient.data?.taskId} />
        {transient.isSuccess ? <p className="callout success" role="status">Transient root password accepted.</p> : null}
        {transient.error ? <p className="callout error" role="alert">{errorMessage(transient.error)}</p> : null}
      </form>
    </Card>
  )
}

function TokenPanel() {
  const queryClient = useQueryClient()
  const [name, setName] = useState('')
  const [revealed, setRevealed] = useState<string>()
  const tokens = useQuery({ queryKey: ['tokens'], queryFn: () => api<TokenSummary[]>('/api/v1/tokens') })
  const mint = useMutation({
    mutationFn: () => api<MintedToken>('/api/v1/tokens', json('POST', { name })),
    onSuccess: (token) => { setRevealed(token.token); setName(''); queryClient.invalidateQueries({ queryKey: ['tokens'] }) },
  })
  const revoke = useMutation({
    mutationFn: (id: string) => api<void>(`/api/v1/tokens/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['tokens'] }),
  })
  const submit = (event: FormEvent) => { event.preventDefault(); mint.mutate() }
  return (
    <Card>
      <CardHeader title="API tokens" description="Long-lived credentials for automation. Plaintext is shown only once." action={<KeyRound className="size-5 text-muted-foreground" />} />
      {revealed ? <div className="reveal-panel"><strong>Copy this token now</strong><code>{revealed}</code><Button variant="secondary" size="sm" onClick={() => navigator.clipboard.writeText(revealed)}><Copy className="size-4" /> Copy</Button></div> : null}
      <div className="collection-list">
        {tokens.data?.map((token) => (
          <div className="collection-row" key={token.id}>
            <div><strong>{token.name}</strong><small><code>{token.id}</code>{token.created ? ` · ${new Date(token.created * 1000).toLocaleDateString()}` : ''}</small></div>
            <Button variant="ghost" size="sm" aria-label={`Revoke ${token.name}`} onClick={() => revoke.mutate(token.id)}><Trash2 className="size-4" /></Button>
          </div>
        ))}
        {tokens.data?.length === 0 ? <p className="empty">No API tokens.</p> : null}
      </div>
      <form className="inline-form" onSubmit={submit}>
        <Field label="New token label"><Input value={name} onChange={(event) => setName(event.target.value)} required placeholder="deployment agent" /></Field>
        <Button type="submit" disabled={mint.isPending}>{mint.isPending ? 'Creating…' : 'Create token'}</Button>
      </form>
      {mint.error || revoke.error ? <p className="callout error" role="alert">{errorMessage(mint.error ?? revoke.error)}</p> : null}
    </Card>
  )
}

function SshPanel() {
  const queryClient = useQueryClient()
  const [key, setKey] = useState('')
  const enabled = useQuery({ queryKey: ['settings', 'access.ssh.enabled'], queryFn: () => api<boolean>('/api/v1/settings/access.ssh.enabled') })
  const keys = useQuery({ queryKey: ['ssh-keys'], queryFn: () => api<AuthorizedKeys>('/api/v1/ssh/authorized-keys') })
  const toggle = useMutation({
    mutationFn: (value: boolean) => api<TaskAccepted>('/api/v1/settings/access.ssh.enabled', json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'access.ssh.enabled'] }),
  })
  const add = useMutation({
    mutationFn: () => api<unknown>('/api/v1/ssh/authorized-keys', json('POST', { key })),
    onSuccess: () => { setKey(''); queryClient.invalidateQueries({ queryKey: ['ssh-keys'] }) },
  })
  const remove = useMutation({
    mutationFn: (fingerprint: string) => api<void>(`/api/v1/ssh/authorized-keys/${encodeURIComponent(fingerprint)}`, { method: 'DELETE' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['ssh-keys'] }),
  })
  return (
    <Card>
      <CardHeader title="SSH" description={keys.data?.notice ?? 'Every authorized key grants root on this appliance.'} action={<Shield className="size-5 text-muted-foreground" />} />
      <div className="service-state"><Status ok={enabled.data === true}>SSH {enabled.data ? 'enabled' : 'disabled'}</Status><Button variant="secondary" size="sm" onClick={() => toggle.mutate(!enabled.data)}>{enabled.data ? 'Disable' : 'Enable'}</Button></div>
      <TaskProgress taskId={toggle.data?.taskId} />
      <div className="collection-list">
        {keys.data?.keys.map((entry) => (
          <div className="collection-row" key={entry.fingerprint ?? entry.key}>
            <div><strong>{entry.comment ?? 'SSH public key'}</strong><small><code>{entry.fingerprint ?? 'Unreadable fingerprint'}</code></small></div>
            {entry.fingerprint ? <Button variant="ghost" size="sm" aria-label="Remove SSH key" onClick={() => remove.mutate(entry.fingerprint!)}><Trash2 className="size-4" /></Button> : null}
          </div>
        ))}
      </div>
      <form className="inline-form" onSubmit={(event) => { event.preventDefault(); add.mutate() }}>
        <Field label="Authorized key"><Input value={key} onChange={(event) => setKey(event.target.value)} required placeholder="ssh-ed25519 AAAA… operator" /></Field>
        <Button type="submit" disabled={add.isPending}>Add key</Button>
      </form>
      {toggle.error || add.error || remove.error ? <p className="callout error" role="alert">{errorMessage(toggle.error ?? add.error ?? remove.error)}</p> : null}
    </Card>
  )
}

export const Route = createFileRoute('/access')({ component: AccessPage })
