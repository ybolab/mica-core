import { createFileRoute } from '@tanstack/react-router'
import { useTranslation } from 'react-i18next'
import { Download, FileArchive, ShieldCheck, Trash2 } from 'lucide-react'
import { errorMessage } from '@/lib/api'
import { useCollectDiagnosticSnapshot, useDeleteDiagnosticSnapshot, useDiagnosticSnapshots } from '@/lib/diagnostics'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'

export function DiagnosticsPage() {
  const { t } = useTranslation()
  const snapshots = useDiagnosticSnapshots()
  const collect = useCollectDiagnosticSnapshot()
  const remove = useDeleteDiagnosticSnapshot()
  const retention = snapshots.data?.retention
  return (
    <div className="page">
      <header className="page-head"><div><p className="eyebrow">{t('diagnostics.eyebrow')}</p><h1>{t('diagnostics.title')}</h1><p>{t('diagnostics.description')}</p></div></header>
      <div className="split-grid">
        <Card>
          <CardHeader title={t('diagnostics.collection.title')} description={t('diagnostics.collection.description')} action={<FileArchive className="size-5 text-muted-foreground" />} />
          <Button onClick={() => collect.mutate()} disabled={collect.isPending}>{collect.isPending ? t('diagnostics.collection.pending') : t('diagnostics.collection.generate')}</Button>
          {collect.data ? <p className="callout" role="status">{t('diagnostics.collection.complete', { id: collect.data.snapshot.id, elapsed: collect.data.elapsedMillis, dropped: collect.data.droppedFields, redacted: collect.data.redactedFields })}</p> : null}
          {collect.error ? <p className="callout error" role="alert">{errorMessage(collect.error, t('common.requestFailed'))}</p> : null}
        </Card>
        <Card>
          <CardHeader title={t('diagnostics.retention.title')} description={t('diagnostics.retention.description')} action={<ShieldCheck className="size-5 text-muted-foreground" />} />
          {retention ? (
            <dl className="details">
              <div><dt>{t('diagnostics.retention.count')}</dt><dd>{t('diagnostics.retention.snapshots', { count: retention.maxSnapshots })}</dd></div>
              <div><dt>{t('diagnostics.retention.total')}</dt><dd>{t('diagnostics.retention.totalBytes', { size: formatBytes(retention.maxTotalBytes) })}</dd></div>
              <div><dt>{t('diagnostics.retention.each')}</dt><dd>{t('diagnostics.retention.snapshotBytes', { size: formatBytes(retention.maxSnapshotBytes) })}</dd></div>
              <div><dt>{t('diagnostics.retention.schemas')}</dt><dd>{t('diagnostics.retention.schemaVersions', { snapshot: retention.schemaVersion, redaction: retention.redactionSchemaVersion })}</dd></div>
            </dl>
          ) : null}
        </Card>
      </div>
      <Card>
        <CardHeader title={t('diagnostics.snapshots.title')} description={t('diagnostics.snapshots.description')} action={<Download className="size-5 text-muted-foreground" />} />
        {snapshots.isPending ? <p className="callout" role="status">{t('diagnostics.snapshots.loading')}</p> : null}
        {snapshots.error ? <p className="callout error" role="alert">{errorMessage(snapshots.error, t('common.requestFailed'))}</p> : null}
        {snapshots.data?.snapshots.length === 0 ? <p className="callout" role="status">{t('diagnostics.snapshots.empty')}</p> : null}
        <div className="grid gap-3">
          {snapshots.data?.snapshots.map((snapshot) => (
            <section key={snapshot.id} className="flex flex-wrap items-center justify-between gap-4 rounded-lg border p-4" aria-label={t('diagnostics.snapshots.snapshot', { id: snapshot.id })}>
              <div><h3 className="font-semibold">{t('diagnostics.snapshots.snapshot', { id: snapshot.id })}</h3><p className="mt-1 text-sm text-muted-foreground">{[snapshot.collectedAt ?? t('common.notAvailable'), formatBytes(snapshot.bytes), snapshot.machineId ?? t('common.notAvailable'), snapshot.schemaVersion === null || snapshot.schemaVersion === undefined ? t('common.notAvailable') : `schema ${snapshot.schemaVersion}`].join(' · ')}</p></div>
              <div className="flex gap-2">
                <a className="inline-flex h-9 items-center gap-2 rounded-md border px-3 text-sm font-medium hover:bg-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring" href={`/api/v1/diagnostics/snapshots/${snapshot.id}`} download aria-label={t('diagnostics.snapshots.downloadLabel', { id: snapshot.id })}><Download className="size-4" />{t('diagnostics.snapshots.download')}</a>
                <Button variant="danger" onClick={() => remove.mutate(snapshot.id)} disabled={remove.isPending} aria-label={t('diagnostics.snapshots.deleteLabel', { id: snapshot.id })}><Trash2 className="size-4" />{t('common.actions.delete')}</Button>
              </div>
            </section>
          ))}
        </div>
        {remove.error ? <p className="callout error" role="alert">{errorMessage(remove.error, t('common.requestFailed'))}</p> : null}
      </Card>
    </div>
  )
}

function formatBytes(bytes: number) {
  const units = ['B', 'KiB', 'MiB', 'GiB']
  let value = bytes
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${unit === 0 ? value : value.toFixed(1)} ${units[unit]}`
}

export const Route = createFileRoute('/diagnostics')({ component: DiagnosticsPage })
