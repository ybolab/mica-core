import { MutationCache, QueryCache, QueryClient } from '@tanstack/react-query'
import { ApiError, rememberSession } from '@/shared/lib/http'

function onError(error: unknown) {
  if (!(error instanceof ApiError) || error.status !== 401) return
  rememberSession({ state: 'unauthenticated' })
  const protectedQueries = { predicate: (query: { queryKey: readonly unknown[] }) => query.queryKey[0] !== 'session' }
  void queryClient.cancelQueries()
  queryClient.setQueryData(['session'], { state: 'unauthenticated' })
  queryClient.removeQueries(protectedQueries)
}

export const queryClient = new QueryClient({
  queryCache: new QueryCache({ onError }),
  mutationCache: new MutationCache({ onError }),
  defaultOptions: { queries: {
    staleTime: 10_000,
    refetchOnWindowFocus: false,
    retry: (failures, error) => !(error instanceof ApiError && error.status === 401) && failures < 3,
  } },
})
