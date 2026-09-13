import { useState, type FormEvent } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Plus, Trash2 } from 'lucide-react'
import { api, json } from '@/shared/lib/http'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { CopyField } from '@/shared/components/copy-field'
import { DataTable } from '@/shared/components/data-table'
import { FormDialog } from '@/shared/components/form-dialog'
import { FormField } from '@/shared/components/form-field'
import { Page, PageHeader, PageSection } from '@/shared/components/page'
import { CollectionPanel, Panel } from '@/shared/components/panel'
import { RowItem, RowList } from '@/shared/components/row-item'
import { StatusBadge } from '@/shared/components/status-badge'
import { TaskProgress } from '@/shared/components/task-progress'
import { Button } from '@/shared/components/ui/button'
import { Input } from '@/shared/components/ui/input'
import { Switch } from '@/shared/components/ui/switch'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'
import type { TaskAccepted } from '@/lib/types'
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
      <PageSection title={t('access.password.title')} description={t('access.password.description')}><PasswordPanel /></PageSection>
      <PageSection title={t('access.tokens.title')} description={t('access.tokens.description')}><TokenPanel /></PageSection>
      <PageSection title={t('access.ssh.title')} description={t('access.ssh.sectionCopy')}><SshPanel /></PageSection>
      <PageSection title={t('access.root.title')} description={t('access.root.description')} tone="danger"><RootPanel /></PageSection>
      <PageSection title={t('access.onboarding.title')} description={t('access.onboarding.addition')}>
        <div className="grid gap-3 lg:grid-cols-2"><ClaimPanel /><ProvisioningPanel /></div>
      </PageSection>
    </Page>
  )
}

function PasswordPanel() {
  const { t } = useTranslation()
  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const change = useMutationFeedback({
    mutationFn: () => api<void>('/api/v1/actions/change-password', json('POST', { currentPassword, newPassword })),
    success: t('access.password.changed'),
    failure: t('access.password.submit'),
    onSuccess: () => { setCurrentPassword(''); setNewPassword(''); setConfirmPassword('') },
  })
  const submit = (event: FormEvent) => {
    event.preventDefault()
    if (newPassword === confirmPassword) change.mutate()
  }
  return (
    <Panel>
      <form className="grid gap-4" onSubmit={submit}>
        <FormField label={t('access.password.current')}>
          {(id) => <Input id={id} autoComplete="current-password" type="password" value={currentPassword} onChange={(event) => setCurrentPassword(event.target.value)} required />}
        </FormField>
        <div className="grid gap-4 sm:grid-cols-2">
          <FormField label={t('access.password.next')} hint={t('access.password.hint')}>
            {(id) => <Input id={id} autoComplete="new-password" minLength={8} type="password" value={newPassword} onChange={(event) => setNewPassword(event.target.value)} required />}
          </FormField>
          <FormField label={t('access.password.confirm')}>
            {(id) => <Input id={id} autoComplete="new-password" minLength={8} type="password" value={confirmPassword} onChange={(event) => setConfirmPassword(event.target.value)} required />}
          </FormField>
        </div>
        {/* A mismatch is the state of the two fields, not the outcome of a
            request, so it stays beside them. */}
        {newPassword && confirmPassword && newPassword !== confirmPassword ? <Callout tone="danger" title={t('access.password.mismatch')} /> : null}
        <Button className="justify-self-end" type="submit" disabled={change.isPending || newPassword !== confirmPassword}>
          {change.isPending ? t('access.password.pending') : t('access.password.submit')}
        </Button>
      </form>
    </Panel>
  )
}

