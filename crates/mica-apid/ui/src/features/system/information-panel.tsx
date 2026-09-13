import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Box, Cpu, PackageOpen, Thermometer } from 'lucide-react'
import { api } from '@/shared/lib/http'
import type { AvailableFact, SystemInformation, SystemTelemetry, ThermalReading, WatchdogDevice } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { DataTable } from '@/shared/components/data-table'
import { FactList, type Fact } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'
import { Spinner } from '@/shared/components/ui/spinner'
import { failureDetail } from '@/shared/feedback/toast'
import { Unavailable, join } from '@/shared/components/fact'

type PackageEntry = NonNullable<SystemInformation['packages']['entries']>[number]

export function InformationPanel() {
  const { t } = useTranslation()
  const information = useQuery({
    queryKey: ['system-information'],
    queryFn: () => api<SystemInformation>('/api/v1/system/info'),
    retry: false,
  })
  const value = information.data
  return (
    <div className="grid gap-3">
      {information.isPending ? <p className="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />{t('system.information.loading')}</p> : null}
      {information.error ? <Callout tone="danger" title={failureDetail(information.error, t('common.requestFailed'))} /> : null}
      {value ? (
        <>
          <div className="grid gap-3 lg:grid-cols-2">
            <Panel title={t('system.information.identity.title')} description={t('system.information.identity.description')} action={<Cpu className="size-5 text-muted-foreground" />}>
              <FactList facts={[
                fact('machineId', t('system.information.identity.machineId'), value.machineId, value.machineId.id, t),
                fact('board', t('system.information.identity.board'), value.board, join([value.board.model, value.board.source]), t),
                fact('release', t('system.information.identity.release'), value.release, join([value.release.prettyName ?? value.release.name, value.release.imageVersion ?? value.release.versionId]), t),
                fact('kernel', t('system.information.identity.kernel'), value.kernel, join([value.kernel.release, value.kernel.version]), t),
              ]} />
            </Panel>
            <Panel title={t('system.information.software.title')} description={t('system.information.software.description')} action={<Box className="size-5 text-muted-foreground" />}>
              <FactList facts={[
                fact('system', t('system.information.software.system'), value.system, systemSummary(value), t),
                fact('commitDate', t('system.information.software.commitDate'), value.system.commitDate ?? value.system, value.system.commitDate?.date, t),
                fact('daemon', t('system.information.software.daemon'), value.daemon, join([value.daemon.name, value.daemon.version, value.daemon.commit]), t),
                fact('deployment', t('system.information.software.deployment'), value.deployment, join([value.deployment.id, value.deployment.version]), t),
                fact('uptime', t('system.information.software.uptime'), value.uptime, value.uptime.seconds === undefined ? undefined : formatUptime(value.uptime.seconds, t), t),
              ]} />
            </Panel>
          </div>
          <TelemetryPanel />
          <Panel title={t('system.information.packages.title')} description={t('system.information.packages.description')} action={<PackageOpen className="size-5 text-muted-foreground" />} contentClassName="gap-3">
            {!value.packages.available ? <Callout tone="warning"><Unavailable fact={value.packages} /></Callout> : (
              <>
                <p className="text-sm text-muted-foreground">{t('system.information.packages.summary', { count: value.packages.count ?? 0, mosCount: value.packages.mosCount ?? 0 })}</p>
                <DataTable<PackageEntry>
                  rows={value.packages.entries ?? []}
                  rowKey={(entry) => `${entry.name}-${entry.architecture}`}
                  empty={t('system.information.packages.empty')}
                  columns={[
                    { id: 'name', header: t('system.information.packages.name'), cell: (entry) => <span className="font-mono text-[0.8125rem]">{entry.name}</span> },
                    { id: 'version', header: t('system.information.packages.version'), cell: (entry) => <span className="font-mono text-[0.8125rem]">{entry.version}</span> },
                    { id: 'architecture', header: t('system.information.packages.architecture'), cell: (entry) => entry.architecture },
                  ]}
                />
                {value.packages.truncated ? <Callout tone="warning" title={t('system.information.packages.truncated')} /> : null}
                {(value.packages.malformedRows ?? 0) > 0 ? <Callout tone="warning" title={t('system.information.packages.malformed', { count: value.packages.malformedRows })} /> : null}
              </>
            )}
          </Panel>
        </>
      ) : null}
    </div>
  )
}

export function TelemetryPanel() {
  const { t } = useTranslation()
  const telemetry = useQuery({
    queryKey: ['system-telemetry'],
    queryFn: () => api<SystemTelemetry>('/api/v1/system/telemetry'),
    retry: false,
  })
  const value = telemetry.data
  return (
    <Panel title={t('system.information.telemetry.title')} description={t('system.information.telemetry.description')} action={<Thermometer className="size-5 text-muted-foreground" />}>
      {telemetry.error ? <Callout tone="danger" title={failureDetail(telemetry.error, t('common.requestFailed'))} /> : null}
      {value ? (
        <FactList facts={[
          fact('thermal', t('system.information.telemetry.thermal'), value.thermal, join([...(value.thermal.zones ?? []), ...(value.thermal.hwmon ?? [])].map(formatReading)), t),
          fact('watchdog', t('system.information.telemetry.watchdog'), value.watchdog, join((value.watchdog.devices ?? []).map((device) => watchdogSummary(device, t))), t),
          fact('reset', t('system.information.telemetry.reset'), value.reset, join([t(`system.information.telemetry.reason.${value.reset.reason}`), value.reset.detail]), t),
        ]} />
      ) : null}
    </Panel>
  )
}

/// An observation the device either made or could not make. The unavailable
/// case keeps its own reason rather than collapsing to a dash, because "not
/// observed" and "observed as nothing" are different answers.
function fact(id: string, label: string, source: AvailableFact, value: string | undefined, t: ReturnType<typeof useTranslation>['t']): Fact {
  return { id, label, value: source.available ? (value ?? t('common.notAvailable')) : <Unavailable fact={source} /> }
}

function formatReading(reading: ThermalReading) {
  return `${reading.label ?? reading.sensor} ${(reading.milliCelsius / 1000).toFixed(1)} °C`
}

function watchdogSummary(device: WatchdogDevice, t: ReturnType<typeof useTranslation>['t']) {
  const bootstatus = device.bootstatus.available
    ? (device.bootstatus.flags?.length ? device.bootstatus.flags.join(', ') : t('system.information.telemetry.noResetFlags'))
    : t('common.unavailable')
  return join([device.device, device.identity, device.state, device.timeoutSeconds === undefined ? undefined : t('system.information.telemetry.timeout', { seconds: device.timeoutSeconds }), bootstatus]) ?? device.device
}

/// The image's own provenance: version, package and the git stamp with its
/// consistency verdict. An inconsistent stamp is stated, because it means the
/// image was assembled from more than one tree.
///
/// The date is NOT here: it is a fact of its own with an absence of its own,
/// so it gets a row rather than a position in a joined line that would drop it
/// without a word. The row falls back to the `system` member's own absence
/// reason when there is no manifest at all -- the reason there is no commit
/// date is then the reason there is no system member either.
function systemSummary(value: SystemInformation) {
  const stamp = value.system.gitStamp
  const git = stamp?.commit ? `git ${stamp.commit}${stamp.dirty ? ' (dirty)' : ''}${stamp.consistent ? ' (consistent)' : ' (inconsistent)'}` : undefined
  return join([value.system.version, value.system.package, git])
}

function formatUptime(seconds: number, t: ReturnType<typeof useTranslation>['t']) {
  const days = Math.floor(seconds / 86_400)
  const hours = Math.floor((seconds % 86_400) / 3_600)
  const minutes = Math.floor((seconds % 3_600) / 60)
  return days > 0
    ? t('system.information.software.uptimeDays', { days, hours, minutes })
    : t('system.information.software.uptimeHours', { hours, minutes })
}
