import { createFileRoute } from '@tanstack/react-router'
import { ServiceDetailPage } from '@/features/services/service-detail'

export const Route = createFileRoute('/services_/$service')({ component: ServiceDetailPage })
