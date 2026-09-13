import { useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ArrowLeft, Check, ExternalLink, MonitorCog, PackageOpen, Trash2, Upload } from 'lucide-react'
import { api, json, uploadZip } from '@/shared/lib/http'
import type { UiBundleDetails, UiBundleList, UiStatus } from '@/lib/types'
import { Callout } from '@/shared/components/callout'
import { ConfirmDialog } from '@/shared/components/confirm-dialog'
import { DataTable } from '@/shared/components/data-table'
import { FilePicker } from '@/shared/components/file-picker'
import { Page, PageHeader } from '@/shared/components/page'
import { CollectionPanel, Panel } from '@/shared/components/panel'
import { StatusDot } from '@/shared/components/status-badge'
import { Button, buttonVariants } from '@/shared/components/ui/button'
import { useMutationFeedback } from '@/shared/feedback/use-mutation-feedback'
import { failureDetail } from '@/shared/feedback/toast'

export function UiManagementPage() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const [progress, setProgress] = useState<number>()
  const bundles = useQuery({ queryKey: ['ui-bundles'], queryFn: () => api<UiBundleList>('/api/v1/ui/bundles') })
  const refresh = (value?: UiBundleList) => {
    if (value) queryClient.setQueryData(['ui-bundles'], value)
    void queryClient.invalidateQueries({ queryKey: ['ui-status'] })
    void bundles.refetch()
  }
  const upload = useMutationFeedback<UiBundleList, File>({
    mutationFn: (selected) => uploadZip<UiBundleList>('/api/v1/ui/bundles', selected, setProgress),
    success: t('system.uiManager.uploadedInactive'),
    failure: t('system.uiManager.upload'),
    onMutate: () => setProgress(0),
    onSuccess: (value) => { setProgress(undefined); refresh(value) },
    onError: () => setProgress(undefined),
  })
  const activate = useMutationFeedback<UiStatus, number>({
    mutationFn: (generation) => api<UiStatus>('/api/v1/ui/active', json('PUT', { generation })),
    success: (_data, generation) => t('system.uiManager.activated', { generation }),
    failure: t('system.uiManager.activate'),
    onSuccess: () => refresh(),
  })
  const deactivate = useMutationFeedback<UiStatus>({
    mutationFn: () => api<UiStatus>('/api/v1/ui/active', { method: 'DELETE' }),
    success: t('system.uiManager.deactivated'),
    failure: t('system.uiManager.useBuiltIn'),
    onSuccess: () => refresh(),
  })
  const remove = useMutationFeedback<void, number>({
    mutationFn: (generation) => api<void>(`/api/v1/ui/bundles/${generation}`, { method: 'DELETE' }),
    success: (_data, generation) => t('system.uiManager.deleted', { generation }),
    failure: t('common.actions.delete'),
    onSuccess: () => refresh(),
  })
  const pending = activate.isPending || deactivate.isPending || remove.isPending

  return (
    <Page>
      <PageHeader
        title={t('system.uiManager.title')}
        description={t('system.uiManager.description')}
        back={<a className={buttonVariants({ variant: 'outline', size: 'sm' })} href="/_ui/system"><ArrowLeft />{t('system.uiManager.back')}</a>}
        action={<a className={buttonVariants({ variant: 'outline', size: 'sm' })} href="/">{t('system.ui.openCustom')}<ExternalLink /></a>}
      />

      <Panel title={t('system.uiManager.uploadTitle')} description={t('system.uiManager.uploadDescription')} action={<Upload className="size-5 text-muted-foreground" />}>
        <FilePicker
          label={t('system.uiManager.packageFile')}
          hint={t('system.uiManager.packageHint')}
          accept=".zip,.mica-ui.zip,application/zip"
          chooseLabel={t('system.uiManager.choose')}
          emptyLabel={t('system.uiManager.noFile')}
          submitLabel={t('system.uiManager.upload')}
          pendingLabel={progress !== undefined && progress >= 100 ? t('system.uiManager.validating') : t('system.uiManager.uploading')}
          progress={upload.isPending ? progress : undefined}
          pending={upload.isPending}
          onSubmit={(file) => upload.mutate(file)}
        />
        <Callout tone="warning" title={t('system.uiManager.safetyNote')} />
      </Panel>

      <CollectionPanel
        title={t('system.uiManager.versionsTitle')}
        description={t('system.uiManager.versionsDescription')}
        action={<PackageOpen className="size-5 text-muted-foreground" />}
        footer={bundles.data ? <p className="text-right text-sm text-muted-foreground">{t('system.uiManager.retention', { count: bundles.data.bundles.length, limit: bundles.data.retentionLimit })}</p> : undefined}
      >
        <DataTable<UiBundleDetails>
          rows={bundles.data?.bundles}
          rowKey={(bundle) => String(bundle.generation)}
          isPending={bundles.isPending}
          empty={t('system.ui.noCustom')}
          columns={[
            { id: 'version', header: t('system.uiManager.version'), cell: (bundle) => (
              // Two lines, not two inline runs. The hand-rolled table rendered
              // these adjacent, so a version read as "kiosk1.2.0".
              <span className="flex flex-col">
                <strong className="font-medium">{bundle.name ?? t('system.ui.generation', { generation: bundle.generation })}</strong>
                <small className="text-muted-foreground">{bundle.version ?? `#${bundle.generation}`}</small>
              </span>
            ) },
            { id: 'size', header: t('system.uiManager.size'), cell: (bundle) => bundle.expandedBytes !== undefined
              ? (
                <span className="flex flex-col">
                  <strong className="font-medium">{formatBytes(bundle.expandedBytes)}</strong>
                  <small className="text-muted-foreground">{t('system.uiManager.archiveSize', { size: formatBytes(bundle.compressedBytes ?? 0) })}</small>
                </span>
              )
              : <span className="text-muted-foreground">{t('system.uiManager.sizeUnavailable')}</span> },
            { id: 'validation', header: t('system.uiManager.validation'), cell: (bundle) => (
              <span className="flex flex-col gap-0.5">
                <StatusDot state={bundle.usable ? 'ok' : 'warning'}>{bundle.usable ? t('system.uiManager.valid') : unavailableMessage(bundle.unavailableReason, t)}</StatusDot>
                <small className="text-muted-foreground">{bundle.compatible === true ? t('system.uiManager.compatible') : bundle.compatible === false ? t('system.uiManager.incompatible') : t('system.uiManager.unchecked')}</small>
                {bundle.digest ? <code className="w-fit font-mono text-xs" title={bundle.digest}>{bundle.digest.slice(0, 12)}…</code> : null}
              </span>
            ) },
            { id: 'state', header: t('system.uiManager.state'), cell: (bundle) => bundles.data?.activeGeneration === bundle.generation
              ? <StatusDot state="ok"><Check className="size-3" />{t('system.uiManager.active')}</StatusDot>
              : t('system.uiManager.inactive') },
            { id: 'actions', header: t('system.uiManager.actions'), align: 'end', cell: (bundle) => {
              const active = bundles.data?.activeGeneration === bundle.generation
              return (
                <div className="flex justify-end gap-2">
                  {active
                    ? <Button size="sm" variant="secondary" disabled={pending} onClick={() => deactivate.mutate()}>{t('system.uiManager.useBuiltIn')}</Button>
                    : <Button size="sm" disabled={pending || !bundle.usable} onClick={() => activate.mutate(bundle.generation)}><MonitorCog />{t('system.uiManager.activate')}</Button>}
                  <ConfirmDialog
                    disabled={pending || active}
                    trigger={<Button size="icon-sm" variant="destructive" disabled={pending || active} aria-label={t('system.uiManager.deleteVersion', { generation: bundle.generation })}><Trash2 /></Button>}
                    title={t('system.uiManager.deleteVersion', { generation: bundle.generation })}
                    description={t('system.uiManager.confirmDelete', { generation: bundle.generation })}
                    confirmLabel={t('common.actions.delete')}
                    success={t('system.uiManager.deleted', { generation: bundle.generation })}
                    failure={t('common.actions.delete')}
                    onConfirm={() => remove.mutateAsync(bundle.generation)}
                  />
                </div>
              )
            } },
          ]}
        />
      </CollectionPanel>
      {bundles.error ? <Callout tone="danger" title={failureDetail(bundles.error, t('common.requestFailed'))} /> : null}
    </Page>
  )
}

function formatBytes(bytes: number) {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KiB`
  return `${bytes} B`
}

function unavailableMessage(
  reason: UiBundleDetails['unavailableReason'],
  t: ReturnType<typeof useTranslation>['t'],
) {
  switch (reason) {
    case 'missingActivationRecord': return t('system.ui.unavailable.missingActivationRecord')
    case 'unsafeTree': return t('system.ui.unavailable.unsafeTree')
    case 'indexUnavailable': return t('system.ui.unavailable.indexUnavailable')
    case 'manifestInvalid': return t('system.ui.unavailable.manifestInvalid')
    case 'digestMismatch': return t('system.ui.unavailable.digestMismatch')
    case 'incompatible': return t('system.ui.unavailable.incompatible')
    default: return t('system.ui.unavailable.unknown')
  }
}
