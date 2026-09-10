import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { api } from '@/shared/lib/http'
import type { TaskRecord } from '@/lib/types'
import { Callout } from './callout'

/// A settings write is accepted before it is applied, so the console follows the
/// task the daemon handed back until it settles. This is device state, not the
/// outcome of a click, so it stays inline rather than becoming a toast: the
/// operator may leave and come back while a change is still applying.
export function TaskProgress({ taskId }: { taskId?: string }) {
  const { t } = useTranslation()
  const task = useQuery({
    queryKey: ['task', taskId],
    queryFn: () => api<TaskRecord>(`/api/v1/tasks/${encodeURIComponent(taskId!)}`),
    enabled: Boolean(taskId),
    refetchInterval: (query) => query.state.data?.status === 'finished' ? false : 1_000,
  })
  if (!taskId) return null
  if (task.error) return <Callout tone="warning" title={t('task.unconfirmed')} />
  const value = task.data
  const complete = value?.status === 'finished'
  const failed = complete && value.outcome !== 'succeeded'
  const message = complete
    ? failed ? value.message ?? value.outcome ?? t('task.failed') : t('task.applied')
    : value ? t('task.applying', { path: value.dotPath }) : t('task.queued')
  return <Callout tone={failed ? 'danger' : complete ? 'success' : 'neutral'} title={message} />
}
