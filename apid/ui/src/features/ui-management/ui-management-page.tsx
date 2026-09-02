import { useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { ArrowLeft, Check, ExternalLink, MonitorCog, PackageOpen, Trash2, Upload } from 'lucide-react'
import { api, errorMessage, json, uploadZip } from '@/lib/api'
import type { UiBundleDetails, UiBundleList, UiStatus } from '@/lib/types'
import { Button } from '@/components/ui/button'
import { Card, CardHeader } from '@/components/ui/card'
import { Status } from '@/components/ui/status'
import {
  AlertDialog,
  AlertDialogActions,
  AlertDialogClose,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogTitle,
  AlertDialogTrigger,
} from '@/components/ui/alert-dialog'

export function UiManagementPage() {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const input = useRef<HTMLInputElement>(null)
  const [file, setFile] = useState<File>()
  const [progress, setProgress] = useState(0)
  const [uploaded, setUploaded] = useState(false)
  const bundles = useQuery({ queryKey: ['ui-bundles'], queryFn: () => api<UiBundleList>('/api/v1/ui/bundles') })
  const refresh = (value?: UiBundleList) => {
    if (value) queryClient.setQueryData(['ui-bundles'], value)
    void queryClient.invalidateQueries({ queryKey: ['ui-status'] })
  }
  const upload = useMutation({
    mutationFn: (selected: File) => uploadZip<UiBundleList>('/api/v1/ui/bundles', selected, setProgress),
    onMutate: () => { setProgress(0); setUploaded(false) },
    onSuccess: (value) => {
      refresh(value)
      setUploaded(true)
      setFile(undefined)
      if (input.current) input.current.value = ''
    },
  })
  const activate = useMutation({
    mutationFn: (generation: number) => api<UiStatus>('/api/v1/ui/active', json('PUT', { generation })),
    onSuccess: () => { refresh(); void bundles.refetch() },
  })
  const deactivate = useMutation({
    mutationFn: () => api<UiStatus>('/api/v1/ui/active', { method: 'DELETE' }),
    onSuccess: () => { refresh(); void bundles.refetch() },
  })
  const remove = useMutation({
    mutationFn: (generation: number) => api<void>(`/api/v1/ui/bundles/${generation}`, { method: 'DELETE' }),
    onSuccess: () => { refresh(); void bundles.refetch() },
  })
  const pending = activate.isPending || deactivate.isPending || remove.isPending
  const mutationError = upload.error ?? activate.error ?? deactivate.error ?? remove.error

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <a className="text-link" href="/_ui/system"><ArrowLeft className="size-4" /> {t('system.uiManager.back')}</a>
          <p className="eyebrow">{t('system.uiManager.eyebrow')}</p>
          <h1>{t('system.uiManager.title')}</h1>
          <p>{t('system.uiManager.description')}</p>
        </div>
        <a className="text-link" href="/">{t('system.ui.openCustom')} <ExternalLink className="size-4" /></a>
      </header>

      <Card>
        <CardHeader title={t('system.uiManager.uploadTitle')} description={t('system.uiManager.uploadDescription')} action={<Upload className="size-5 text-muted-foreground" />} />
        <div className="upload-grid">
          <label className="upload-picker">
            <span>{t('system.uiManager.packageFile')}</span>
            <input ref={input} type="file" accept=".zip,.mos-ui.zip,application/zip" onChange={(event) => { setFile(event.target.files?.[0]); setUploaded(false) }} />
            <small>{file?.name ?? t('system.uiManager.noFile')}</small>
          </label>
          <Button disabled={!file || upload.isPending} onClick={() => file && upload.mutate(file)}><Upload className="size-4" /> {upload.isPending ? (progress >= 100 ? t('system.uiManager.validating') : t('system.uiManager.uploading')) : t('system.uiManager.upload')}</Button>
        </div>
        {upload.isPending ? <div className="upload-progress" role="progressbar" aria-label={t('system.uiManager.uploadProgress')} aria-valuemin={0} aria-valuemax={100} aria-valuenow={progress}><span style={{ width: `${progress}%` }} /><small>{progress}%</small></div> : null}
        {uploaded ? <p className="callout success" role="status">{t('system.uiManager.uploadedInactive')}</p> : null}
        <p className="callout warning" role="note">{t('system.uiManager.safetyNote')}</p>
      </Card>

      <Card>
        <CardHeader title={t('system.uiManager.versionsTitle')} description={t('system.uiManager.versionsDescription')} action={<PackageOpen className="size-5 text-muted-foreground" />} />
        {bundles.isPending ? <p className="empty">{t('system.ui.checking')}</p> : null}
        {!bundles.isPending && bundles.data?.bundles.length === 0 ? <p className="empty">{t('system.ui.noCustom')}</p> : null}
        {bundles.data?.bundles.length ? (
          <div className="ui-table-wrap">
            <table className="ui-table">
              <thead><tr><th>{t('system.uiManager.version')}</th><th>{t('system.uiManager.size')}</th><th>{t('system.uiManager.validation')}</th><th>{t('system.uiManager.state')}</th><th>{t('system.uiManager.actions')}</th></tr></thead>
              <tbody>{bundles.data.bundles.map((bundle) => (
                <BundleRow
                  key={bundle.generation}
                  bundle={bundle}
                  active={bundles.data?.activeGeneration === bundle.generation}
                  pending={pending}
                  onActivate={() => activate.mutate(bundle.generation)}
                  onDeactivate={() => deactivate.mutate()}
                  onDelete={() => remove.mutate(bundle.generation)}
                />
              ))}</tbody>
            </table>
          </div>
        ) : null}
        {bundles.data ? <p className="retention-note">{t('system.uiManager.retention', { count: bundles.data.bundles.length, limit: bundles.data.retentionLimit })}</p> : null}
      </Card>
      {bundles.error ? <p className="callout error" role="alert">{errorMessage(bundles.error, t('common.requestFailed'))}</p> : null}
      {mutationError ? <p className="callout error" role="alert">{errorMessage(mutationError, t('common.requestFailed'))}</p> : null}
    </div>
  )
}

