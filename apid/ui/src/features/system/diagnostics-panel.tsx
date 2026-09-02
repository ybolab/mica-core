import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Download, FileArchive, ShieldCheck, Trash2 } from 'lucide-react'
import { ApiError, api, errorMessage } from '@/lib/api'
import type { SnapshotCollected, SnapshotList } from '@/lib/types'
import { Button } from '@/shared/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { formatBytes } from '@/shared/components/fact'
import { AlertDialog, AlertDialogAction, AlertDialogCancel, AlertDialogContent, AlertDialogDescription, AlertDialogFooter, AlertDialogHeader, AlertDialogTitle, AlertDialogTrigger } from '@/shared/components/ui/alert-dialog'

const snapshotsKey = ['diagnostic-snapshots'] as const

export function DiagnosticsPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const snapshots = useQuery({ queryKey: snapshotsKey, queryFn: () => api<SnapshotList>('/api/v1/diagnostics/snapshots'), retry: false })
  const collect = useMutation({
    mutationFn: () => api<SnapshotCollected>('/api/v1/diagnostics/snapshots', { method: 'POST' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: snapshotsKey }),
  })
  const remove = useMutation({
    mutationFn: (id: number) => api<void>(`/api/v1/diagnostics/snapshots/${id}`, { method: 'DELETE' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: snapshotsKey }),
  })
  const retention = snapshots.data?.retention
  // Collection is one at a time. A 409 is not a queue and must not read like
  // one: the operator is told another collection is running and nothing of
  // theirs is pending.
  const busy = collect.error instanceof ApiError && collect.error.status === 409
  return (
    <div className="stack">
      <div className="split-grid">
        <Card>
          <CardHeader title={t('system.diagnostics.collection.title')} description={t('system.diagnostics.collection.description')} action={<FileArchive className="size-5 text-muted-foreground" />} />
          <Button onClick={() => collect.mutate()} disabled={collect.isPending}>{collect.isPending ? t('system.diagnostics.collection.pending') : t('system.diagnostics.collection.generate')}</Button>
          {collect.data ? <p className="callout success" role="status">{t('system.diagnostics.collection.complete', { id: collect.data.snapshot.id, elapsed: collect.data.elapsedMillis, dropped: collect.data.droppedFields, redacted: collect.data.redactedFields })}</p> : null}
          {busy ? <p className="callout warning" role="status">{t('system.diagnostics.collection.busy')}</p> : null}
          {collect.error && !busy ? <p className="callout error" role="alert">{errorMessage(collect.error, t('common.requestFailed'))}</p> : null}
        </Card>
        <Card>
          <CardHeader title={t('system.diagnostics.retention.title')} description={t('system.diagnostics.retention.description')} action={<ShieldCheck className="size-5 text-muted-foreground" />} />
          {retention ? (
            <dl className="details">
              <div><dt>{t('system.diagnostics.retention.count')}</dt><dd>{t('system.diagnostics.retention.snapshots', { count: retention.maxSnapshots })}</dd></div>
              <div><dt>{t('system.diagnostics.retention.total')}</dt><dd>{t('system.diagnostics.retention.totalBytes', { size: formatBytes(retention.maxTotalBytes) })}</dd></div>
              <div><dt>{t('system.diagnostics.retention.each')}</dt><dd>{t('system.diagnostics.retention.snapshotBytes', { size: formatBytes(retention.maxSnapshotBytes) })}</dd></div>
              <div><dt>{t('system.diagnostics.retention.schemas')}</dt><dd>{t('system.diagnostics.retention.schemaVersions', { snapshot: retention.schemaVersion, redaction: retention.redactionSchemaVersion })}</dd></div>
            </dl>
          ) : null}
        </Card>
      </div>
      <Card>
        <CardHeader title={t('system.diagnostics.snapshots.title')} description={t('system.diagnostics.snapshots.description')} action={<Download className="size-5 text-muted-foreground" />} />
        {snapshots.isPending ? <p className="callout" role="status">{t('system.diagnostics.snapshots.loading')}</p> : null}
        {snapshots.error ? <p className="callout error" role="alert">{errorMessage(snapshots.error, t('common.requestFailed'))}</p> : null}
        {snapshots.data?.snapshots.length === 0 ? <p className="empty">{t('system.diagnostics.snapshots.empty')}</p> : null}
        <div className="collection-list">
          {snapshots.data?.snapshots.map((snapshot) => (
            <section key={snapshot.id} className="collection-row" aria-label={t('system.diagnostics.snapshots.snapshot', { id: snapshot.id })}>
              <div>
                <strong>{t('system.diagnostics.snapshots.snapshot', { id: snapshot.id })}</strong>
                <small>{[snapshot.collectedAt ?? t('common.notAvailable'), formatBytes(snapshot.bytes), snapshot.machineId ?? t('common.notAvailable'), snapshot.schemaVersion == null ? t('common.notAvailable') : t('system.diagnostics.snapshots.schema', { version: snapshot.schemaVersion })].join(' · ')}</small>
              </div>
              <div className="table-actions">
                {/* The export is the stored bytes, already redacted by the
                    fail-closed allowlist. There is deliberately no in-console
                    view of a snapshot that could bypass it. */}
                <a className="text-link" href={`/api/v1/diagnostics/snapshots/${snapshot.id}`} download aria-label={t('system.diagnostics.snapshots.downloadLabel', { id: snapshot.id })}>
                  <Download className="size-4" />{t('system.diagnostics.snapshots.download')}
                </a>
                <AlertDialog>
                  <AlertDialogTrigger render={<Button type="button" size="icon-sm" variant="destructive" disabled={remove.isPending} aria-label={t('system.diagnostics.snapshots.deleteLabel', { id: snapshot.id })} />}><Trash2 /></AlertDialogTrigger>
                  <AlertDialogContent>
                    <AlertDialogHeader>
                      <AlertDialogTitle>{t('system.diagnostics.snapshots.deleteLabel', { id: snapshot.id })}</AlertDialogTitle>
                      <AlertDialogDescription>{t('system.diagnostics.snapshots.deleteCopy', { id: snapshot.id })}</AlertDialogDescription>
                    </AlertDialogHeader>
                    <AlertDialogFooter>
                      <AlertDialogCancel>{t('common.actions.cancel')}</AlertDialogCancel>
                      <AlertDialogAction variant="destructive" onClick={() => remove.mutate(snapshot.id)}>{t('common.actions.delete')}</AlertDialogAction>
                    </AlertDialogFooter>
                  </AlertDialogContent>
                </AlertDialog>
              </div>
            </section>
          ))}
        </div>
        {remove.error ? <p className="callout error" role="alert">{errorMessage(remove.error, t('common.requestFailed'))}</p> : null}
      </Card>
    </div>
  )
}
