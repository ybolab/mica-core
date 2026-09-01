export const supportedLocales = ['en', 'zh-CN'] as const

export type Locale = (typeof supportedLocales)[number]

export const LOCALE_STORAGE_KEY = 'mos.ui.locale'

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
