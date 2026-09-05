import { useState, type FormEvent } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { CalendarClock, Timer } from 'lucide-react'
import { api, errorMessage, json } from '@/lib/api'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Field } from '@/shared/components/field'
import { Input } from '@/shared/components/ui/input'
import { Surface } from '@/shared/components/product-layout'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/shared/components/ui/select'

/// The **resolved** policy, as `GET /api/v1/update` reports it under
/// `lifecycle.policy`: PLAN-070 §5.1's precedence already applied, which is
/// what the device is actually following. Which layer each value came from is
/// the provisioning-status route's answer and this panel never re-derives it.
///
/// All four overridable values are `null` when the operator document did not
/// load — a device whose configuration is unreadable does not know which
/// channel it is on, and `policy_error` beside them is what says so.
interface ResolvedPolicy {
  policy?: string | null
  checkIntervalMinutes?: number | null
  sourceUrl?: string | null
  channel?: string | null
  rebootPolicy?: string
  maintenanceWindows?: MaintenanceWindow[]
}

interface MaintenanceWindow {
  days?: string[]
  start?: string
  end?: string
}

interface Deferral {
  reason?: string
  detail?: string
  waitedSeconds?: number
  attempts?: number
}

interface UpdatePolicyDoc {
  lifecycle?: {
    policy?: ResolvedPolicy
    policy_error?: string
    deferred?: Deferral
  }
}

/// The three layers of `GET /api/v1/provisioning/status` (PLAN-070 §8), for
/// the source address as well as the channel (PLAN-071 §10).
///
/// A key the operator never wrote is **absent** from `operator` and one they
/// wrote as `null` is `null`. Both resolve to the baked default and they are
/// two different statements, so this panel reports "from the image" only for
/// the first.
interface ProvisioningLayers {
  baked?: { update?: { source?: string | null; channel?: string; policy?: string } }
  operator?: { update?: { source?: string | null; channel?: string | null; policy?: string | null } }
  effective?: { update?: { source?: string | null; channel?: string; policy?: string } }
}

/// The patch `POST /api/v1/update/config` takes: only the keys being changed.
///
/// A key omitted here is left as it is on the device, and an explicit `null`
/// clears an override so the image's default applies again. Sending the
/// resolved values back instead would pin those defaults into the operator's
/// layer, and the device would stop following its image on the day the image
/// changed — which is why the address and channel fields below are seeded from
/// the operator layer and not from the effective one.
interface UpdateConfigPatch {
  policy?: string
  checkIntervalMinutes?: number
  rebootPolicy?: string
  source?: { url?: string | null; channel?: string | null }
  maintenance?: { windows: { days: string[]; start: string; end: string }[] }
}

const POLICIES = ['off', 'check', 'auto'] as const
const REBOOT_POLICIES = ['manual', 'window'] as const

type WriteMutation = ReturnType<typeof useMutation<unknown, Error, UpdateConfigPatch>>

export function AutomaticUpdatesPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  // The same query key the rest of the update tab reads: one document, one
  // fetch, and no second answer about the same policy.
  const state = useQuery({ queryKey: ['update-state'], queryFn: () => api<UpdatePolicyDoc>('/api/v1/update') })
  const layers = useQuery({ queryKey: ['provisioning-status'], queryFn: () => api<ProvisioningLayers>('/api/v1/provisioning/status') })
  const write = useMutation({
    mutationFn: (patch: UpdateConfigPatch) => api<unknown>('/api/v1/update/config', json('POST', patch)),
    onSuccess: () => {
      // Both reads move: the resolved policy, and which layer it came from.
      queryClient.invalidateQueries({ queryKey: ['update-state'] })
      queryClient.invalidateQueries({ queryKey: ['provisioning-status'] })
    },
  })
  const policy = state.data?.lifecycle?.policy
  return (
    <div className="stack">
      <SourcePanel layers={layers.data} error={layers.error} write={write} />
      <PolicyPanel policy={policy} write={write} />
      <WindowsPanel windows={policy?.maintenanceWindows} write={write} />
      <DeferralNotice deferred={state.data?.lifecycle?.deferred} />
      {state.data?.lifecycle?.policy_error ? <p className="callout error" role="alert">{t('system.update.automatic.documentError', { reason: state.data.lifecycle.policy_error })}</p> : null}
      {state.error ? <p className="callout error" role="alert">{errorMessage(state.error, t('common.requestFailed'))}</p> : null}
      {write.error ? <p className="callout error" role="alert">{errorMessage(write.error, t('common.requestFailed'))}</p> : null}
    </div>
  )
}

