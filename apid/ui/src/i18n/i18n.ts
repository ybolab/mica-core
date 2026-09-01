import { createInstance } from 'i18next'
import { loadLocale } from './load'
import { detectLocale, LOCALE_STORAGE_KEY, normalizeLocale, type Locale } from './locale'
import { en, type Translation } from './resources'

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

const ready = i18n.init({
  resources: { en: { translation: en } },
  lng: 'en',
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

syncDocument('en')
i18n.on('languageChanged', (language) => {
  syncDocument(normalizeLocale(language) ?? 'en')
})

let initialization: Promise<void> | undefined

function installLocale(locale: Locale, translation: Translation) {
  if (!i18n.hasResourceBundle(locale, 'translation')) {
    i18n.addResourceBundle(locale, 'translation', translation)
  }
}

async function activateLocale(locale: Locale) {
  const translation = await loadLocale(locale)
  installLocale(locale, translation)
  await i18n.changeLanguage(locale)
}

export function initializeI18n(): Promise<void> {
  initialization ??= ready
    .then(() => activateLocale(initialLocale))
    .catch(async () => {
      await i18n.changeLanguage('en')
    })
  return initialization
}

export function currentLocale(): Locale {
  return normalizeLocale(i18n.resolvedLanguage ?? i18n.language) ?? 'en'
}

export async function setLocale(locale: Locale) {
  try {
    await activateLocale(locale)
  } catch {
    await i18n.changeLanguage('en')
    return
  }
  try {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, locale)
  } catch {
    // Browser privacy settings may disable local storage; the session still updates.
  }
}
