import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Box, Cpu, PackageOpen, Thermometer } from 'lucide-react'
import { api, errorMessage } from '@/lib/api'
import type { AvailableFact, SystemInformation, SystemTelemetry, ThermalReading, WatchdogDevice } from '@/lib/types'
import { Card, CardHeader } from '@/components/ui/card'
import { Unavailable, join } from '@/shared/components/fact'

export function InformationPanel() {
  const { t } = useTranslation()
  const information = useQuery({
    queryKey: ['system-information'],
    queryFn: () => api<SystemInformation>('/api/v1/system/info'),
    retry: false,
  })
  const value = information.data
  return (
    <div className="stack">
      {information.isPending ? <p className="callout" role="status">{t('system.information.loading')}</p> : null}
      {information.error ? <p className="callout error" role="alert">{errorMessage(information.error, t('common.requestFailed'))}</p> : null}
      {value ? (
        <>
          <div className="split-grid">
            <Card>
              <CardHeader title={t('system.information.identity.title')} description={t('system.information.identity.description')} action={<Cpu className="size-5 text-muted-foreground" />} />
              <dl className="details">
                <FactRow label={t('system.information.identity.machineId')} fact={value.machineId} value={value.machineId.id} />
                <FactRow label={t('system.information.identity.board')} fact={value.board} value={join([value.board.model, value.board.source])} />
                <FactRow label={t('system.information.identity.release')} fact={value.release} value={join([value.release.prettyName ?? value.release.name, value.release.imageVersion ?? value.release.versionId])} />
                <FactRow label={t('system.information.identity.kernel')} fact={value.kernel} value={join([value.kernel.release, value.kernel.version])} />
              </dl>
            </Card>
            <Card>
              <CardHeader title={t('system.information.software.title')} description={t('system.information.software.description')} action={<Box className="size-5 text-muted-foreground" />} />
              <dl className="details">
                <FactRow label={t('system.information.software.system')} fact={value.system} value={systemSummary(value)} />
                <FactRow label={t('system.information.software.commitDate')} fact={value.system.commitDate ?? value.system} value={value.system.commitDate?.date} />
                <FactRow label={t('system.information.software.daemon')} fact={value.daemon} value={join([value.daemon.name, value.daemon.version, value.daemon.commit])} />
                <FactRow label={t('system.information.software.slot')} fact={value.slot} value={join([value.slot.booted, value.slot.bootname, value.slot.bootStatus, value.slot.primary ? t('system.information.software.primary') : undefined])} />
                <FactRow label={t('system.information.software.uptime')} fact={value.uptime} value={value.uptime.seconds === undefined ? undefined : formatUptime(value.uptime.seconds, t)} />
              </dl>
            </Card>
          </div>
          <TelemetryPanel />
          <Card>
            <CardHeader title={t('system.information.packages.title')} description={t('system.information.packages.description')} action={<PackageOpen className="size-5 text-muted-foreground" />} />
            {!value.packages.available ? <p className="callout warning" role="status"><Unavailable fact={value.packages} /></p> : (
              <>
                <p className="mb-4 text-sm text-muted-foreground">{t('system.information.packages.summary', { count: value.packages.count ?? 0, mosCount: value.packages.mosCount ?? 0 })}</p>
                {value.packages.entries?.length ? (
                  <div className="data-table-wrap">
                    <table className="data-table">
                      <thead><tr><th>{t('system.information.packages.name')}</th><th>{t('system.information.packages.version')}</th><th>{t('system.information.packages.architecture')}</th></tr></thead>
                      <tbody>{value.packages.entries.map((entry) => <tr key={`${entry.name}-${entry.architecture}`}><td className="mono-cell">{entry.name}</td><td className="mono-cell">{entry.version}</td><td>{entry.architecture}</td></tr>)}</tbody>
                    </table>
                  </div>
                ) : <p className="empty">{t('system.information.packages.empty')}</p>}
                {value.packages.truncated ? <p className="callout warning" role="status">{t('system.information.packages.truncated')}</p> : null}
                {(value.packages.malformedRows ?? 0) > 0 ? <p className="callout warning" role="status">{t('system.information.packages.malformed', { count: value.packages.malformedRows })}</p> : null}
              </>
            )}
          </Card>
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
    <Card>
      <CardHeader title={t('system.information.telemetry.title')} description={t('system.information.telemetry.description')} action={<Thermometer className="size-5 text-muted-foreground" />} />
      {telemetry.error ? <p className="callout error" role="alert">{errorMessage(telemetry.error, t('common.requestFailed'))}</p> : null}
      {value ? (
        <dl className="details">
          <FactRow label={t('system.information.telemetry.thermal')} fact={value.thermal} value={join([...(value.thermal.zones ?? []), ...(value.thermal.hwmon ?? [])].map(formatReading))} />
          <FactRow label={t('system.information.telemetry.watchdog')} fact={value.watchdog} value={join((value.watchdog.devices ?? []).map((device) => watchdogSummary(device, t)))} />
          <FactRow label={t('system.information.telemetry.reset')} fact={value.reset} value={join([t(`system.information.telemetry.reason.${value.reset.reason}`), value.reset.detail])} />
        </dl>
      ) : null}
    </Card>
  )
}

function FactRow({ label, fact, value }: { label: string; fact: AvailableFact; value?: string }) {
  const { t } = useTranslation()
  return <div><dt>{label}</dt><dd>{fact.available ? (value ?? t('common.notAvailable')) : <Unavailable fact={fact} />}</dd></div>
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
