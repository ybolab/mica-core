import { Languages, MonitorCog } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { currentLocale, setLocale } from '@/i18n/i18n'
import type { Locale } from '@/i18n/locale'
import { useTheme, type ThemeMode } from '@/theme/theme'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/shared/components/ui/select'
import { cn } from '@/shared/lib/utils'

const localeOptions: Locale[] = ['en', 'zh-CN']
const themeOptions: ThemeMode[] = ['system', 'light', 'dark']

export function Preferences({ compact = false }: { compact?: boolean }) {
  const { t } = useTranslation()
  const { mode, setMode } = useTheme()
  const locale = currentLocale()

  return (
    <section
      className={cn('preferences', compact && 'preferences-compact')}
      aria-label={t('preferences.regionLabel')}
    >
      <div className="preference-field">
        <span><Languages aria-hidden="true" />{t('preferences.language')}</span>
        <Select
          value={locale}
          onValueChange={(value) => {
            if (localeOptions.includes(value as Locale)) void setLocale(value as Locale)
          }}
        >
          <SelectTrigger aria-label={t('preferences.language')}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent align="start">
            <SelectItem value="en">{t('preferences.languages.en')}</SelectItem>
            <SelectItem value="zh-CN">{t('preferences.languages.zhCN')}</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <div className="preference-field">
        <span><MonitorCog aria-hidden="true" />{t('preferences.appearance')}</span>
        <Select
          value={mode}
          onValueChange={(value) => {
            if (themeOptions.includes(value as ThemeMode)) setMode(value as ThemeMode)
          }}
        >
          <SelectTrigger aria-label={t('preferences.appearance')}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent align="start">
            <SelectItem value="system">{t('preferences.themes.system')}</SelectItem>
            <SelectItem value="light">{t('preferences.themes.light')}</SelectItem>
            <SelectItem value="dark">{t('preferences.themes.dark')}</SelectItem>
          </SelectContent>
        </Select>
      </div>
    </section>
  )
}
