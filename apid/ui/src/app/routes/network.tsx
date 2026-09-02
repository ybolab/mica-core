import { createFileRoute } from '@tanstack/react-router'
import { NetworkPage } from '@/features/network/network-page'

export const Route = createFileRoute('/network')({ component: NetworkPage })
