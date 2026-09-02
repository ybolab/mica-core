import { createFileRoute } from '@tanstack/react-router'
import { UiManagementPage } from '@/features/ui-management/ui-management-page'

export const Route = createFileRoute('/system_/ui')({ component: UiManagementPage })
