import { useMemo, useState } from 'react'
import { Globe, Laptop, Moon, Sun } from 'lucide-react'
import { useTranslation } from 'react-i18next'
import { currentLocaleChoice, setLocaleChoice } from '@/i18n/i18n'
import { AUTO_LOCALE, autoLanguageNative, languageChoices } from '@/i18n/locale'
import { useTheme, type ThemeMode } from '@/theme/theme'
import { SegmentedControl } from '@/shared/components/segmented-control'
import { StatusBadge } from '@/shared/components/status-badge'
import {
  Combobox,
  ComboboxContent,
  ComboboxEmpty,
  ComboboxInput,
  ComboboxItem,
  ComboboxList,
} from '@/shared/components/ui/combobox'

function browserLocales(): readonly string[] {
  if (typeof navigator === 'undefined') return []
  return navigator.languages?.length ? navigator.languages : [navigator.language]
}

interface Choice {
  id: string
  native: string
  sub: string
  planned: boolean
}

/// The language picker.
///
/// It used to be a `Dialog` rendered inside the header's `DropdownMenuContent`,
/// which left the menu open and painted over the dialog backdrop with two focus
/// scopes competing. A combobox is the registry's control for a filtered list
/// and anchors to its own trigger, so the overlap cannot happen: the picker is
/// its own header control rather than a second layer inside a menu.
export function LanguageControl({ className }: { className?: string }) {
  const { t } = useTranslation()
  const choice = currentLocaleChoice()
  const [query, setQuery] = useState('')
  const auto = `${t('preferences.auto')} · ${autoLanguageNative(browserLocales())}`

  const choices = useMemo<Choice[]>(() => languageChoices().map((entry) => entry.id === AUTO_LOCALE
    ? { id: entry.id, native: t('preferences.auto'), sub: auto, planned: false }
    : { id: entry.id, native: entry.native, sub: entry.english, planned: entry.planned }),
  [t, auto])

  const selected = choices.find((entry) => entry.id === choice)
  const needle = query.trim().toLowerCase()
  const items = needle
    ? choices.filter((entry) => entry.id.toLowerCase().includes(needle)
      || entry.native.toLowerCase().includes(needle)
      || entry.sub.toLowerCase().includes(needle))
    : choices

  return (
    <Combobox
      items={items}
      value={selected ?? null}
      onValueChange={(next: Choice | null) => {
        if (next) void setLocaleChoice(next.id)
        setQuery('')
      }}
      inputValue={query}
      onInputValueChange={setQuery}
      itemToStringLabel={(entry: Choice) => entry.native}
    >
      <ComboboxInput
        className={className}
        aria-label={t('preferences.language')}
        placeholder={selected?.native ?? t('preferences.language')}
      />
      {/* The popup is anchored to a 160px trigger, so it is widened here
          rather than left to wrap a language name onto three lines. */}
      <ComboboxContent className="w-72 min-w-72">
        <ComboboxEmpty>{t('preferences.noMatch')}</ComboboxEmpty>
        <ComboboxList>
          {items.map((entry) => (
            <ComboboxItem key={entry.id} value={entry}>
              <span className="flex min-w-0 flex-col">
                <span className="truncate">{entry.native}</span>
                <span className="truncate text-xs text-muted-foreground">{entry.sub}</span>
              </span>
              {entry.planned ? <StatusBadge>{t('preferences.planned')}</StatusBadge> : null}
            </ComboboxItem>
          ))}
        </ComboboxList>
      </ComboboxContent>
    </Combobox>
  )
}

const themeSegments = [
  { value: 'light' as const, icon: <Sun aria-hidden="true" /> },
  { value: 'dark' as const, icon: <Moon aria-hidden="true" /> },
  { value: 'system' as const, icon: <Laptop aria-hidden="true" /> },
]

export function ThemeControl({ labelled = true }: { labelled?: boolean }) {
  const { t } = useTranslation()
  const { mode, setMode } = useTheme()
  return (
    <SegmentedControl<ThemeMode>
      label={t('preferences.appearance')}
      value={mode}
      onValueChange={setMode}
      segments={themeSegments.map((segment) => ({
        value: segment.value,
        label: t(`preferences.themes.${segment.value}`),
        icon: segment.icon,
        labelHidden: !labelled,
      }))}
    />
  )
}

/// The pair shown on the sign-in screen, before there is a shell to hold them.
export function Preferences() {
  const { t } = useTranslation()
  return (
    <section className="flex items-center gap-2" aria-label={t('preferences.regionLabel')}>
      <Globe className="size-4 shrink-0 text-muted-foreground" aria-hidden="true" />
      <LanguageControl className="w-44" />
      <ThemeControl labelled={false} />
    </section>
  )
}
