import { createRouter } from '@tanstack/react-router'
import { routeTree } from '@/app/routeTree.gen'

export function createAppRouter() {
  return createRouter({ routeTree, basepath: '/_ui' })
}

export type AppRouter = ReturnType<typeof createAppRouter>

declare module '@tanstack/react-router' {
  interface Register { router: AppRouter }
}