function TokenPanel() {
  const { t, i18n: activeI18n } = useTranslation()
  const queryClient = useQueryClient()
  const [name, setName] = useState('')
  const [revealed, setRevealed] = useState<string>()
  const [minting, setMinting] = useState(false)
  const tokens = useQuery({ queryKey: ['tokens'], queryFn: () => api<TokenSummary[]>('/api/v1/tokens') })
  const revoke = useMutationFeedback<void, TokenSummary>({
    mutationFn: (token) => api<void>(`/api/v1/tokens/${encodeURIComponent(token.id)}`, { method: 'DELETE' }),
    success: (_data, token) => t('access.tokens.revoked', { name: token.name }),
    failure: t('access.tokens.revoke'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['tokens'] }),
  })
  const mint = () => api<MintedToken>('/api/v1/tokens', json('POST', { name }))
    .then((token) => {
      setRevealed(token.token)
      setName('')
      return queryClient.invalidateQueries({ queryKey: ['tokens'] })
    })

  return (
    <div className="grid gap-3">
      {/* Shown once. It survives a re-render deliberately, and closing it is a
          confirmation, because the value cannot be recovered. */}
      {revealed ? (
        <Callout tone="warning" title={t('access.tokens.copyNow')}>
          <span className="grid gap-3">
            <span>{t('access.tokens.revealCopy')}</span>
            <CopyField value={revealed} label={t('common.actions.copy')} />
            <ConfirmDialog
              trigger={<Button size="sm" variant="outline">{t('access.tokens.closeReveal')}</Button>}
              title={t('access.tokens.closeReveal')}
              description={t('access.tokens.closeRevealCopy')}
              confirmLabel={t('access.tokens.closeRevealConfirm')}
              success={t('access.tokens.revealClosed')}
              failure={t('access.tokens.closeReveal')}
              onConfirm={() => setRevealed(undefined)}
            />
          </span>
        </Callout>
      ) : null}
      <CollectionPanel
        footer={<Button size="sm" variant="outline" onClick={() => setMinting(true)}><Plus />{t('access.tokens.mint')}</Button>}
      >
        <DataTable<TokenSummary>
          rows={tokens.data}
          rowKey={(token) => token.id}
          isPending={tokens.isPending}
          empty={t('access.tokens.empty')}
          columns={[
            { id: 'name', header: t('access.tokens.label'), cell: (token) => token.name },
            { id: 'id', header: 'ID', cell: (token) => <span className="font-mono text-[0.8125rem] break-all">{token.id}</span> },
            { id: 'created', header: t('access.tokens.created'), cell: (token) => token.created ? new Intl.DateTimeFormat(activeI18n.resolvedLanguage ?? 'en').format(new Date(token.created * 1000)) : '—' },
            { id: 'actions', header: '', align: 'end', cell: (token) => (
              <ConfirmDialog
                trigger={<Button variant="destructive" size="sm" aria-label={t('access.tokens.revokeLabel', { name: token.name })}>{t('access.tokens.revoke')}</Button>}
                title={t('access.tokens.revokeLabel', { name: token.name })}
                description={t('access.tokens.confirmRevoke', { name: token.name })}
                confirmLabel={t('access.tokens.revoke')}
                success={t('access.tokens.revoked', { name: token.name })}
                failure={t('access.tokens.revoke')}
                onConfirm={() => revoke.mutateAsync(token)}
              />
            ) },
          ]}
        />
      </CollectionPanel>
      {tokens.error ? <Callout tone="danger" title={failureDetail(tokens.error, t('common.requestFailed'))} /> : null}
      <FormDialog
        open={minting}
        onOpenChange={setMinting}
        title={t('access.tokens.mint')}
        submitLabel={t('access.tokens.create')}
        success={t('access.tokens.minted')}
        failure={t('access.tokens.mint')}
        onSubmit={mint}
      >
        <FormField label={t('access.tokens.newLabel')}>
          {(id) => <Input id={id} value={name} onChange={(event) => setName(event.target.value)} required placeholder={t('access.tokens.placeholder')} />}
        </FormField>
      </FormDialog>
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
  const toggle = useMutationFeedback<TaskAccepted, boolean>({
    mutationFn: (value) => api<TaskAccepted>('/api/v1/settings/access.ssh.enabled', json('PUT', value)),
    success: (_data, value) => t(value ? 'access.ssh.serverEnabled' : 'access.ssh.serverDisabled'),
    failure: t('access.ssh.server'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['settings', 'access.ssh.enabled'] }),
  })
  const remove = useMutationFeedback<void, string>({
    mutationFn: (fingerprint) => api<void>(`/api/v1/ssh/authorized-keys/${encodeURIComponent(fingerprint)}`, { method: 'DELETE' }),
    success: t('access.ssh.keyRemoved'),
    failure: t('access.ssh.remove'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: ['ssh-keys'] }),
  })
  const add = () => api<unknown>('/api/v1/ssh/authorized-keys', json('POST', { key }))
    .then(() => { setKey(''); return queryClient.invalidateQueries({ queryKey: ['ssh-keys'] }) })
  const state = enabled.isPending ? 'common.states.pending' : enabled.isError ? 'common.states.unknown' : enabled.data ? 'common.states.enabled' : 'common.states.disabled'
  const panelError = enabled.error ?? keys.error

  return (
    <CollectionPanel
      footer={<Button size="sm" variant="outline" onClick={() => setAdding(true)}><Plus />{t('access.ssh.addKey')}</Button>}
    >
      <RowList>
        <RowItem
          title={t('access.ssh.server')}
          description={t('access.ssh.serverState', { state: t(state) })}
          actions={<Switch checked={enabled.data === true} onCheckedChange={(value) => toggle.mutate(value)} disabled={enabled.isPending || enabled.isError || toggle.isPending} aria-label={t('access.ssh.server')} />}
        />
        {keys.data?.keys.map((entry) => (
          <RowItem
            key={entry.fingerprint ?? entry.key}
            title={<span className="font-mono text-[0.8125rem] break-all">{entry.fingerprint ?? t('access.ssh.unreadableFingerprint')}</span>}
            description={<>{entry.comment ?? t('access.ssh.keyFallback')} · <span className="text-destructive">{keys.data.notice}</span></>}
            actions={entry.fingerprint ? (
              <ConfirmDialog
                trigger={<Button variant="destructive" size="sm" aria-label={t('access.ssh.removeLabel')}><Trash2 />{t('access.ssh.remove')}</Button>}
                title={t('access.ssh.removeLabel')}
                description={t('access.ssh.confirmRemove')}
                confirmLabel={t('access.ssh.remove')}
                success={t('access.ssh.keyRemoved')}
                failure={t('access.ssh.remove')}
                onConfirm={() => remove.mutateAsync(entry.fingerprint ?? '')}
              />
            ) : undefined}
          />
        ))}
      </RowList>
      <div className="px-3 pb-3"><TaskProgress taskId={toggle.data?.taskId} /></div>
      {panelError ? <div className="p-3"><Callout tone="danger" title={failureDetail(panelError, t('common.requestFailed'))} /></div> : null}
      <FormDialog
        open={adding}
        onOpenChange={setAdding}
        title={t('access.ssh.addKey')}
        submitLabel={t('access.ssh.addKey')}
        success={t('access.ssh.keyAdded')}
        failure={t('access.ssh.addKey')}
        onSubmit={add}
      >
        <FormField label={t('access.ssh.authorizedKey')}>
          {(id) => <Input id={id} className="font-mono" value={key} onChange={(event) => setKey(event.target.value)} required placeholder={t('access.ssh.keyPlaceholder')} />}
        </FormField>
      </FormDialog>
    </CollectionPanel>
  )
}

