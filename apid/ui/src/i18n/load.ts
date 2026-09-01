import type { Locale } from './locale'
import { en, type Translation } from './resources'

const loaders: Record<Locale, () => Promise<Translation>> = {
  en: async () => en,
  'zh-CN': async () => (await import('./zh-cn')).zhCN,
}

export function loadLocale(locale: Locale): Promise<Translation> {
  return loaders[locale]()
}
