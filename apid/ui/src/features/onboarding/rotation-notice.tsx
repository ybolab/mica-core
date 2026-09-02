import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { claimQuery } from '@/features/onboarding/claim-panel'

/// The bound announced where it bites. A device claimed by a provisioning
/// document refuses every authenticated mutation on every page, so the notice
/// that says why belongs in the shell rather than only on the page that
/// carries the remedy.
export function RotationNotice() {
  const { t } = useTranslation()
  const claim = useQuery(claimQuery)
  if (!claim.data?.rotationRequired) return null
  return (
    <div className="mx-auto w-full max-w-[1280px] px-6 pt-4">
      <p className="callout warning" role="status">
        {t('access.claim.notice')} <a className="text-link" href="/_ui/access">{t('access.claim.noticeLink')}</a>
      </p>
    </div>
  )
}
