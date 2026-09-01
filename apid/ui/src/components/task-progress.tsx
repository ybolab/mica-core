import { useQuery } from '@tanstack/react-query'
import { api } from '@/lib/api'
import type { TaskRecord } from '@/lib/types'

export function TaskProgress({ taskId }: { taskId?: string }) {
  const task = useQuery({
    queryKey: ['task', taskId],
    queryFn: () => api<TaskRecord>(`/api/v1/tasks/${encodeURIComponent(taskId!)}`),
    enabled: Boolean(taskId),
    refetchInterval: (query) => query.state.data?.status === 'finished' ? false : 1_000,
  })
  if (!taskId) return null
  if (task.error) {
    return <p className="callout warning" role="status">Change accepted; its final outcome is not confirmed yet.</p>
  }
  const value = task.data
  const complete = value?.status === 'finished'
  const failed = complete && value.outcome !== 'succeeded'
  return (
    <p className={`callout ${failed ? 'error' : complete ? 'success' : 'warning'}`} role="status">
      {complete
        ? failed ? value.message ?? value.outcome ?? 'Apply did not complete successfully.' : 'Change applied.'
        : value ? `Applying ${value.dotPath}…` : 'Change queued…'}
    </p>
  )
}
