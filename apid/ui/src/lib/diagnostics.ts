import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import type { ObservedNetworkState, SnapshotCollected, SnapshotList, SystemInformation } from '@/lib/types'
import { api } from '@/lib/api'

export const systemInformationKey = ['system-information'] as const
export const observedNetworkKey = ['observed-network'] as const
export const diagnosticSnapshotsKey = ['diagnostic-snapshots'] as const

export function useSystemInformation() {
  return useQuery({
    queryKey: systemInformationKey,
    queryFn: () => api<SystemInformation>('/api/v1/system/info'),
    retry: false,
  })
}

export function useObservedNetwork() {
  return useQuery({
    queryKey: observedNetworkKey,
    queryFn: () => api<ObservedNetworkState>('/api/v1/network/status'),
    refetchInterval: 15_000,
    retry: false,
  })
}

export function useDiagnosticSnapshots() {
  return useQuery({
    queryKey: diagnosticSnapshotsKey,
    queryFn: () => api<SnapshotList>('/api/v1/diagnostics/snapshots'),
    retry: false,
  })
}

export function useCollectDiagnosticSnapshot() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => api<SnapshotCollected>('/api/v1/diagnostics/snapshots', { method: 'POST' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: diagnosticSnapshotsKey }),
  })
}

export function useDeleteDiagnosticSnapshot() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => api<void>(`/api/v1/diagnostics/snapshots/${id}`, { method: 'DELETE' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: diagnosticSnapshotsKey }),
  })
}