function RootPanel() {
  const { t } = useTranslation()
  const [password, setPassword] = useState('')
  const [confirming, setConfirming] = useState(false)
  const transient = useMutationFeedback<TaskAccepted>({
    mutationFn: () => api<TaskAccepted>('/api/v1/actions/transient-root-password', json('POST', { password })),
    success: t('access.ssh.transientAccepted'),
    failure: t('access.ssh.setUntilReboot'),
    onSuccess: () => setPassword(''),
  })
  return (
    <Panel className="border-destructive">
      <div className="flex flex-wrap items-center gap-2.5 text-sm">
        <StatusBadge tone="danger">{t('access.root.highPrivilege')}</StatusBadge>
        <span>{t('access.root.state')}</span>
      </div>
      <form className="grid gap-4" onSubmit={(event) => { event.preventDefault(); setConfirming(true) }}>
        <FormField label={t('access.root.password')} hint={t('access.ssh.transientHint')}>
          {(id) => <Input id={id} autoComplete="new-password" minLength={8} maxLength={72} type="password" value={password} onChange={(event) => setPassword(event.target.value)} required />}
        </FormField>
        <Button className="justify-self-end" type="submit" variant="destructive" disabled={transient.isPending}>
          {transient.isPending ? t('access.ssh.setting') : t('access.ssh.setUntilReboot')}
        </Button>
      </form>
      <TaskProgress taskId={transient.data?.taskId} />
      {/* Driven by the form's submit rather than by a trigger of its own: the
          operator types the password first, then confirms what it will do. */}
      <ConfirmDialog
        open={confirming}
        onOpenChange={setConfirming}
        title={t('access.root.title')}
        description={t('access.ssh.confirmTransient')}
        confirmLabel={t('access.ssh.setUntilReboot')}
        success={t('access.ssh.transientAccepted')}
        failure={t('access.ssh.setUntilReboot')}
        onConfirm={() => transient.mutateAsync()}
      />
    </Panel>
  )
}