function BundleRow({ bundle, active, pending, onActivate, onDeactivate, onDelete }: {
  bundle: UiBundleDetails
  active: boolean
  pending: boolean
  onActivate: () => void
  onDeactivate: () => void
  onDelete: () => void
}) {
  const { t } = useTranslation()
  return (
    <tr>
      <td><strong>{bundle.name ?? t('system.ui.generation', { generation: bundle.generation })}</strong><small>{bundle.version ?? `#${bundle.generation}`}</small></td>
      <td>{bundle.expandedBytes !== undefined
        ? <><strong>{formatBytes(bundle.expandedBytes)}</strong><small>{t('system.uiManager.archiveSize', { size: formatBytes(bundle.compressedBytes ?? 0) })}</small></>
        : <span className="text-muted-foreground">{t('system.uiManager.sizeUnavailable')}</span>}</td>
      <td>
        <Status ok={bundle.usable}>{bundle.usable ? t('system.uiManager.valid') : unavailableMessage(bundle.unavailableReason, t)}</Status>
        <small>{bundle.compatible === true ? t('system.uiManager.compatible') : bundle.compatible === false ? t('system.uiManager.incompatible') : t('system.uiManager.unchecked')}</small>
        {bundle.digest ? <code className="bundle-digest" title={bundle.digest}>{bundle.digest.slice(0, 12)}…</code> : null}
      </td>
      <td>{active ? <Status ok><Check className="size-3" /> {t('system.uiManager.active')}</Status> : t('system.uiManager.inactive')}</td>
      <td><div className="table-actions">
        {active
          ? <Button size="sm" variant="secondary" disabled={pending} onClick={onDeactivate}>{t('system.uiManager.useBuiltIn')}</Button>
          : <Button size="sm" disabled={pending || !bundle.usable} onClick={onActivate}><MonitorCog className="size-4" /> {t('system.uiManager.activate')}</Button>}
        <AlertDialog>
          <AlertDialogTrigger
            disabled={pending || active}
            render={<Button size="sm" variant="ghost" disabled={pending || active} aria-label={t('system.uiManager.deleteVersion', { generation: bundle.generation })} />}
          ><Trash2 className="size-4" /></AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogTitle>{t('system.uiManager.deleteVersion', { generation: bundle.generation })}</AlertDialogTitle>
            <AlertDialogDescription>{t('system.uiManager.confirmDelete', { generation: bundle.generation })}</AlertDialogDescription>
            <AlertDialogActions>
              <AlertDialogClose render={<Button variant="secondary" />}>{t('common.actions.cancel')}</AlertDialogClose>
              <AlertDialogClose render={<Button variant="danger" onClick={onDelete} />}>{t('common.actions.delete')}</AlertDialogClose>
            </AlertDialogActions>
          </AlertDialogContent>
        </AlertDialog>
      </div></td>
    </tr>
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
