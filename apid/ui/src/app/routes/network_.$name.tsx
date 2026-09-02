import { createFileRoute } from '@tanstack/react-router'
import { InterfaceDetailPage } from '@/features/network/interface-detail'

export const Route = createFileRoute('/network_/$name')({ component: InterfaceDetailPage })