/// Where this device looks for updates: the address and the channel, each read
/// as baked / operator / effective, and each writable.
///
/// **This panel is PLAN-071 U7's gate**: a re-pointed device shows the address
/// it is dialling and that it is not the baked one. Both halves are needed —
/// the effective value alone cannot be told apart from the image's, and the
/// baked value alone is not what the device is doing.
///
/// The two fields hold the **operator's** value, not the effective one, and an
/// empty field is `null`: clearing the box returns the device to the image's
/// default rather than writing that default back as an override.
function SourcePanel({ layers, error, write }: { layers?: ProvisioningLayers; error: unknown; write: WriteMutation }) {
  const { t } = useTranslation()
  const baked = layers?.baked?.update
  const operator = layers?.operator?.update
  const effective = layers?.effective?.update
  const [url, setUrl] = useState<string>()
  const [channel, setChannel] = useState<string>()
  const currentUrl = url ?? operator?.source ?? ''
  const currentChannel = channel ?? operator?.channel ?? ''
  const rows = [
    {
      id: 'source',
      label: t('system.update.automatic.source'),
      effective: effective?.source ?? undefined,
      baked: baked?.source ?? undefined,
      overridden: operator !== undefined && 'source' in operator,
    },
    {
      id: 'channel',
      label: t('system.update.automatic.channel'),
      effective: effective?.channel,
      baked: baked?.channel,
      overridden: operator !== undefined && 'channel' in operator,
    },
  ]
  return (
    <Card>
      <CardHeader title={t('system.update.automatic.sourceTitle')} description={t('system.update.automatic.sourceDescription')} action={<CalendarClock className="size-5 text-muted-foreground" />} />
      <dl className="details">
        {rows.map((row) => (
          <div key={row.id}>
            <dt>{row.label}</dt>
            <dd>
              <span className="mono">{row.effective ?? t('system.update.automatic.noSource')}</span>
              <small>
                {row.overridden
                  ? t('system.update.automatic.overridden', { baked: row.baked ?? t('system.update.automatic.noSource') })
                  : t('system.update.automatic.fromImage')}
              </small>
            </dd>
          </div>
        ))}
      </dl>
      <form
        className="grid gap-4"
        onSubmit={(event: FormEvent) => {
          event.preventDefault()
          write.mutate({ source: { url: currentUrl.trim() || null, channel: currentChannel.trim() || null } })
        }}
      >
        <div className="content-grid">
          <Field label={t('system.update.automatic.urlLabel')} hint={t('system.update.automatic.urlHint')}>
            <Input value={currentUrl} onChange={(event) => setUrl(event.target.value)} placeholder={baked?.source ?? 'https://updates.example/repo'} />
          </Field>
          <Field label={t('system.update.automatic.channelLabel')} hint={t('system.update.automatic.channelHint')}>
            <Input value={currentChannel} onChange={(event) => setChannel(event.target.value)} placeholder={baked?.channel ?? 'stable'} />
          </Field>
        </div>
        <Button type="submit" disabled={write.isPending}>{t('system.update.automatic.saveSource')}</Button>
      </form>
      {error ? <p className="callout error" role="alert">{errorMessage(error, t('common.requestFailed'))}</p> : null}
    </Card>
  )
}

