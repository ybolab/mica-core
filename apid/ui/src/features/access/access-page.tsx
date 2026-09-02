import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Copy } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import { Page, PageHeader, Section, Surface } from '@/shared/components/product-layout'
import { StatusBadge } from '@/shared/components/status-badge'
import { Button } from '@/shared/components/ui/button'
import { Field } from '@/shared/components/field'
import { Input } from '@/shared/components/ui/input'
import { Switch } from '@/shared/components/ui/switch'
import { TaskProgress } from '@/shared/components/task-progress'
import type { TaskAccepted } from '@/lib/types'
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from '@/shared/components/ui/alert-dialog'
import { ClaimPanel } from '@/features/onboarding/claim-panel'
import { ProvisioningPanel } from '@/features/onboarding/provisioning-panel'

interface TokenSummary { id: string; name: string; created: number }
interface MintedToken extends TokenSummary { token: string }
interface AuthorizedKey { key: string; comment?: string; fingerprint?: string }
interface AuthorizedKeys { keys: AuthorizedKey[]; notice: string }

export function AccessPage() {
  const { t } = useTranslation()
  return (
    <Page>
      <PageHeader title={t('access.title')} />
      <Section title={t('access.password.title')} description={t('access.password.description')}><PasswordPanel /></Section>
      <Section title={t('access.tokens.title')} description={t('access.tokens.description')}><TokenPanel /></Section>
      <Section title={t('access.ssh.title')} description={t('access.ssh.sectionCopy')}><SshPanel /></Section>
      <Section title={t('access.root.title')} description={t('access.root.description')} className="danger-section"><RootPanel /></Section>
      <Section title={t('access.onboarding.title')} description={t('access.onboarding.addition')} className="marked-addition">
        <div className="split-grid"><ClaimPanel /><ProvisioningPanel /></div>
      </Section>
    </Page>
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
  const submit = (event: FormEvent) => {
    event.preventDefault()
    if (newPassword === confirmPassword) change.mutate()
  }
  return (
    <Surface>
      <form className="grid gap-4" onSubmit={submit}>
        <Field label={t('access.password.current')}><Input autoComplete="current-password" type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} required /></Field>
        <div className="content-grid">
          <Field label={t('access.password.next')} hint={t('access.password.hint')}><Input autoComplete="new-password" minLength={8} type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} required /></Field>
          <Field label={t('access.password.confirm')}><Input autoComplete="new-password" minLength={8} type="password" value={confirmPassword} onChange={(event) => setConfirmPassword(event.target.value)} required /></Field>
        </div>
        {newPassword && confirmPassword && newPassword !== confirmPassword ? <p className="callout error" role="alert">{t('access.password.mismatch')}</p> : null}
        {change.isSuccess ? <p className="callout success" role="status">{t('access.password.changed')}</p> : null}
        {change.error ? <p className="callout error" role="alert">{errorMessage(change.error, t('common.requestFailed'))}</p> : null}
        <Button type="submit" disabled={change.isPending || newPassword !== confirmPassword}>{change.isPending ? t('access.password.pending') : t('access.password.submit')}</Button>
      </form>
    </Surface>
  )
}

