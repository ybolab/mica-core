export const supportedLocales = ['en', 'zh-CN'] as const

export type Locale = (typeof supportedLocales)[number]

export const LOCALE_STORAGE_KEY = 'mica.ui.locale'

export function normalizeLocale(value: string | null | undefined): Locale | undefined {
  const normalized = value?.trim().toLowerCase()
  if (!normalized) return undefined
  if (normalized === 'zh' || normalized.startsWith('zh-')) return 'zh-CN'
  if (normalized === 'en' || normalized.startsWith('en-')) return 'en'
  return undefined
}

export function detectLocale(
  stored: string | null | undefined,
  navigatorLocales: readonly string[] = [],
): Locale {
  const storedLocale = normalizeLocale(stored)
  if (storedLocale) return storedLocale
  for (const candidate of navigatorLocales) {
    const locale = normalizeLocale(candidate)
    if (locale) return locale
  }
  return 'en'
}

/// Every language the prototype's picker lists, in its order. The ones without
/// a bundle here are still offered and still labelled, because the picker is
/// also how the console admits what it cannot yet do.
const catalog: readonly { id: string; native: string; english: string }[] = [
  { id: 'en', native: 'English', english: 'English' },
  { id: 'zh-CN', native: '简体中文', english: 'Chinese (Simplified)' },
  { id: 'zh-TW', native: '繁體中文', english: 'Chinese (Traditional)' },
  { id: 'ja', native: '日本語', english: 'Japanese' },
  { id: 'ko', native: '한국어', english: 'Korean' },
  { id: 'de', native: 'Deutsch', english: 'German' },
  { id: 'fr', native: 'Français', english: 'French' },
  { id: 'es', native: 'Español', english: 'Spanish' },
  { id: 'pt', native: 'Português', english: 'Portuguese' },
  { id: 'it', native: 'Italiano', english: 'Italian' },
  { id: 'nl', native: 'Nederlands', english: 'Dutch' },
  { id: 'pl', native: 'Polski', english: 'Polish' },
  { id: 'ru', native: 'Русский', english: 'Russian' },
  { id: 'tr', native: 'Türkçe', english: 'Turkish' },
  { id: 'ar', native: 'العربية', english: 'Arabic' },
  { id: 'vi', native: 'Tiếng Việt', english: 'Vietnamese' },
]

export const AUTO_LOCALE = 'auto'

export interface LanguageChoice {
  id: string
  native: string
  english: string
  planned: boolean
}

function isSupported(id: string): id is Locale {
  return (supportedLocales as readonly string[]).includes(id)
}

/// `auto` first, then the catalog. A choice is planned when the console has no
/// bundle for it, which the picker shows next to the name.
export function languageChoices(): LanguageChoice[] {
  return [
    { id: AUTO_LOCALE, native: AUTO_LOCALE, english: AUTO_LOCALE, planned: false },
    ...catalog.map((entry) => ({ ...entry, planned: !isSupported(entry.id) })),
  ]
}

/// Resolves a stored picker choice to a locale the console can actually render.
/// A planned language falls back to English rather than to the browser, so the
/// choice still reads as a decision the operator made.
export function resolveLocaleChoice(
  choice: string | null | undefined,
  navigatorLocales: readonly string[] = [],
): Locale {
  if (choice && isSupported(choice)) return choice
  if (choice && choice !== AUTO_LOCALE && catalog.some((entry) => entry.id === choice)) return 'en'
  return detectLocale(choice === AUTO_LOCALE ? null : choice, navigatorLocales)
}

/// The native name of whatever `auto` currently resolves to, used as the
/// picker's subtitle. It answers for languages the console cannot render, so
/// the label stays truthful about what the browser asked for.
export function autoLanguageNative(navigatorLocales: readonly string[]): string {
  const requested = navigatorLocales[0]?.trim().toLowerCase()
  if (!requested) return 'English'
  const base = requested.split('-')[0]
  const match = catalog.find((entry) => {
    const id = entry.id.toLowerCase()
    return id === requested || id.split('-')[0] === base
  })
  return match?.native ?? 'English'
}
