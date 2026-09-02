import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Copy, KeyRound, Shield, Trash2 } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field } from '@/shared/components/field'
import { Input } from '@/shared/components/ui/input'
import { Status } from '@/components/ui/status'
import { TaskProgress } from '@/shared/components/task-progress'
import type { TaskAccepted } from '@/lib/types'
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from '@/shared/components/ui/alert-dialog'

interface TokenSummary { id: string; name: string; created: number }
interface MintedToken extends TokenSummary { token: string }
interface AuthorizedKey { key: string; comment?: string; fingerprint?: string }
interface AuthorizedKeys { keys: AuthorizedKey[]; notice: string }

export function AccessPage() {
  const { t } = useTranslation()
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('access.eyebrow')}</p><h1>{t('access.title')}</h1><p>{t('access.description')}</p></div></header>
      <TokenPanel />
      <div className="split-grid"><SshPanel /><PasswordPanel /></div>
    </div>
  )
}

function PasswordPanel() {
  const { t } = useTranslation()
  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const change = useMutation({
    mutationFn: () => api<void>('/api/v1/actions/change-password', json('POST', { currentPassword, newPassword })),
    onSuccess: () => { setCurrentPassword(''); setNewPassword(''); setConfirmPassword('') },
  })
  const changeSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (newPassword === confirmPassword) change.mutate()
  }
  return (
    <Card>
      <CardHeader title={t('access.password.title')} description={t('access.password.description')} action={<KeyRound className="size-5 text-muted-foreground" />} />
      <form className="grid gap-4" onSubmit={changeSubmit}>
        <Field label={t('access.password.current')}><Input autoComplete="current-password" type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} required /></Field>
        <Field label={t('access.password.next')} hint={t('access.password.hint')}><Input autoComplete="new-password" minLength={8} type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} required /></Field>
        <Field label={t('access.password.confirm')}><Input autoComplete="new-password" minLength={8} type="password" value={confirmPassword} onChange={(event) => setConfirmPassword(event.target.value)} required /></Field>
        {newPassword && confirmPassword && newPassword !== confirmPassword ? <p className="callout error" role="alert">{t('access.password.mismatch')}</p> : null}
        <Button type="submit" disabled={change.isPending || newPassword !== confirmPassword}>{change.isPending ? t('access.password.pending') : t('access.password.submit')}</Button>
        {change.isSuccess ? <p className="callout success" role="status">{t('access.password.changed')}</p> : null}
        {change.error ? <p className="callout error" role="alert">{errorMessage(change.error, t('common.requestFailed'))}</p> : null}
      </form>
    </Card>
  )
}

