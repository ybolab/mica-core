import { createFileRoute } from '@tanstack/react-router'
import { ApplicationsPage } from '@/features/applications/applications-page'

export const Route = createFileRoute('/applications')({ component: ApplicationsPage })
