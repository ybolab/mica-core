import { FlaskConical } from 'lucide-react'
import { useTranslation } from 'react-i18next'

export function SimulationNotice({ scope }: { scope: string }) {
  const { t } = useTranslation()
  return (
    <aside className="simulation-notice" aria-label={t('simulation.title')}>
      <FlaskConical aria-hidden="true" />
      <div>
        <strong>{t('simulation.title')}</strong>
        <p>{t('simulation.description', { scope })}</p>
      </div>
    </aside>
  )
}
