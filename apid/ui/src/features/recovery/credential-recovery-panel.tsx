import { useTranslation } from 'react-i18next'
import { KeyRound } from 'lucide-react'
import { Card, CardHeader } from '@/components/ui/card'

/// Credential recovery, rendered and deliberately NOT offered.
///
/// `POST /api/v1/recovery/credential` is implemented and tested, and its only
/// authority is a physical-presence assertion. Nothing in this build writes
/// one, so on a fielded device the route refuses every request that can reach
/// it. A control here would promise an operation an operator cannot complete,
/// which is worse than no control; what the operator needs instead is to know
/// that the flow exists, what it would cost, and what to do while it is out of
/// reach.
export function CredentialRecoveryPanel() {
  const { t } = useTranslation()
  return (
    <Card>
      <CardHeader title={t('system.recovery.credential.title')} description={t('system.recovery.credential.description')} action={<KeyRound className="size-5 text-muted-foreground" />} />
      <dl className="details">
        <div><dt>{t('system.recovery.credential.authority')}</dt><dd>{t('system.recovery.credential.authorityValue')}</dd></div>
        <div><dt>{t('system.recovery.credential.channel')}</dt><dd>{t('system.recovery.credential.channelValue')}</dd></div>
        <div><dt>{t('system.recovery.credential.cost')}</dt><dd>{t('system.recovery.credential.costValue')}</dd></div>
      </dl>
      <p className="callout warning" role="status">{t('system.recovery.credential.unavailable')}</p>
      <p className="callout" role="status">{t('system.recovery.credential.stillHolding')}</p>
      <p className="callout" role="status">{t('system.recovery.credential.noSoftwarePath')}</p>
    </Card>
  )
}