function TokenPanel() {
  const { t, i18n: activeI18n } = useTranslation()
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
      <CardHeader title={t('access.tokens.title')} description={t('access.tokens.description')} action={<KeyRound className="size-5 text-muted-foreground" />} />
      {revealed ? <div className="reveal-panel"><strong>{t('access.tokens.copyNow')}</strong><code>{revealed}</code><Button variant="secondary" size="sm" onClick={() => navigator.clipboard.writeText(revealed)}><Copy className="size-4" /> {t('common.actions.copy')}</Button></div> : null}
      <div className="collection-list">
        {tokens.data?.map((token) => (
          <div className="collection-row" key={token.id}>
            <div><strong>{token.name}</strong><small><code>{token.id}</code>{token.created ? ` · ${new Intl.DateTimeFormat(activeI18n.resolvedLanguage ?? 'en').format(new Date(token.created * 1000))}` : ''}</small></div>
            <AlertDialog><AlertDialogTrigger render={<Button variant="ghost" size="sm" disabled={revoke.isPending} aria-label={t('access.tokens.revokeLabel', { name: token.name })} />}><Trash2 className="size-4" /></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{t('access.tokens.revokeLabel', { name: token.name })}</AlertDialogTitle><AlertDialogDescription>{t('access.tokens.confirmRevoke', { name: token.name })}</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => revoke.mutate(token.id)}>{t('access.tokens.revokeLabel', { name: token.name })}</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog>
          </div>
        ))}
        {!tokens.isPending && !tokens.isError && tokens.data?.length === 0 ? <p className="empty">{t('access.tokens.empty')}</p> : null}
      </div>
      <form className="inline-form" onSubmit={submit}>
        <Field label={t('access.tokens.newLabel')}><Input value={name} onChange={(event) => setName(event.target.value)} required placeholder={t('access.tokens.placeholder')} /></Field>
        <Button type="submit" disabled={mint.isPending}>{mint.isPending ? t('access.tokens.pending') : t('access.tokens.create')}</Button>
      </form>
      {tokens.error || mint.error || revoke.error ? <p className="callout error" role="alert">{errorMessage(tokens.error ?? mint.error ?? revoke.error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

function SshPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [key, setKey] = useState('')
  const [transientPassword, setTransientPassword] = useState('')
  const [confirmTransient, setConfirmTransient] = useState(false)
  const transient = useMutation({
    mutationFn: () => api<TaskAccepted>('/api/v1/actions/transient-root-password', json('POST', { password: transientPassword })),
    onSuccess: () => setTransientPassword(''),
  })
  const transientSubmit = (event: FormEvent) => {
    event.preventDefault()
    setConfirmTransient(true)
  }
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
  const enabledState = enabled.isPending ? 'common.states.pending' : enabled.isError ? 'common.states.unknown' : enabled.data ? 'common.states.enabled' : 'common.states.disabled'
  return (
    <Card>
      <CardHeader title={t('access.ssh.title')} description={keys.data?.notice ?? t('access.ssh.defaultNotice')} action={<Shield className="size-5 text-muted-foreground" />} />
      <div className="service-state"><Status ok={enabled.data === true && !enabled.isPending}>{t('access.ssh.status', { state: t(enabledState) })}</Status><Button variant="secondary" size="sm" disabled={enabled.isPending || enabled.isError || toggle.isPending} onClick={() => toggle.mutate(!enabled.data)}>{t(enabled.data ? 'common.actions.disable' : 'common.actions.enable')}</Button></div>
      <TaskProgress taskId={toggle.data?.taskId} />
      <div className="collection-list">
        {keys.data?.keys.map((entry) => (
          <div className="collection-row" key={entry.fingerprint ?? entry.key}>
            <div><strong>{entry.comment ?? t('access.ssh.keyFallback')}</strong><small><code>{entry.fingerprint ?? t('access.ssh.unreadableFingerprint')}</code></small></div>
            {entry.fingerprint ? <AlertDialog><AlertDialogTrigger render={<Button variant="ghost" size="sm" disabled={remove.isPending} aria-label={t('access.ssh.removeLabel')} />}><Trash2 className="size-4" /></AlertDialogTrigger><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{t('access.ssh.removeLabel')}</AlertDialogTitle><AlertDialogDescription>{t('access.ssh.confirmRemove')}</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => remove.mutate(entry.fingerprint!)}>{t('common.actions.delete')}</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog> : null}
          </div>
        ))}
      </div>
      <form className="inline-form" onSubmit={(event) => { event.preventDefault(); add.mutate() }}>
        <Field label={t('access.ssh.authorizedKey')}><Input value={key} onChange={(event) => setKey(event.target.value)} required placeholder={t('access.ssh.keyPlaceholder')} /></Field>
        <Button type="submit" disabled={add.isPending}>{t('access.ssh.addKey')}</Button>
      </form>
      {enabled.error || keys.error || toggle.error || add.error || remove.error ? <p className="callout error" role="alert">{errorMessage(enabled.error ?? keys.error ?? toggle.error ?? add.error ?? remove.error, t('common.requestFailed'))}</p> : null}
      <div className="section-divider" />
      <form className="grid gap-4" onSubmit={transientSubmit}>
        <Field label={t('access.ssh.transient')} hint={t('access.ssh.transientHint')}><Input autoComplete="new-password" minLength={8} maxLength={72} type="password" value={transientPassword} onChange={(event) => setTransientPassword(event.target.value)} required /></Field>
        <Button variant="secondary" type="submit" disabled={transient.isPending}>{transient.isPending ? t('access.ssh.setting') : t('access.ssh.setUntilReboot')}</Button>
        <TaskProgress taskId={transient.data?.taskId} />
        {transient.isSuccess ? <p className="callout success" role="status">{t('access.ssh.transientAccepted')}</p> : null}
        {transient.error ? <p className="callout error" role="alert">{errorMessage(transient.error, t('common.requestFailed'))}</p> : null}
      </form>
      <AlertDialog open={confirmTransient} onOpenChange={setConfirmTransient}><AlertDialogContent><AlertDialogHeader><AlertDialogTitle>{t('access.ssh.transient')}</AlertDialogTitle><AlertDialogDescription>{t('access.ssh.confirmTransient')}</AlertDialogDescription></AlertDialogHeader><AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction onClick={() => transient.mutate()}>{t('access.ssh.setUntilReboot')}</AlertDialogAction></AlertDialogFooter></AlertDialogContent></AlertDialog>
    </Card>
  )
}
