import { createFileRoute } from '@tanstack/react-router'
import { SystemPage } from '@/features/system/system-page'

// Nothing else may be exported from a route module: the router plugin only
// code-splits a route whose exports are the route, so a re-export here hoists
// the whole page — and everything it imports — into the entry chunk.
export const Route = createFileRoute('/system')({ component: SystemPage })