/// What the device does on its own: the mode, the cadence and what happens
/// after an automatic install (PLAN-071 §1, §3).
///
/// These three are written explicitly, unlike the address and channel above:
/// choosing a mode in a selector IS the operator deciding, so recording it in
/// their layer is what they meant.
function PolicyPanel({ policy, write }: { policy?: ResolvedPolicy; write: WriteMutation }) {
  const { t } = useTranslation()
  const [mode, setMode] = useState<string>()
  const [interval, setInterval] = useState<string>()
  const [rebootPolicy, setRebootPolicy] = useState<string>()
  const currentMode = mode ?? policy?.policy ?? 'check'
  const currentInterval = interval ?? (policy?.checkIntervalMinutes ?? 1440).toString()
  const currentReboot = rebootPolicy ?? policy?.rebootPolicy ?? 'manual'
  const knownMode = POLICIES.find((known) => known === currentMode)
  const knownReboot = REBOOT_POLICIES.find((known) => known === currentReboot)
  return (
    <Card>
      <CardHeader title={t('system.update.automatic.policyTitle')} description={t('system.update.automatic.policyDescription')} action={<Timer className="size-5 text-muted-foreground" />} />
      <form
        className="grid gap-4"
        onSubmit={(event: FormEvent) => {
          event.preventDefault()
          write.mutate({
            policy: currentMode,
            checkIntervalMinutes: Number(currentInterval),
            rebootPolicy: currentReboot,
          })
        }}
      >
        <div className="content-grid">
          <Field label={t('system.update.automatic.mode')} hint={knownMode ? t(`system.update.automatic.modes.${knownMode}`) : undefined}>
            <Select value={currentMode} onValueChange={(value) => setMode(String(value))}>
              <SelectTrigger aria-label={t('system.update.automatic.mode')}><SelectValue /></SelectTrigger>
              <SelectContent>{POLICIES.map((option) => <SelectItem value={option} key={option}>{t(`system.update.automatic.modeNames.${option}`)}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
          <Field label={t('system.update.automatic.interval')} hint={t('system.update.automatic.intervalHint')}>
            <Input type="number" min={0} value={currentInterval} onChange={(event) => setInterval(event.target.value)} required />
          </Field>
          <Field label={t('system.update.automatic.rebootPolicy')} hint={knownReboot ? t(`system.update.automatic.rebootPolicies.${knownReboot}`) : undefined}>
            <Select value={currentReboot} onValueChange={(value) => setRebootPolicy(String(value))}>
              <SelectTrigger aria-label={t('system.update.automatic.rebootPolicy')}><SelectValue /></SelectTrigger>
              <SelectContent>{REBOOT_POLICIES.map((option) => <SelectItem value={option} key={option}>{t(`system.update.automatic.rebootPolicyNames.${option}`)}</SelectItem>)}</SelectContent>
            </Select>
          </Field>
        </div>
        <Button type="submit" disabled={write.isPending}>{t('system.update.automatic.save')}</Button>
      </form>
    </Card>
  )
}

/// The maintenance windows, edited as the list the document holds.
///
/// A list rather than one window because that is what the document carries: an
/// editor showing only the first would silently delete the rest on the next
/// save. Days are the document's own `mon`..`sun`, comma-separated, and no day
/// means every day. The device validates both and refuses with the offending
/// value named, so this form does not restate the rule — including §2's, that
/// `auto` needs a window at all.
function WindowsPanel({ windows, write }: { windows?: MaintenanceWindow[]; write: WriteMutation }) {
  const { t } = useTranslation()
  const [draft, setDraft] = useState<{ days: string; start: string; end: string }[]>()
  const rows = draft ?? (windows ?? []).map((window) => ({
    days: (window.days ?? []).join(', '),
    start: window.start ?? '',
    end: window.end ?? '',
  }))
  const change = (index: number, key: 'days' | 'start' | 'end', value: string) =>
    setDraft(rows.map((row, at) => at === index ? { ...row, [key]: value } : row))
  return (
    <Card>
      <CardHeader title={t('system.update.automatic.windowsTitle')} description={t('system.update.automatic.windowsDescription')} />
      <form
        className="grid gap-4"
        onSubmit={(event: FormEvent) => {
          event.preventDefault()
          write.mutate({
            maintenance: {
              windows: rows.map((row) => ({
                days: row.days.split(',').map((day) => day.trim().toLowerCase()).filter(Boolean),
                start: row.start.trim(),
                end: row.end.trim(),
              })),
            },
          })
        }}
      >
        {rows.length === 0 ? <p className="empty">{t('system.update.automatic.noWindows')}</p> : null}
        {rows.map((row, index) => (
          <div className="content-grid" key={index}>
            <Field label={t('system.update.automatic.windowDays')} hint={t('system.update.automatic.windowDaysHint')}>
              <Input value={row.days} onChange={(event) => change(index, 'days', event.target.value)} placeholder="mon, thu" />
            </Field>
            <Field label={t('system.update.automatic.windowStart')}>
              <Input value={row.start} onChange={(event) => change(index, 'start', event.target.value)} placeholder="02:00" required />
            </Field>
            <Field label={t('system.update.automatic.windowEnd')}>
              <Input value={row.end} onChange={(event) => change(index, 'end', event.target.value)} placeholder="04:00" required />
            </Field>
            <Button type="button" variant="outline" onClick={() => setDraft(rows.filter((_, at) => at !== index))}>{t('system.update.automatic.removeWindow')}</Button>
          </div>
        ))}
        <div className="flex flex-wrap gap-3">
          <Button type="button" variant="secondary" onClick={() => setDraft([...rows, { days: '', start: '02:00', end: '04:00' }])}>{t('system.update.automatic.addWindow')}</Button>
          <Button type="submit" disabled={write.isPending}>{t('system.update.automatic.saveWindows')}</Button>
        </div>
      </form>
    </Card>
  )
}

/// What the automatic path refused, and for how long (PLAN-071 §2, U5).
///
/// A deferral is a fact about the automatic path and not a state of the
/// machine, so it is rendered beside the policy rather than as the device's
/// status. The reason is named in the console's language; the detail is the
/// device's own sentence and is shown verbatim, because it carries the values
/// — a channel name, a window, a version — that a translation cannot.
function DeferralNotice({ deferred }: { deferred?: Deferral }) {
  const { t } = useTranslation()
  if (!deferred?.reason) return null
  // A reason outside the vocabulary is rendered as the device sent it rather
  // than as one of these, so a refusal a newer daemon adds reads as itself in
  // an older console instead of as the wrong sentence.
  const known = DEFERRAL_REASONS.find((reason) => reason === deferred.reason)
  return (
    <Surface className="surface-compact">
      <p className="callout warning" role="status">
        {t('system.update.automatic.deferred', {
          reason: known ? t(`system.update.automatic.deferrals.${known}`) : deferred.reason,
        })}
      </p>
      {deferred.detail ? <p className="field-hint">{deferred.detail}</p> : null}
      <p className="field-hint">
        {t('system.update.automatic.deferredSince', {
          minutes: Math.round((deferred.waitedSeconds ?? 0) / 60),
          attempts: deferred.attempts ?? 1,
        })}
      </p>
    </Surface>
  )
}

/// The vocabulary `update_auto.rs` records.
const DEFERRAL_REASONS = [
  'check-refused',
  'clock-untrusted',
  'fetch-refused',
  'install-refused',
  'no-newer-release',
  'outside-window',
  'reboot-gate-closed',
  'reboot-pending',
  'recheck-failed',
  'recheck-refused',
  'slot-status-unknown',
  'superseded',
  'suppression-unreadable',
  'version-suppressed',
  'workspace-unready',
] as const
