import { createInstance } from 'i18next'
import { detectLocale, LOCALE_STORAGE_KEY, normalizeLocale, type Locale } from './locale'
import { resources } from './resources'

function safeStoredLocale() {
  try {
    return window.localStorage.getItem(LOCALE_STORAGE_KEY)
  } catch {
    return null
  }
}

function browserLocales(): readonly string[] {
  if (typeof navigator === 'undefined') return []
  return navigator.languages?.length ? navigator.languages : [navigator.language]
}

const initialLocale = detectLocale(safeStoredLocale(), browserLocales())

export const i18n = createInstance()

void i18n.init({
  resources,
  lng: initialLocale,
  fallbackLng: 'en',
  supportedLngs: ['en', 'zh-CN'],
  defaultNS: 'translation',
  interpolation: { escapeValue: false },
  returnNull: false,
  initAsync: false,
})

function syncDocument(locale: Locale) {
  document.documentElement.lang = locale
  document.title = i18n.t('shell.documentTitle', { lng: locale })
}

syncDocument(initialLocale)
i18n.on('languageChanged', (language) => {
  syncDocument(normalizeLocale(language) ?? 'en')
})

export function currentLocale(): Locale {
  return normalizeLocale(i18n.resolvedLanguage ?? i18n.language) ?? 'en'
}

export async function setLocale(locale: Locale) {
  try {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, locale)
  } catch {
    // Browser privacy settings may disable local storage; the session still updates.
  }
  await i18n.changeLanguage(locale)
}
