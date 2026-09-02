import { useMemo, useState } from 'react'
import { ChevronRight, Globe, Laptop, Moon, Sun } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { currentLocaleChoice, setLocaleChoice } from '@/i18n/i18n'
import { AUTO_LOCALE, autoLanguageNative, languageChoices } from '@/i18n/locale'
import { useTheme, type ThemeMode } from '@/theme/theme'
import { StatusBadge } from '@/shared/components/status-badge'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/shared/components/ui/dialog'
import { Input } from '@/shared/components/ui/input'

const themeOptions: { mode: ThemeMode; icon: typeof Sun }[] = [
  { mode: 'light', icon: Sun },
  { mode: 'dark', icon: Moon },
  { mode: 'system', icon: Laptop },
]

function browserLocales(): readonly string[] {
  if (typeof navigator === 'undefined') return []
  return navigator.languages?.length ? navigator.languages : [navigator.language]
}

/// The language row: the current choice, and the picker it opens. `auto` names
/// the language it currently resolves to so the row is never just "Auto".
export function LanguageControl({ inMenu = false }: { inMenu?: boolean }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const choice = currentLocaleChoice()
  const catalog = useMemo(() => languageChoices(), [])
  const label = choice === AUTO_LOCALE
    ? `${t('preferences.auto')} · ${autoLanguageNative(browserLocales())}`
    : catalog.find((entry) => entry.id === choice)?.native ?? t('preferences.auto')

  return (
    <>
      <button type="button" className={inMenu ? 'menu-item' : 'language-button'} onClick={() => setOpen(true)} aria-label={t('preferences.language')} title={t('preferences.language')}>
        <span><Globe aria-hidden="true" /><span>{label}</span></span>
        {inMenu ? <ChevronRight aria-hidden="true" /> : null}
      </button>
      <LanguagePicker open={open} onClose={() => setOpen(false)} choice={choice} />
    </>
  )
}

function LanguagePicker({ open, onClose, choice }: { open: boolean; onClose: () => void; choice: string }) {
  const { t } = useTranslation()
  const [query, setQuery] = useState('')
  const catalog = useMemo(() => languageChoices(), [])
  const auto = `${t('preferences.auto')} · ${autoLanguageNative(browserLocales())}`
  const needle = query.trim().toLowerCase()
  const rows = catalog
    .map((entry) => entry.id === AUTO_LOCALE
      ? { ...entry, native: t('preferences.auto'), english: t('preferences.autoCopy'), sub: auto }
      : { ...entry, sub: entry.english })
    .filter((entry) => !needle || entry.id.toLowerCase().includes(needle) || entry.native.toLowerCase().includes(needle) || entry.english.toLowerCase().includes(needle))

  const select = (id: string) => {
    void setLocaleChoice(id)
    onClose()
  }

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next) onClose() }}>
      <DialogContent className="language-dialog" showCloseButton>
        <DialogHeader>
          <DialogTitle>{t('preferences.language')}</DialogTitle>
        </DialogHeader>
        <Input type="search" value={query} onChange={(event) => setQuery(event.target.value)} aria-label={t('preferences.search')} placeholder={t('preferences.search')} />
        <div className="language-list">
          {rows.map((entry) => (
            <button type="button" key={entry.id} onClick={() => select(entry.id)} aria-pressed={entry.id === choice} data-active={entry.id === choice || undefined}>
              <span><span>{entry.native}</span><small>{entry.sub}</small></span>
              {entry.planned ? <StatusBadge>{t('preferences.planned')}</StatusBadge> : null}
            </button>
          ))}
          {rows.length === 0 ? <p className="empty">{t('preferences.noMatch')}</p> : null}
        </div>
      </DialogContent>
    </Dialog>
  )
}

/// The three-way appearance control the prototype puts in the settings menu and
/// on the sign-in screen; labels are dropped where only the icons fit.
export function ThemeControl({ labelled = true }: { labelled?: boolean }) {
  const { t } = useTranslation()
  const { mode, setMode } = useTheme()
  return (
    <div className="theme-control" role="radiogroup" aria-label={t('preferences.appearance')}>
      {themeOptions.map(({ mode: option, icon: Icon }) => {
        const label = t(`preferences.themes.${option}`)
        return (
          <button type="button" key={option} role="radio" aria-checked={mode === option} data-active={mode === option || undefined} aria-label={label} title={label} onClick={() => setMode(option)}>
            <Icon aria-hidden="true" />{labelled ? <span>{label}</span> : null}
          </button>
        )
      })}
    </div>
  )
}

export function Preferences() {
  const { t } = useTranslation()
  return (
    <section className="preferences" aria-label={t('preferences.regionLabel')}>
      <LanguageControl />
      <ThemeControl labelled={false} />
    </section>
  )
}
