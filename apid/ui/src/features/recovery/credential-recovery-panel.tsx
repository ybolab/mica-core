import { useTranslation } from 'react-i18next'
import { KeyRound } from 'lucide-react'
import { Callout } from '@/shared/components/callout'
import { FactList } from '@/shared/components/fact-list'
import { Panel } from '@/shared/components/panel'

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
    <Panel
      title={t('system.recovery.credential.title')}
      description={t('system.recovery.credential.description')}
      action={<KeyRound className="size-5 text-muted-foreground" />}
    >
      <FactList facts={[
        { id: 'authority', label: t('system.recovery.credential.authority'), value: t('system.recovery.credential.authorityValue') },
        { id: 'channel', label: t('system.recovery.credential.channel'), value: t('system.recovery.credential.channelValue') },
        { id: 'cost', label: t('system.recovery.credential.cost'), value: t('system.recovery.credential.costValue') },
      ]} />
      <Callout tone="warning" title={t('system.recovery.credential.unavailable')} />
      <Callout title={t('system.recovery.credential.stillHolding')} />
      <Callout title={t('system.recovery.credential.noSoftwarePath')} />
    </Panel>
  )
}
