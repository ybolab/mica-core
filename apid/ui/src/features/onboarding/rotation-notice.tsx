import { useQuery } from '@tanstack/react-query'
import { useTranslation } from 'react-i18next'
import { claimQuery } from '@/features/onboarding/claim-panel'
import { Callout } from '@/shared/components/callout'

/// The bound announced where it bites. A device claimed by a provisioning
/// document refuses every authenticated mutation on every page, so the notice
/// that says why belongs in the shell rather than only on the page that
/// carries the remedy.
export function RotationNotice() {
  const { t } = useTranslation()
  const claim = useQuery(claimQuery)
  if (!claim.data?.rotationRequired) return null
  return (
    <div className="mx-auto w-full max-w-[1280px] px-4 pt-4 sm:px-6 lg:px-8">
      <Callout tone="warning" title={t('access.claim.notice')}>
        {/* A plain anchor rather than a router link: this notice is rendered
            above the outlet, and the destination is a full route the operator
            reaches once. */}
        <a href="/_ui/access" className="underline underline-offset-2">{t('access.claim.noticeLink')}</a>
      </Callout>
    </div>
  )
}
