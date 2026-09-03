import { createFileRoute } from '@tanstack/react-router'
import { SystemPage } from '@/features/system/system-page'

export { UiPanel, UpdateChecks, UpdatePanel } from '@/features/system/system-page'

export const Route = createFileRoute('/system')({ component: SystemPage })
