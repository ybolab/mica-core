import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { Download, FileArchive, ShieldCheck, Trash2 } from 'lucide-react'
import { ApiError, api } from '@/shared/lib/http'
import type { SnapshotCollected, SnapshotList } from '@/lib/types'
import { Button, buttonVariants } from '@/shared/components/ui/button'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { FactList } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'
import { RowItem, RowList } from '@/shared/components/row-item'
import { Spinner } from '@/shared/components/ui/spinner'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'
import { formatBytes } from '@/shared/components/fact'

const snapshotsKey = ['diagnostic-snapshots'] as const

export function DiagnosticsPanel() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const snapshots = useQuery({ queryKey: snapshotsKey, queryFn: () => api<SnapshotList>('/api/v1/diagnostics/snapshots'), retry: false })
  const collect = useMutationFeedback<SnapshotCollected>({
    mutationFn: () => api<SnapshotCollected>('/api/v1/diagnostics/snapshots', { method: 'POST' }),
    success: (data) => t('system.diagnostics.collection.complete', {
      id: data.snapshot.id,
      elapsed: data.elapsedMillis,
      dropped: data.droppedFields,
      redacted: data.redactedFields,
    }),
    failure: t('system.diagnostics.collection.generate'),
    onSuccess: () => void queryClient.invalidateQueries({ queryKey: snapshotsKey }),
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
    <div className="grid gap-3">
      <div className="grid gap-3 lg:grid-cols-2">
        <Panel title={t('system.diagnostics.collection.title')} description={t('system.diagnostics.collection.description')} action={<FileArchive className="size-5 text-muted-foreground" />}>
          <Button className="justify-self-start" onClick={() => collect.mutate()} disabled={collect.isPending}>
            {collect.isPending ? <Spinner /> : null}
            {collect.isPending ? t('system.diagnostics.collection.pending') : t('system.diagnostics.collection.generate')}
          </Button>
          {/* Collection is one at a time, and a 409 stays on the page rather
              than becoming a toast: it is a state of the device that persists
              until the running collection finishes. */}
          {busy ? <Callout tone="warning" title={t('system.diagnostics.collection.busy')} /> : null}
        </Panel>
        <Panel title={t('system.diagnostics.retention.title')} description={t('system.diagnostics.retention.description')} action={<ShieldCheck className="size-5 text-muted-foreground" />}>
          <FactList facts={retention ? [
            { id: 'count', label: t('system.diagnostics.retention.count'), value: t('system.diagnostics.retention.snapshots', { count: retention.maxSnapshots }) },
            { id: 'total', label: t('system.diagnostics.retention.total'), value: t('system.diagnostics.retention.totalBytes', { size: formatBytes(retention.maxTotalBytes) }) },
            { id: 'each', label: t('system.diagnostics.retention.each'), value: t('system.diagnostics.retention.snapshotBytes', { size: formatBytes(retention.maxSnapshotBytes) }) },
            { id: 'schemas', label: t('system.diagnostics.retention.schemas'), value: t('system.diagnostics.retention.schemaVersions', { snapshot: retention.schemaVersion, redaction: retention.redactionSchemaVersion }) },
          ] : []} />
        </Panel>
      </div>
      <Panel title={t('system.diagnostics.snapshots.title')} description={t('system.diagnostics.snapshots.description')} action={<Download className="size-5 text-muted-foreground" />} contentClassName="gap-0">
        {snapshots.isPending ? <p className="flex items-center gap-2 text-sm text-muted-foreground"><Spinner />{t('system.diagnostics.snapshots.loading')}</p> : null}
        {snapshots.error ? <Callout tone="danger" title={failureDetail(snapshots.error, t('common.requestFailed'))} /> : null}
        {snapshots.data?.snapshots.length === 0 ? <p className="py-6 text-center text-sm text-muted-foreground">{t('system.diagnostics.snapshots.empty')}</p> : null}
        <RowList>
          {snapshots.data?.snapshots.map((snapshot) => (
            <RowItem
              key={snapshot.id}
              title={t('system.diagnostics.snapshots.snapshot', { id: snapshot.id })}
              description={[snapshot.collectedAt ?? t('common.notAvailable'), formatBytes(snapshot.bytes), snapshot.machineId ?? t('common.notAvailable'), snapshot.schemaVersion == null ? t('common.notAvailable') : t('system.diagnostics.snapshots.schema', { version: snapshot.schemaVersion })].join(' · ')}
              actions={(
                <>
                  {/* The export is the stored bytes, already redacted by the
                      fail-closed allowlist. There is deliberately no in-console
                      view of a snapshot that could bypass it. */}
                  {/* A navigation, not a command, so it stays an anchor and
                      keeps the link role; the look comes from the primitive's
                      own variants rather than from a parallel rule. */}
                  <a
                    className={buttonVariants({ variant: 'outline', size: 'sm' })}
                    href={`/api/v1/diagnostics/snapshots/${snapshot.id}`}
                    download
                    aria-label={t('system.diagnostics.snapshots.downloadLabel', { id: snapshot.id })}
                  >
                    <Download />{t('system.diagnostics.snapshots.download')}
                  </a>
                  <ConfirmDialog
                    trigger={<Button type="button" size="icon-sm" variant="destructive" aria-label={t('system.diagnostics.snapshots.deleteLabel', { id: snapshot.id })}><Trash2 /></Button>}
                    title={t('system.diagnostics.snapshots.deleteLabel', { id: snapshot.id })}
                    description={t('system.diagnostics.snapshots.deleteCopy', { id: snapshot.id })}
                    confirmLabel={t('common.actions.delete')}
                    success={t('system.diagnostics.snapshots.deleted', { id: snapshot.id })}
                    failure={t('system.diagnostics.snapshots.deleteLabel', { id: snapshot.id })}
                    onConfirm={() => remove.mutateAsync(snapshot.id)}
                  />
                </>
              )}
            />
          ))}
        </RowList>
      </Panel>
    </div>
  )
}
