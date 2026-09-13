import type { ReactNode } from 'react'
import { useTranslation } from 'react-i18next'
import { StatusBadge } from '@/shared/components/status-badge'

/// Marks one section as designed but not yet real. The page-level simulation
/// notice is not enough when only part of a page is unfinished: an operator
/// reading a form needs to know, at that form, that saving it changes nothing
/// on the device.
export function PlannedNotice({ children }: { children?: ReactNode }) {
  const { t } = useTranslation()
  return (
    <div className="planned-notice" role="note">
      <StatusBadge tone="warning">{t('common.planned')}</StatusBadge>
      <span>{children ?? t('common.plannedCopy')}</span>
    </div>
  )
}
