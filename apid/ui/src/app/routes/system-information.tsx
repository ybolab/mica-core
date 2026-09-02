import { createFileRoute } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Box, Cpu, PackageOpen } from 'lucide-react'
import type { AvailableFact, SystemInformation } from '@/lib/types'
import { useSystemInformation } from '@/lib/diagnostics'
import { errorMessage } from '@/lib/api'
import { Card, CardHeader } from '@/components/ui/card'

export function SystemInformationPage() {
  const { t } = useTranslation()
  const information = useSystemInformation()
  const value = information.data
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('systemInformation.eyebrow')}</p><h1>{t('systemInformation.title')}</h1><p>{t('systemInformation.description')}</p></div></header>
      {information.isPending ? <p className="callout" role="status">{t('systemInformation.loading')}</p> : null}
      {information.error ? <p className="callout error" role="alert">{errorMessage(information.error, t('common.requestFailed'))}</p> : null}
      {value ? (
        <>
          <div className="split-grid">
            <Card>
              <CardHeader title={t('systemInformation.identity.title')} description={t('systemInformation.identity.description')} action={<Cpu className="size-5 text-muted-foreground" />} />
              <dl className="details">
                <FactRow label={t('systemInformation.identity.machineId')} fact={value.machineId} value={value.machineId.id} />
                <FactRow label={t('systemInformation.identity.board')} fact={value.board} value={join([value.board.model, value.board.source])} />
                <FactRow label={t('systemInformation.identity.release')} fact={value.release} value={join([value.release.prettyName ?? value.release.name, value.release.imageVersion ?? value.release.versionId])} />
                <FactRow label={t('systemInformation.identity.kernel')} fact={value.kernel} value={join([value.kernel.release, value.kernel.version])} />
              </dl>
            </Card>
            <Card>
              <CardHeader title={t('systemInformation.software.title')} description={t('systemInformation.software.description')} action={<Box className="size-5 text-muted-foreground" />} />
              <dl className="details">
                <FactRow label={t('systemInformation.software.system')} fact={value.system} value={systemSummary(value)} />
                <FactRow label={t('systemInformation.software.daemon')} fact={value.daemon} value={join([value.daemon.name, value.daemon.version, value.daemon.commit])} />
                <FactRow label={t('systemInformation.software.slot')} fact={value.slot} value={slotSummary(value)} />
                <FactRow label={t('systemInformation.software.uptime')} fact={value.uptime} value={value.uptime.seconds === undefined ? undefined : formatUptime(value.uptime.seconds)} />
              </dl>
            </Card>
          </div>
          <Card>
            <CardHeader title={t('systemInformation.packages.title')} description={t('systemInformation.packages.description')} action={<PackageOpen className="size-5 text-muted-foreground" />} />
            {!value.packages.available ? <Unavailable fact={value.packages} /> : (
              <>
                <p className="mb-4 text-sm text-muted-foreground">{t('systemInformation.packages.summary', { count: value.packages.count ?? 0, mosCount: value.packages.mosCount ?? 0 })}</p>
                <div className="overflow-x-auto">
                  <table className="w-full text-left text-sm">
                    <thead><tr className="border-b"><th className="pb-2 font-medium">{t('systemInformation.packages.name')}</th><th className="pb-2 font-medium">{t('systemInformation.packages.version')}</th><th className="pb-2 font-medium">{t('systemInformation.packages.architecture')}</th></tr></thead>
                    <tbody>{value.packages.entries?.map((entry) => <tr key={`${entry.name}-${entry.architecture}`} className="border-b last:border-0"><td className="py-2 font-mono">{entry.name}</td><td className="py-2 font-mono">{entry.version}</td><td className="py-2">{entry.architecture}</td></tr>)}</tbody>
                  </table>
                </div>
                {value.packages.truncated ? <p className="callout warning" role="status">{t('systemInformation.packages.truncated')}</p> : null}
                {(value.packages.malformedRows ?? 0) > 0 ? <p className="callout warning" role="status">{t('systemInformation.packages.malformed', { count: value.packages.malformedRows })}</p> : null}
              </>
            )}
          </Card>
        </>
      ) : null}
    </div>
  )
}

function FactRow({ label, fact, value }: { label: string; fact: AvailableFact; value?: string }) {
  return <div><dt>{label}</dt><dd>{fact.available ? (value ?? '—') : <Unavailable fact={fact} />}</dd></div>
}

function Unavailable({ fact }: { fact: AvailableFact }) {
  const { t } = useTranslation()
  return <span>{t('systemInformation.unavailable')}{fact.detail ? <small className="ml-2 text-muted-foreground">{fact.detail}</small> : null}</span>
}

function join(values: (string | null | undefined)[]) {
  const present = values.filter((value): value is string => Boolean(value))
  return present.length > 0 ? present.join(' · ') : undefined
}

function systemSummary(value: SystemInformation) {
  const stamp = value.system.gitStamp
  const git = stamp?.commit ? `git ${stamp.commit}${stamp.dirty ? ' (dirty)' : ''}${stamp.consistent ? ' (consistent)' : ' (inconsistent)'}` : undefined
  return join([value.system.version, value.system.package, git, value.system.buildDate])
}

function slotSummary(value: SystemInformation) {
  return join([value.slot.booted, value.slot.bootname, value.slot.bootStatus, value.slot.primary ? 'primary' : undefined])
}

function formatUptime(seconds: number) {
  const days = Math.floor(seconds / 86_400)
  const hours = Math.floor((seconds % 86_400) / 3_600)
  const minutes = Math.floor((seconds % 3_600) / 60)
  return [days ? `${days}d` : '', hours ? `${hours}h` : '', `${minutes}m`].filter(Boolean).join(' ')
}

export const Route = createFileRoute('/system-information')({ component: SystemInformationPage })