function TokenPanel() {
  const { t, i18n: activeI18n } = useTranslation()
  const queryClient = useQueryClient()
  const [name, setName] = useState('')
  const [revealed, setRevealed] = useState<string>()
  const [copied, setCopied] = useState(false)
  const [minting, setMinting] = useState(false)
  const tokens = useQuery({ queryKey: ['tokens'], queryFn: () => api<TokenSummary[]>('/api/v1/tokens') })
  const mint = useMutation({
    mutationFn: () => api<MintedToken>('/api/v1/tokens', json('POST', { name })),
    onSuccess: (token) => { setRevealed(token.token); setCopied(false); setName(''); setMinting(false); void queryClient.invalidateQueries({ queryKey: ['tokens'] }) },
  })
  const revoke = useMutation({
    mutationFn: (id: string) => api<void>(`/api/v1/tokens/${encodeURIComponent(id)}`, { method: 'DELETE' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['tokens'] }),
  })
  const copy = () => { void navigator.clipboard.writeText(revealed ?? ''); setCopied(true) }

  return (
    <div className="stack">
      {revealed ? (
        <div className="reveal-panel" role="alert">
          <div><strong>{t('access.tokens.copyNow')}</strong><span>{t('access.tokens.revealCopy')}</span></div>
          <code>{revealed}</code>
          <div className="table-actions">
            <Button size="sm" onClick={copy}><Copy />{copied ? t('access.tokens.copied') : t('common.actions.copy')}</Button>
            <AlertDialog>
              <AlertDialogTrigger render={<Button size="sm" variant="outline" />}>{t('access.tokens.closeReveal')}</AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader><AlertDialogTitle>{t('access.tokens.closeReveal')}</AlertDialogTitle><AlertDialogDescription>{t('access.tokens.closeRevealCopy')}</AlertDialogDescription></AlertDialogHeader>
                <AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => setRevealed(undefined)}>{t('access.tokens.closeRevealConfirm')}</AlertDialogAction></AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          </div>
        </div>
      ) : null}
      <Surface className="surface-compact">
        <div className="data-table-wrap">
          <table className="data-table">
            <thead><tr><th>{t('access.tokens.label')}</th><th>ID</th><th>{t('access.tokens.created')}</th><th /></tr></thead>
            <tbody>{tokens.data?.map((token) => (
              <tr key={token.id}>
                <td>{token.name}</td>
                <td className="mono-cell">{token.id}</td>
                <td>{token.created ? new Intl.DateTimeFormat(activeI18n.resolvedLanguage ?? 'en').format(new Date(token.created * 1000)) : '—'}</td>
                <td className="text-right">
                  <AlertDialog>
                    <AlertDialogTrigger render={<Button variant="destructive" size="sm" disabled={revoke.isPending} aria-label={t('access.tokens.revokeLabel', { name: token.name })} />}>{t('access.tokens.revoke')}</AlertDialogTrigger>
                    <AlertDialogContent>
                      <AlertDialogHeader><AlertDialogTitle>{t('access.tokens.revokeLabel', { name: token.name })}</AlertDialogTitle><AlertDialogDescription>{t('access.tokens.confirmRevoke', { name: token.name })}</AlertDialogDescription></AlertDialogHeader>
                      <AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => revoke.mutate(token.id)}>{t('access.tokens.revoke')}</AlertDialogAction></AlertDialogFooter>
                    </AlertDialogContent>
                  </AlertDialog>
                </td>
              </tr>
            ))}</tbody>
          </table>
          {!tokens.isPending && !tokens.isError && tokens.data?.length === 0 ? <p className="empty">{t('access.tokens.empty')}</p> : null}
        </div>
        <div className="panel-footer">
          {minting ? (
            <form className="inline-form" onSubmit={(event) => { event.preventDefault(); mint.mutate() }}>
              <Field label={t('access.tokens.newLabel')}><Input value={name} onChange={(event) => setName(event.target.value)} required placeholder={t('access.tokens.placeholder')} autoFocus /></Field>
              <Button type="submit" disabled={mint.isPending}>{mint.isPending ? t('access.tokens.pending') : t('access.tokens.create')}</Button>
            </form>
          ) : <Button size="sm" variant="outline" onClick={() => setMinting(true)}>{t('access.tokens.mint')}</Button>}
        </div>
      </Surface>
      {tokens.error || mint.error || revoke.error ? <p className="callout error" role="alert">{errorMessage(tokens.error ?? mint.error ?? revoke.error, t('common.requestFailed'))}</p> : null}
    </div>
  )
}

function SshPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [key, setKey] = useState('')
  const [adding, setAdding] = useState(false)
  const enabled = useQuery({ queryKey: ['settings', 'access.ssh.enabled'], queryFn: () => api<boolean>('/api/v1/settings/access.ssh.enabled') })
  const keys = useQuery({ queryKey: ['ssh-keys'], queryFn: () => api<AuthorizedKeys>('/api/v1/ssh/authorized-keys') })
  const toggle = useMutation({
    mutationFn: (value: boolean) => api<TaskAccepted>('/api/v1/settings/access.ssh.enabled', json('PUT', value)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['settings', 'access.ssh.enabled'] }),
  })
  const add = useMutation({
    mutationFn: () => api<unknown>('/api/v1/ssh/authorized-keys', json('POST', { key })),
    onSuccess: () => { setKey(''); setAdding(false); void queryClient.invalidateQueries({ queryKey: ['ssh-keys'] }) },
  })
  const remove = useMutation({
    mutationFn: (fingerprint: string) => api<void>(`/api/v1/ssh/authorized-keys/${encodeURIComponent(fingerprint)}`, { method: 'DELETE' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['ssh-keys'] }),
  })
  const state = enabled.isPending ? 'common.states.pending' : enabled.isError ? 'common.states.unknown' : enabled.data ? 'common.states.enabled' : 'common.states.disabled'
  const panelError = enabled.error ?? keys.error ?? toggle.error ?? add.error ?? remove.error

  return (
    <Surface className="surface-compact">
      <div className="panel-row">
        <div><strong>{t('access.ssh.server')}</strong><small>{t('access.ssh.serverState', { state: t(state) })}</small></div>
        <Switch checked={enabled.data === true} onCheckedChange={(value) => toggle.mutate(value)} disabled={enabled.isPending || enabled.isError || toggle.isPending} aria-label={t('access.ssh.server')} />
      </div>
      <TaskProgress taskId={toggle.data?.taskId} />
      {keys.data?.keys.map((entry) => (
        <div className="panel-row" key={entry.fingerprint ?? entry.key}>
          <div>
            <span className="mono">{entry.fingerprint ?? t('access.ssh.unreadableFingerprint')}</span>
            <small>{entry.comment ?? t('access.ssh.keyFallback')} · <span className="text-danger">{keys.data.notice}</span></small>
          </div>
          {entry.fingerprint ? (
            <AlertDialog>
              <AlertDialogTrigger render={<Button variant="destructive" size="sm" disabled={remove.isPending} aria-label={t('access.ssh.removeLabel')} />}>{t('access.ssh.remove')}</AlertDialogTrigger>
              <AlertDialogContent>
                <AlertDialogHeader><AlertDialogTitle>{t('access.ssh.removeLabel')}</AlertDialogTitle><AlertDialogDescription>{t('access.ssh.confirmRemove')}</AlertDialogDescription></AlertDialogHeader>
                <AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => remove.mutate(entry.fingerprint ?? '')}>{t('access.ssh.remove')}</AlertDialogAction></AlertDialogFooter>
              </AlertDialogContent>
            </AlertDialog>
          ) : null}
        </div>
      ))}
      <div className="panel-footer">
        {adding ? (
          <form className="inline-form" onSubmit={(event) => { event.preventDefault(); add.mutate() }}>
            <Field label={t('access.ssh.authorizedKey')}><Input value={key} onChange={(event) => setKey(event.target.value)} required placeholder={t('access.ssh.keyPlaceholder')} autoFocus /></Field>
            <Button type="submit" disabled={add.isPending}>{t('access.ssh.addKey')}</Button>
          </form>
        ) : <Button size="sm" variant="outline" onClick={() => setAdding(true)}>{t('access.ssh.addKey')}</Button>}
      </div>
      {panelError ? <p className="callout error" role="alert">{errorMessage(panelError, t('common.requestFailed'))}</p> : null}
    </Surface>
  )
}

function RootPanel() {
  const { t } = useTranslation()
  const [password, setPassword] = useState('')
  const [confirming, setConfirming] = useState(false)
  const transient = useMutation({
    mutationFn: () => api<TaskAccepted>('/api/v1/actions/transient-root-password', json('POST', { password })),
    onSuccess: () => setPassword(''),
  })
  return (
    <Surface className="root-panel">
      <div className="panel-head"><StatusBadge tone="danger">{t('access.root.highPrivilege')}</StatusBadge><span>{t('access.root.state')}</span></div>
      <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); setConfirming(true) }}>
        <Field label={t('access.root.password')} hint={t('access.ssh.transientHint')}>
          <Input autoComplete="new-password" minLength={8} maxLength={72} type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />
        </Field>
        {transient.isSuccess ? <p className="callout success" role="status">{t('access.ssh.transientAccepted')}</p> : null}
        {transient.error ? <p className="callout error" role="alert">{errorMessage(transient.error, t('common.requestFailed'))}</p> : null}
        <Button type="submit" variant="destructive" disabled={transient.isPending}>{transient.isPending ? t('access.ssh.setting') : t('access.ssh.setUntilReboot')}</Button>
      </form>
      <TaskProgress taskId={transient.data?.taskId} />
      <AlertDialog open={confirming} onOpenChange={setConfirming}>
        <AlertDialogContent>
          <AlertDialogHeader><AlertDialogTitle>{t('access.root.title')}</AlertDialogTitle><AlertDialogDescription>{t('access.ssh.confirmTransient')}</AlertDialogDescription></AlertDialogHeader>
          <AlertDialogFooter><AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel><AlertDialogAction variant="destructive" onClick={() => transient.mutate()}>{t('access.ssh.setUntilReboot')}</AlertDialogAction></AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </Surface>
  )
}
